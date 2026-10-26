//! The Reflect bond basket strategy as a Titan [`TradingVenue`].
//!
//! [`ReflectBondVenue`] maps [`BondVenue`] onto Titan's contract (async
//! two-phase fetch via [`AccountsCache`], a fetched Clock, a full min_out-baked
//! instruction). All routing/protocol logic lives in `reflect-routing-core` +
//! `reflect-sdk`; this file is the Titan adaptation only.

use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::ids::{TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use reflect_sdk::common::Now;

use reflect_routing_core::{BondVenue, MintInfo};

use reflect_titan_common::account_caching::AccountsCache;
use reflect_titan_common::trading_venue::{
    error::TradingVenueError, protocol::PoolProtocol, token_info::TokenInfo, AddressLookupTableTrait, FromAccount, QuoteRequest, QuoteResult, TradingVenue,
};

/// Slippage baked into the built instruction's `min_out`. ponytail: 0.5% floor;
/// tune (or thread from the route) once Titan's slippage model is pinned.
const SLIPPAGE_BPS: u16 = 50;

/// The Clock sysvar id (`SysvarC1ock11111111111111111111111111111111`).
const CLOCK_SYSVAR: Pubkey = solana_program::sysvar::clock::ID;

/// Map any core/SDK error into the venue error type (the detail is in the text).
fn venue_err<E: core::fmt::Debug>(e: E) -> TradingVenueError {
    TradingVenueError::AmmMethodError(format!("{e:?}").into())
}

/// [`MintInfo`] → Titan `TokenInfo` (note `u8` → `i32` decimals). A basket
/// asset that carries a Token-2022 TransferFee extension would need it parsed
/// via [`TokenInfo::new`] - done lazily only if one appears.
fn to_token_info(m: MintInfo) -> TokenInfo {
    TokenInfo { pubkey: m.pubkey, decimals: m.decimals as i32, is_token_2022: m.token_program == TOKEN_2022_PROGRAM, transfer_fee: None, maximum_fee: None }
}

/// [`BondVenue`] presented as a Titan venue.
pub struct ReflectBondVenue {
    venue: BondVenue,
    clock: Now,
    /// Advisory fetch set surfaced to the framework: seeds before load, the full
    /// discovery set after.
    keys: Vec<Pubkey>,
    /// `[receipt (idx 0), asset_0..n]` - basket order, `bounds()` uses these indices.
    tokens: Vec<TokenInfo>,
    loaded: bool,
}

impl FromAccount for ReflectBondVenue {
    fn from_account(pubkey: &Pubkey, account: &Account) -> Result<Self, TradingVenueError> {
        let venue = BondVenue::from_market(*pubkey, account.owner);
        let keys = venue.seeds();
        Ok(Self { venue, clock: Now { slot: 0, unix_timestamp: 0 }, keys, tokens: vec![], loaded: false })
    }
}

#[async_trait]
impl TradingVenue for ReflectBondVenue {
    fn initialized(&self) -> bool {
        self.loaded
    }

    fn program_id(&self) -> Pubkey {
        self.venue.program_id()
    }

    fn market_id(&self) -> Pubkey {
        self.venue.market()
    }

    fn program_dependencies(&self) -> Vec<Pubkey> {
        // A basket asset may be Token-2022 (unlike lending's SPL-only pair).
        vec![TOKEN_PROGRAM, TOKEN_2022_PROGRAM]
    }

    fn protocol(&self) -> PoolProtocol {
        PoolProtocol::ReflectBonds
    }

    fn get_token_info(&self) -> &[TokenInfo] {
        &self.tokens
    }

    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(self.keys.clone())
    }

    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError> {
        // seeds (controller, asset_registry, multiocular, token_registry) + Clock sysvar.
        let seeds = self.venue.seeds();
        let p1: Vec<Pubkey> = seeds.iter().copied().chain([CLOCK_SYSVAR]).collect();
        let base = cache.get_accounts(&p1).await?;
        let controller = base[0].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(seeds[0].into()))?;
        let asset_registry = base[1].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(seeds[1].into()))?;
        let multiocular = base[2].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(seeds[2].into()))?;
        let token_registry = base[3].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(seeds[3].into()))?;
        let clock_acc = base[4].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(CLOCK_SYSVAR.into()))?;
        self.clock = Now::from_sysvar_bytes(&clock_acc.data).map_err(venue_err)?;

        // the venue derives the full set from the seed bytes; fetch it and
        // hand it back for loading.
        let keys = self.venue.discovery_keys(&controller.data, &asset_registry.data, &multiocular.data, &token_registry.data).map_err(venue_err)?;
        let datas = cache.get_accounts(&keys).await?;
        self.venue.load(keys.iter().zip(datas).filter_map(|(k, a)| a.map(|a| (*k, a.owner, a.data)))).map_err(venue_err)?;

        self.tokens = self.venue.mints().into_iter().map(to_token_info).collect();

        // Every bond basket has assets; an empty result is a malformed load.
        if self.tokens.is_empty() {
            return Err(TradingVenueError::AmmMethodError("empty bond basket".into()));
        }

        self.keys = keys;
        self.loaded = true;
        Ok(())
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        if !self.loaded {
            return Err(TradingVenueError::NotInitialized(self.venue.market().into()));
        }
        // Zero input → clean 0 (never panics). A predicted revert (supply
        // floor/cap/partial-fill) or any quote error maps to no-liquidity -
        // exactly what bounds() and the not_enough_liquidity flag expect.
        let expected_output = match self.venue.quote(request.input_mint, request.output_mint, request.amount, self.clock) {
            Ok(Some(q)) => q.amount_out,
            _ => 0,
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
        if !self.loaded {
            return Err(TradingVenueError::NotInitialized(self.venue.market().into()));
        }

        // Titan consumes the whole Instruction (min_out baked from
        // SLIPPAGE_BPS). For the redeem direction min_out is a USD@1e6
        // net-withdraw floor (the program's unit), NOT out-token atoms -
        // Titan's own output check is the token-level guard for that leg.
        self.venue.build(user, request.input_mint, request.output_mint, request.amount, SLIPPAGE_BPS, self.clock).map_err(venue_err)
    }

    // bounds(): the default impl binary-searches our monotonic, zero-input-safe quote().
}

#[async_trait]
impl AddressLookupTableTrait for ReflectBondVenue {
    async fn get_lookup_table_keys(&self, _accounts_cache: Option<&dyn AccountsCache>) -> Result<Vec<Pubkey>, TradingVenueError> {
        // No bond lookup table is published yet (POOL_REGISTRY_LOOKUP_TABLE in
        // reflect-sdk's ids.rs is lending-specific); add the constant here when
        // sdk/ts publishes one.
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // u8 → i32 decimals and the token-2022 flag, the two casts `to_token_info` owns.
    #[test]
    fn to_token_info_casts_decimals_and_flags_2022() {
        let spl = to_token_info(MintInfo { pubkey: Pubkey::new_unique(), token_program: TOKEN_PROGRAM, decimals: 6 });
        assert_eq!(spl.decimals, 6_i32);
        assert!(!spl.is_token_2022);

        let t22 = to_token_info(MintInfo { pubkey: Pubkey::new_unique(), token_program: TOKEN_2022_PROGRAM, decimals: 9 });
        assert_eq!(t22.decimals, 9_i32);
        assert!(t22.is_token_2022);
    }

    fn keyed_account(owner: Pubkey) -> Account {
        Account { lamports: 1, data: vec![0u8; 8], owner, executable: false, rent_epoch: 0 }
    }

    // A cold venue is uninitialized, advertises no tokens, and every call that
    // needs loaded state errors NotInitialized rather than panicking.
    #[test]
    fn cold_venue_is_uninitialized_and_quote_errors() {
        let owner = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let v = ReflectBondVenue::from_account(&market, &keyed_account(owner)).unwrap();

        assert!(!v.initialized());
        assert!(v.get_token_info().is_empty());
        assert_eq!(v.get_required_pubkeys_for_update().unwrap().len(), 4); // controller + 3 bond seeds

        let req =
            QuoteRequest { input_mint: Pubkey::new_unique(), output_mint: Pubkey::new_unique(), amount: 1, swap_type: reflect_titan_common::trading_venue::SwapType::ExactIn };
        assert!(matches!(v.quote(req).unwrap_err(), TradingVenueError::NotInitialized(_)));
    }
}
