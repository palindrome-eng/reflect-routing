//! The Reflect lending strategy as a Titan [`TradingVenue`].
//!
//! [`ReflectLendingVenue`] maps [`LendingVenue`] onto Titan's contract (async
//! two-phase fetch via [`AccountsCache`], a fetched Clock, a full min_out-baked
//! instruction). All routing/protocol logic lives in `reflect-routing-core` +
//! `reflect-sdk`; this file is the Titan adaptation only.

use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::ids::{TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use reflect_sdk::common::Clock;
use reflect_sdk::strategy::ids::POOL_REGISTRY_LOOKUP_TABLE;

use reflect_routing_core::{AccountBytes, LendingVenue, RouteMint};

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

/// Map any core/SDK error into the venue error type (the detail is in the text).
fn venue_err<E: core::fmt::Debug>(e: E) -> TradingVenueError {
    TradingVenueError::AmmMethodError(format!("{e:?}").into())
}

/// [`RouteMint`] → Titan `TokenInfo` (note `u8` → `i32` decimals). USDC/USDC+/USDT
/// are plain SPL; a Token-2022 mint that trades would need its TransferFee
/// extension parsed via [`TokenInfo::new`] - done lazily only if one appears.
fn to_token_info(m: RouteMint) -> TokenInfo {
    TokenInfo { pubkey: m.pubkey, decimals: m.decimals as i32, is_token_2022: m.token_program == TOKEN_2022_PROGRAM, transfer_fee: None, maximum_fee: None }
}

/// `AccountsCache` results → core lookup. Pairs a fetched slice with the keys it
/// was fetched for (the cache contract is order-preserving, miss → None).
struct CacheBytes<'a>(&'a [Option<Account>], &'a [Pubkey]);
impl AccountBytes for CacheBytes<'_> {
    fn get(&self, k: &Pubkey) -> Option<(Pubkey, Vec<u8>)> {
        self.1.iter().position(|key| key == k).and_then(|i| self.0.get(i)).and_then(|o| o.as_ref()).map(|a| (a.owner, a.data.clone()))
    }
}

/// [`LendingVenue`] presented as a Titan venue.
pub struct ReflectLendingVenue {
    venue: LendingVenue,
    clock: Clock,
    /// Advisory fetch set surfaced to the framework: seeds before load, the full
    /// discovery set after.
    keys: Vec<Pubkey>,
    /// `[deposit (idx 0), receipt (idx 1)]` - `bounds()` uses these indices.
    tokens: Vec<TokenInfo>,
    loaded: bool,
}

impl FromAccount for ReflectLendingVenue {
    fn from_account(pubkey: &Pubkey, account: &Account) -> Result<Self, TradingVenueError> {
        let venue = LendingVenue::from_market(*pubkey, account.owner);
        let keys = venue.seeds();
        Ok(Self { venue, clock: Clock { slot: 0, unix: 0 }, keys, tokens: vec![], loaded: false })
    }
}

#[async_trait]
impl TradingVenue for ReflectLendingVenue {
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
        vec![TOKEN_PROGRAM]
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
        // Phase 1: seeds + Clock sysvar (the quote timestamp).
        let seeds = self.venue.seeds();
        let p1: Vec<Pubkey> = seeds.iter().copied().chain([CLOCK_SYSVAR]).collect();
        let base = cache.get_accounts(&p1).await?;
        let clock_acc = base[seeds.len()].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(CLOCK_SYSVAR.into()))?;
        self.clock = Clock::from_sysvar_bytes(&clock_acc.data).map_err(venue_err)?;

        // Phase 2: the venue derives the full set from the seed bytes; fetch it and
        // hand it back for loading.
        let cold = CacheBytes(&base, &p1);
        let keys = self.venue.discovery_keys(&cold).ok_or_else(|| TradingVenueError::NoAccountFound(seeds[0].into()))?;
        let datas = cache.get_accounts(&keys).await?;
        self.venue.load(&CacheBytes(&datas, &keys)).map_err(venue_err)?;

        self.keys = keys;
        self.tokens = self.venue.mints().into_iter().map(to_token_info).collect();
        self.loaded = true;
        Ok(())
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        // Zero input → clean 0 (never panics). A predicted revert (supply
        // floor/cap) or any quote error maps to no-liquidity - exactly what
        // bounds() and the not_enough_liquidity flag expect.
        let expected_output = match self.venue.quote(request.input_mint, request.amount, self.clock) {
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
        // Titan consumes the whole Instruction (min_out baked from SLIPPAGE_BPS +
        // HEADROOM_UNITS), unlike Jupiter. extra_signers (Loopscale cosigner) is the
        // known execution blocker → the agreed reserve-only path; not resolvable here.
        self.venue.build(user, request.input_mint, request.amount, SLIPPAGE_BPS, HEADROOM_UNITS, self.clock).map_err(venue_err)
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
        let spl = to_token_info(RouteMint { pubkey: Pubkey::new_unique(), token_program: TOKEN_PROGRAM, decimals: 6 });
        assert_eq!(spl.decimals, 6_i32);
        assert!(!spl.is_token_2022);

        let t22 = to_token_info(RouteMint { pubkey: Pubkey::new_unique(), token_program: TOKEN_2022_PROGRAM, decimals: 9 });
        assert_eq!(t22.decimals, 9_i32);
        assert!(t22.is_token_2022);
    }
}
