//! The Reflect lending strategy as a Titan [`TradingVenue`].
//!
//! Two mints, two directions: deposit→receipt (`mint`) and receipt→deposit
//! (`redeem`). Everything protocol-specific is delegated to `reflect-sdk`; this
//! file is account plumbing plus the one `common::dir` dispatch. The five SDK
//! calls are: `discovery_keys` → `from_controller` → `tradable_pair` →
//! `prepare_quote` → `quote_mint`/`quote_redeem` (and `mint`/`redeem` to build).

use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::ids::TOKEN_2022_PROGRAM;
use reflect_sdk::common::Clock;
use reflect_sdk::strategy::ids::POOL_REGISTRY_LOOKUP_TABLE;
use reflect_sdk::strategy::{lending_discovery_keys, pda, MintInfo, StrategyAccounts};

use crate::account_caching::AccountsCache;
use crate::trading_venue::{
    error::TradingVenueError, protocol::PoolProtocol, token_info::TokenInfo, AddressLookupTableTrait, FromAccount, QuoteRequest, QuoteResult, TradingVenue,
};

/// Slippage baked into the built instruction's `min_out`. ponytail: 0.5% floor;
/// tune (or thread from the route) once Titan's slippage model is pinned.
const SLIPPAGE_BPS: u16 = 50;
/// Units subtracted from a redeem's `min_out` to absorb venue-unwind rounding
/// dust (the redeem quote is a tight UPPER bound). ponytail: 5 units.
const HEADROOM_UNITS: u64 = 5;

/// The Clock sysvar id (`SysvarC1ock11111111111111111111111111111111`).
const CLOCK_SYSVAR: Pubkey = solana_program::sysvar::clock::ID;

/// Map any SDK error into the venue error type (the SDK's `ActionError` /
/// `CoreError` carry the detail; we surface it as text).
fn venue_err<E: core::fmt::Debug>(e: E) -> TradingVenueError {
    TradingVenueError::AmmMethodError(format!("{e:?}").into())
}

/// `MintInfo` → Titan `TokenInfo` (note `u8` → `i32` decimals). USDC/USDC+/USDT
/// are plain SPL; a Token-2022 mint that trades would need its TransferFee
/// extension parsed via [`TokenInfo::new`] — done lazily only if one appears.
fn to_token_info(m: MintInfo) -> TokenInfo {
    TokenInfo { pubkey: m.pubkey, decimals: m.decimals as i32, is_token_2022: m.token_program == TOKEN_2022_PROGRAM, transfer_fee: None, maximum_fee: None }
}


/// A Reflect lending strategy bound to one controller (= one market = one mint
/// pair). Caches the fetched account bytes; `prepare_quote` rebuilds the venue
/// view per `quote()` (the same per-call work Jupiter does).
pub struct ReflectLendingVenue {
    program_id: Pubkey,
    controller_key: Pubkey,
    deposit_mint: Pubkey,
    /// Kept for the `lending()` arm in quote/build.
    deposit_token_program: Pubkey,
    /// `[deposit (idx 0), receipt (idx 1)]` — `bounds()` uses these indices.
    tokens: Vec<TokenInfo>,
    /// Discovery set: seeds before first load, full set after `update_state`.
    keys: Vec<Pubkey>,
    clock: Clock,
    /// Owns the fetched bytes; `None` until `update_state` succeeds.
    loaded: Option<StrategyAccounts>,
}

impl FromAccount for ReflectLendingVenue {
    fn from_account(pubkey: &Pubkey, account: &Account) -> Result<Self, TradingVenueError> {
        // We already hold the controller key (= `pubkey`), so the seed set needs
        // no strategy index: [controller, pool_registry]. The index is read from
        // the controller bytes later, inside the SDK (lending_discovery_keys /
        // from_controller).
        let program_id = account.owner;
        Ok(Self {
            program_id,
            controller_key: *pubkey,
            deposit_mint: Pubkey::default(),
            deposit_token_program: Pubkey::default(),
            tokens: vec![],
            keys: vec![*pubkey, pda::derive_pool_registry(&program_id).0],
            clock: Clock { slot: 0, unix: 0 },
            loaded: None,
        })
    }
}

#[async_trait]
impl TradingVenue for ReflectLendingVenue {
    fn initialized(&self) -> bool {
        self.loaded.is_some()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn market_id(&self) -> Pubkey {
        self.controller_key
    }

    fn program_dependencies(&self) -> Vec<Pubkey> {
        vec![reflect_sdk::common::ids::TOKEN_PROGRAM]
    }

    fn protocol(&self) -> PoolProtocol {
        PoolProtocol::ReflectLending
    }

    fn get_token_info(&self) -> &[TokenInfo] {
        &self.tokens
    }

    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(self.keys.clone())
    }

    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError> {
        // Phase 1: controller + registry (+ Clock sysvar for the quote timestamp).
        let (ck, rk) = (self.controller_key, pda::derive_pool_registry(&self.program_id).0);
        let base = cache.get_accounts(&[ck, rk, CLOCK_SYSVAR]).await?;
        let controller = base[0].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(ck.into()))?;
        let registry = base[1].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(rk.into()))?;
        let clock_acc = base[2].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(CLOCK_SYSVAR.into()))?;
        self.clock = Clock::from_sysvar_bytes(&clock_acc.data).map_err(venue_err)?;

        // Phase 2: the full discovery set → straight into the SDK bridge.
        self.keys = lending_discovery_keys(&controller.data, &registry.data, &self.program_id).map_err(venue_err)?;
        let datas = cache.get_accounts(&self.keys).await?;
        let accts = StrategyAccounts::from_account_map(
            &self.program_id,
            &self.controller_key,
            self.keys.iter().zip(datas).filter_map(|(k, a)| a.map(|a| (*k, a.owner, a.data))),
        )
        .map_err(venue_err)?;
        let pair = accts.tradable_pair().map_err(venue_err)?;
        self.deposit_mint = pair.deposit.pubkey;
        self.deposit_token_program = pair.deposit.token_program;
        self.tokens = vec![to_token_info(pair.deposit), to_token_info(pair.receipt)];
        self.loaded = Some(accts);
        Ok(())
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        let accts = self.loaded.as_ref().ok_or_else(|| TradingVenueError::NotInitialized(self.controller_key.into()))?;

        // Zero input is a documented hard requirement: clean 0, never panic or error.
        // The SDK owns the mint→op dispatch (quote_by_mints); a would-revert
        // (supply floor/cap) or any quote error maps to no-liquidity — exactly
        // what bounds() and the not_enough_liquidity flag expect.
        let expected_output = if request.amount == 0 {
            0
        } else {
            match accts.quote_by_mints(request.input_mint, request.amount, self.clock) {
                Ok(a) if a.would_revert.is_none() => a.amount_out,
                _ => 0,
            }
        };

        Ok(QuoteResult {
            input_mint: request.input_mint,
            output_mint: request.output_mint,
            amount: request.amount,
            expected_output,
            not_enough_liquidity: expected_output == 0 && request.amount > 0,
        })
    }

    fn generate_swap_instruction(&self, request: QuoteRequest, user: Pubkey) -> Result<Instruction, TradingVenueError> {
        let accts = self.loaded.as_ref().ok_or_else(|| TradingVenueError::NotInitialized(self.controller_key.into()))?;
        let h = accts.lending(user, self.deposit_mint, self.deposit_token_program).map_err(venue_err)?;

        // Titan consumes the whole Instruction (min_out baked), unlike Jupiter.
        // extra_signers (Loopscale cosigner) is the known execution blocker → the
        // agreed reserve-only path; not resolvable in the adapter.
        let (_quote, built) = if request.input_mint == self.deposit_mint {
            h.mint(request.amount, SLIPPAGE_BPS, self.clock).map_err(venue_err)?
        } else {
            h.redeem(request.amount, SLIPPAGE_BPS, HEADROOM_UNITS, self.clock).map_err(venue_err)?
        };
        Ok(built.instruction)
    }

    // bounds(): the default impl binary-searches our monotonic, zero-input-safe quote().
}

#[async_trait]
impl AddressLookupTableTrait for ReflectLendingVenue {
    async fn get_lookup_table_keys(&self, _accounts_cache: Option<&dyn AccountsCache>) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(vec![POOL_REGISTRY_LOOKUP_TABLE])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // u8 → i32 decimals and the token-2022 flag, the two casts `to_token_info` owns.
    #[test]
    fn to_token_info_casts_decimals_and_flags_2022() {
        let spl = to_token_info(MintInfo { pubkey: Pubkey::new_unique(), token_program: reflect_sdk::common::ids::TOKEN_PROGRAM, decimals: 6 });
        assert_eq!(spl.decimals, 6_i32);
        assert!(!spl.is_token_2022);

        let t22 = to_token_info(MintInfo { pubkey: Pubkey::new_unique(), token_program: TOKEN_2022_PROGRAM, decimals: 9 });
        assert_eq!(t22.decimals, 9_i32);
        assert!(t22.is_token_2022);
    }
}
