//! The Reflect whitelabel branded pair as a Titan [`TradingVenue`].
//!
//! [`ReflectWhitelabelVenue`] maps [`WhitelabelVenue`] onto Titan's contract
//! (async two-phase fetch via [`AccountsCache`], a fetched Clock, a full
//! min_out-baked instruction).

use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::ids::{TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use reflect_sdk::common::Clock;

use reflect_routing_core::{MintInfo, WhitelabelVenue};

use reflect_titan_common::account_caching::AccountsCache;
use reflect_titan_common::trading_venue::{
    error::TradingVenueError, protocol::PoolProtocol, token_info::TokenInfo, AddressLookupTableTrait, FromAccount, QuoteRequest, QuoteResult, TradingVenue,
};

/// Slippage baked into the built instruction's `min_out`. 0.5% floor;
/// tune (or thread from the route) once Titan's slippage model is pinned.
const SLIPPAGE_BPS: u16 = 50;

/// The Clock sysvar id (`SysvarC1ock11111111111111111111111111111111`).
const CLOCK_SYSVAR: Pubkey = solana_program::sysvar::clock::ID;

/// Map any core/SDK error into the venue error type (the detail is in the text).
fn venue_err<E: core::fmt::Debug>(e: E) -> TradingVenueError {
    TradingVenueError::AmmMethodError(format!("{e:?}").into())
}

/// [`MintInfo`] → Titan `TokenInfo` (note `u8` → `i32` decimals). This product
/// is SPL-only (no Token-2022 branded/stablecoin pair observed on-chain).
fn to_token_info(m: MintInfo) -> TokenInfo {
    TokenInfo { pubkey: m.pubkey, decimals: m.decimals as i32, is_token_2022: m.token_program == TOKEN_2022_PROGRAM, transfer_fee: None, maximum_fee: None }
}

/// [`WhitelabelVenue`] presented as a Titan venue.
pub struct ReflectWhitelabelVenue {
    venue: WhitelabelVenue,
    clock: Clock,
    /// Advisory fetch set surfaced to the framework: the seed before load,
    /// the full discovery set after.
    keys: Vec<Pubkey>,
    /// `[stablecoin (idx 0), branded (idx 1)]` - `bounds()` uses these indices.
    tokens: Vec<TokenInfo>,
    loaded: bool,
}

impl FromAccount for ReflectWhitelabelVenue {
    fn from_account(pubkey: &Pubkey, _account: &Account) -> Result<Self, TradingVenueError> {
        // No owner param: this product is single-deployment
        // (`WhitelabelVenue::load` verifies the account is proxy-owned).
        let venue = WhitelabelVenue::from_market(*pubkey);
        let keys = venue.seeds();
        Ok(Self { venue, clock: Clock { slot: 0, unix: 0 }, keys, tokens: vec![], loaded: false })
    }
}

#[async_trait]
impl TradingVenue for ReflectWhitelabelVenue {
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
        PoolProtocol::ReflectWhitelabel
    }

    fn get_token_info(&self) -> &[TokenInfo] {
        &self.tokens
    }

    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(self.keys.clone())
    }

    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError> {
        // one seed (the ProxyState itself) + Clock sysvar (the quote timestamp).
        let seeds = self.venue.seeds();
        let p1: Vec<Pubkey> = seeds.iter().copied().chain([CLOCK_SYSVAR]).collect();
        let base = cache.get_accounts(&p1).await?;
        let market = base[0].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(seeds[0].into()))?;
        let clock_acc = base[1].as_ref().ok_or_else(|| TradingVenueError::NoAccountFound(CLOCK_SYSVAR.into()))?;
        self.clock = Clock::from_sysvar_bytes(&clock_acc.data).map_err(venue_err)?;

        // the venue derives the full set from the ProxyState bytes; fetch it
        // and hand it back for loading.
        let keys = self.venue.discovery_keys(&market.data).map_err(venue_err)?;
        let datas = cache.get_accounts(&keys).await?;
        self.venue.load(keys.iter().zip(datas).filter_map(|(k, a)| a.map(|a| (*k, a.owner, a.data)))).map_err(venue_err)?;

        self.tokens = self.venue.mints().into_iter().map(to_token_info).collect();

        // A proxy market always has both mints; empty is a malformed load.
        if self.tokens.is_empty() {
            return Err(TradingVenueError::AmmMethodError("proxy market has no mint pair".into()));
        }

        self.keys = keys;
        self.loaded = true;
        Ok(())
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        if !self.loaded {
            return Err(TradingVenueError::NotInitialized(self.venue.market().into()));
        }

        // Zero input → clean 0.
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
        if !self.loaded {
            return Err(TradingVenueError::NotInitialized(self.venue.market().into()));
        }

        self.venue.build(user, request.input_mint, request.amount, SLIPPAGE_BPS, self.clock).map_err(venue_err)
    }

    // bounds(): the default impl binary-searches our monotonic, zero-input-safe quote().
}

#[async_trait]
impl AddressLookupTableTrait for ReflectWhitelabelVenue {
    async fn get_lookup_table_keys(&self, _accounts_cache: Option<&dyn AccountsCache>) -> Result<Vec<Pubkey>, TradingVenueError> {
        // No whitelabel lookup table published yet.
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
        let market = Pubkey::new_unique();
        let v = ReflectWhitelabelVenue::from_account(&market, &keyed_account(Pubkey::new_unique())).unwrap();

        assert!(!v.initialized());
        assert!(v.get_token_info().is_empty());
        assert_eq!(v.get_required_pubkeys_for_update().unwrap().len(), 1); // the single seed

        let req =
            QuoteRequest { input_mint: Pubkey::new_unique(), output_mint: Pubkey::new_unique(), amount: 1, swap_type: reflect_titan_common::trading_venue::SwapType::ExactIn };
        assert!(matches!(v.quote(req).unwrap_err(), TradingVenueError::NotInitialized(_)));
    }
}
