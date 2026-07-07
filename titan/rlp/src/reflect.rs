//! The Reflect rlp insurance pool as a Titan [`TradingVenue`].
//!
//! [`ReflectRlpVenue`] maps [`RlpVenue`] onto Titan's contract (async
//! three-phase fetch via [`AccountsCache`] - seeds, then the discovery set,
//! then load - a fetched Clock, a full min_out-baked instruction). All
//! routing/protocol logic lives in `reflect-routing-core` + `reflect-sdk`;
//! this file is the Titan adaptation only.

use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::ids::TOKEN_PROGRAM;
use reflect_sdk::common::Clock;

use reflect_routing_core::{MintInfo, RlpVenue};

use crate::account_caching::AccountsCache;
use crate::trading_venue::{
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

/// [`MintInfo`] → Titan `TokenInfo` (note `u8` → `i32` decimals). This
/// product is SPL-only (rlp's restake/swap CPI only into the classic SPL
/// Token program).
fn to_token_info(m: MintInfo) -> TokenInfo {
    TokenInfo { pubkey: m.pubkey, decimals: m.decimals as i32, is_token_2022: false, transfer_fee: None, maximum_fee: None }
}

/// [`RlpVenue`] presented as a Titan venue.
pub struct ReflectRlpVenue {
    venue: RlpVenue,
    clock: Clock,
    /// Advisory fetch set surfaced to the framework: seeds before load, the
    /// full discovery set after.
    keys: Vec<Pubkey>,
    /// `[lp (idx 0), public asset_0..n]` - `bounds()` uses these indices.
    tokens: Vec<TokenInfo>,
    loaded: bool,
}

impl ReflectRlpVenue {
    /// The integration entry point: Titan's registry supplies the market key
    /// AND the registered asset mints (see [`RlpVenue`] docs for the
    /// one-call gPA recipe - `Settings` stores only an asset COUNT, never
    /// the mints, so they cannot be discovered from the pool account alone).
    /// No owner check here - `RlpVenue::load` enforces ownership.
    pub fn new(pool: Pubkey, asset_mints: Vec<Pubkey>) -> Self {
        let venue = RlpVenue::from_market(pool, asset_mints);
        let keys = venue.seeds();
        Self { venue, clock: Clock { slot: 0, unix: 0 }, keys, tokens: vec![], loaded: false }
    }
}

impl FromAccount for ReflectRlpVenue {
    fn from_account(_: &Pubkey, _: &Account) -> Result<Self, TradingVenueError> {
        Err(TradingVenueError::AmmMethodError("rlp venue needs the registered asset-mint set; construct via ReflectRlpVenue::new".into()))
    }
}

#[async_trait]
impl TradingVenue for ReflectRlpVenue {
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
        PoolProtocol::ReflectRlp
    }

    fn get_token_info(&self) -> &[TokenInfo] {
        &self.tokens
    }

    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(self.keys.clone())
    }

    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError> {
        // Round 1: seeds + clock, one batch. Seed order is the venue
        // contract: [pool, settings, asset_pda_0..n-1, mint_0..n-1].
        let seeds = self.venue.seeds();
        let mut p1 = seeds.clone();
        p1.push(CLOCK_SYSVAR);
        let fetched = cache.get_accounts(&p1).await?;
        let mut accs = Vec::with_capacity(fetched.len());
        for (k, a) in p1.iter().zip(fetched) {
            accs.push(a.ok_or_else(|| TradingVenueError::NoAccountFound((*k).into()))?);
        }
        let clock_acc = accs.pop().expect("clock pushed above");
        self.clock = Clock::from_sysvar_bytes(&clock_acc.data).map_err(venue_err)?;

        // Round 2: the Asset PDA bytes sit at seed slots 2..2+n.
        let n = self.venue.asset_mints().len();
        let asset_bytes: Vec<&[u8]> = accs[2..2 + n].iter().map(|a| a.data.as_slice()).collect();
        let keys = self.venue.discovery_keys(&accs[0].data, &accs[1].data, &asset_bytes).map_err(venue_err)?;
        let datas = cache.get_accounts(&keys).await?;

        // Round 3: load (missing non-seed accounts tolerated by filter_map;
        // load() itself errors on anything it actually needs).
        self.venue.load(keys.iter().zip(datas).filter_map(|(k, a)| a.map(|a| (*k, a.owner, a.data)))).map_err(venue_err)?;

        self.tokens = self.venue.mints().into_iter().map(to_token_info).collect();
        if self.tokens.len() < 2 {
            return Err(TradingVenueError::AmmMethodError("no public routable assets".into()));
        }

        self.keys = keys;
        self.loaded = true;
        Ok(())
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        if !self.loaded {
            return Err(TradingVenueError::NotInitialized(self.venue.market().into()));
        }
        // Zero input → clean 0 (never panics). A predicted revert (killswitch,
        // stale oracle, insufficient swap inventory) or any quote error maps
        // to no-liquidity - exactly what bounds() and not_enough_liquidity expect.
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
        // Titan consumes the whole Instruction (min_out baked from SLIPPAGE_BPS).
        self.venue.build(user, request.input_mint, request.output_mint, request.amount, SLIPPAGE_BPS, self.clock).map_err(venue_err)
    }

    // bounds(): the default impl binary-searches our monotonic, zero-input-safe
    // quote(). On the LP-input direction it correctly finds no valid quote and
    // returns Err(NoQuotableValue) - LP -> asset is the cooldown withdrawal.
}

#[async_trait]
impl AddressLookupTableTrait for ReflectRlpVenue {
    async fn get_lookup_table_keys(&self, _accounts_cache: Option<&dyn AccountsCache>) -> Result<Vec<Pubkey>, TradingVenueError> {
        // No rlp lookup table published yet.
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // u8 → i32 decimals cast; this product is never Token-2022.
    #[test]
    fn to_token_info_casts_decimals() {
        let info = to_token_info(MintInfo { pubkey: Pubkey::new_unique(), token_program: TOKEN_PROGRAM, decimals: 6 });
        assert_eq!(info.decimals, 6_i32);
        assert!(!info.is_token_2022);
    }

    #[test]
    fn from_account_requires_new_constructor() {
        let account = Account { lamports: 1, data: vec![0u8; 8], owner: Pubkey::new_unique(), executable: false, rent_epoch: 0 };
        assert!(ReflectRlpVenue::from_account(&Pubkey::new_unique(), &account).is_err());
    }

    // A cold venue is uninitialized, advertises no tokens, and every call that
    // needs loaded state errors NotInitialized rather than panicking.
    #[test]
    fn cold_venue_is_uninitialized_and_quote_errors() {
        let v = ReflectRlpVenue::new(Pubkey::new_unique(), vec![Pubkey::new_unique(), Pubkey::new_unique()]);

        assert!(!v.initialized());
        assert!(v.get_token_info().is_empty());
        assert_eq!(v.get_required_pubkeys_for_update().unwrap().len(), 6); // pool + settings + 2 asset pdas + 2 mints

        let req = QuoteRequest { input_mint: Pubkey::new_unique(), output_mint: Pubkey::new_unique(), amount: 1, swap_type: crate::trading_venue::SwapType::ExactIn };
        assert!(matches!(v.quote(req).unwrap_err(), TradingVenueError::NotInitialized(_)));
    }
}
