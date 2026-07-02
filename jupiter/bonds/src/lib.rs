//! `reflect-jupiter-bonds` - the Reflect bond basket strategy as a Jupiter `Amm`.
//!
//! The `Amm` trait + `Swap` tag are vendored (see [`swap`]) to stay on solana
//! 2.2 alongside `reflect-sdk`. This crate is a thin shim: [`ReflectBondsAmm`]
//! maps [`BondVenue`] onto Jupiter's contract (a 4-seed two-phase account
//! handshake, atomic clock, metas-only swap build). All routing/protocol logic
//! lives in `reflect-routing-core` + `reflect-sdk`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use solana_sdk::account::Account;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::instruction::AccountMeta;
use solana_sdk::pubkey::Pubkey;

use reflect_routing_core::{BondRoute, BondVenue};
use reflect_sdk::common::Clock as ReflectClock;

mod swap;
pub use swap::{AccountsType, RemainingAccountsInfo, RemainingAccountsSlice, Side, Swap};

/// ExactIn / ExactOut. The SDK's bond math is ExactIn only; the adapter
/// declares `supports_exact_out() == false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SwapMode {
    #[default]
    ExactIn,
    ExactOut,
}

#[derive(Debug)]
pub struct QuoteParams {
    pub amount: u64,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub swap_mode: SwapMode,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Quote {
    pub in_amount: u64,
    pub out_amount: u64,
    pub fee_amount: u64,
    pub fee_mint: Pubkey,
}

pub type QuoteMintToReferrer = HashMap<Pubkey, Pubkey, ahash::RandomState>;
pub type AccountMap = HashMap<Pubkey, Account, ahash::RandomState>;

pub struct SwapParams<'a, 'b> {
    pub swap_mode: SwapMode,
    pub in_amount: u64,
    pub out_amount: u64,
    pub source_mint: Pubkey,
    pub destination_mint: Pubkey,
    pub source_token_account: Pubkey,
    pub destination_token_account: Pubkey,
    /// The user or the program authority over the source token account.
    pub token_transfer_authority: Pubkey,
    pub quote_mint_to_referrer: Option<&'a QuoteMintToReferrer>,
    pub jupiter_program_id: &'b Pubkey,
    /// Replace missing dynamic accounts with the default Pubkey instead of erroring.
    pub missing_dynamic_accounts_as_default: bool,
}

pub struct SwapAndAccountMetas {
    pub swap: Swap,
    pub account_metas: Vec<AccountMeta>,
}

#[derive(Clone)]
pub struct KeyedAccount {
    pub key: Pubkey,
    pub account: Account,
}

pub struct AmmContext {
    pub clock_ref: ClockRef,
}

/// The Jupiter `Amm` trait (vendored from `jupiter-amm-interface` v0.6.1).
pub trait Amm {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self>
    where
        Self: Sized;
    fn label(&self) -> String;
    fn program_id(&self) -> Pubkey;
    /// The pool/market state address.
    fn key(&self) -> Pubkey;
    /// The mints that can be traded.
    fn get_reserve_mints(&self) -> Vec<Pubkey>;
    /// The accounts necessary to produce a quote.
    fn get_accounts_to_update(&self) -> Vec<Pubkey>;
    /// Ingest fetched account data; heavy deserialization/precompute goes here.
    fn update(&mut self, account_map: &AccountMap) -> Result<()>;

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote>;

    /// The `Swap` tag + the ordered account metas for the swap.
    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas>;

    fn has_dynamic_accounts(&self) -> bool {
        false
    }

    fn requires_update_for_reserve_mints(&self) -> bool {
        false
    }

    fn supports_exact_out(&self) -> bool {
        false
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync>;

    fn unidirectional(&self) -> bool {
        false
    }

    fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
        vec![]
    }

    fn get_accounts_len(&self) -> usize {
        32
    }

    fn underlying_liquidities(&self) -> Option<HashSet<Pubkey>> {
        None
    }

    fn is_active(&self) -> bool {
        true
    }
}

impl Clone for Box<dyn Amm + Send + Sync> {
    fn clone(&self) -> Box<dyn Amm + Send + Sync> {
        self.clone_amm()
    }
}

/// Atomic clock reference shared with the host; the adapter reads slot/unix at
/// quote time.
#[derive(Default, Clone)]
pub struct ClockRef {
    pub slot: Arc<AtomicU64>,
    pub epoch_start_timestamp: Arc<AtomicI64>,
    pub epoch: Arc<AtomicU64>,
    pub leader_schedule_epoch: Arc<AtomicU64>,
    pub unix_timestamp: Arc<AtomicI64>,
}

impl ClockRef {
    pub fn update(&self, clock: SolanaClock) {
        self.epoch.store(clock.epoch, Ordering::Relaxed);
        self.slot.store(clock.slot, Ordering::Relaxed);
        self.unix_timestamp.store(clock.unix_timestamp, Ordering::Relaxed);
        self.epoch_start_timestamp.store(clock.epoch_start_timestamp, Ordering::Relaxed);
        self.leader_schedule_epoch.store(clock.leader_schedule_epoch, Ordering::Relaxed);
    }
}

impl From<SolanaClock> for ClockRef {
    fn from(clock: SolanaClock) -> Self {
        ClockRef {
            epoch: Arc::new(AtomicU64::new(clock.epoch)),
            epoch_start_timestamp: Arc::new(AtomicI64::new(clock.epoch_start_timestamp)),
            leader_schedule_epoch: Arc::new(AtomicU64::new(clock.leader_schedule_epoch)),
            slot: Arc::new(AtomicU64::new(clock.slot)),
            unix_timestamp: Arc::new(AtomicI64::new(clock.unix_timestamp)),
        }
    }
}

/// [`BondVenue`] presented as a Jupiter market. Bonds need 4 seed accounts
/// (controller, asset_registry, multiocular, main - [`BondVenue::seeds`]
/// order), unlike lending's 2, so the cached seed bytes are an array, not a
/// pair of `Option`s.
#[derive(Clone)]
pub struct ReflectBondsAmm {
    venue: BondVenue,
    clock_ref: ClockRef,
    seed_data: [Option<Vec<u8>>; 4],
}

impl ReflectBondsAmm {
    fn clock(&self) -> ReflectClock {
        ReflectClock { slot: self.clock_ref.slot.load(Ordering::Relaxed), unix: self.clock_ref.unix_timestamp.load(Ordering::Relaxed) }
    }
}

impl Amm for ReflectBondsAmm {
    fn from_keyed_account(ka: &KeyedAccount, ctx: &AmmContext) -> Result<Self> {
        Ok(Self { venue: BondVenue::from_market(ka.key, ka.account.owner), clock_ref: ctx.clock_ref.clone(), seed_data: [Some(ka.account.data.clone()), None, None, None] })
    }

    fn label(&self) -> String {
        "Reflect".to_string()
    }

    fn program_id(&self) -> Pubkey {
        self.venue.program_id()
    }

    fn key(&self) -> Pubkey {
        self.venue.market()
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        // receipt + every basket asset; any listed pair is quotable.
        self.venue.mints().iter().map(|m| m.pubkey).collect()
    }

    fn requires_update_for_reserve_mints(&self) -> bool {
        true // the mints are known only after update()
    }

    fn has_dynamic_accounts(&self) -> bool {
        true // cold start returns seeds; re-called after update for the full set
    }

    /// Two-phase handshake: cold returns the seeds; once `update` has cached their
    /// bytes, the venue derives the full set. On derive failure fall back to the
    /// seeds (re-cold) rather than panic.
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let seeds = self.venue.seeds();
        match &self.seed_data {
            [Some(c), Some(ar), Some(mo), Some(m)] => self.venue.discovery_keys(c, ar, mo, m).unwrap_or_else(|_| seeds.clone()),
            _ => seeds, // cold: [controller, asset_registry, multiocular, main]
        }
    }

    fn update(&mut self, m: &AccountMap) -> Result<()> {
        let seeds = self.venue.seeds();

        for (slot, seed) in self.seed_data.iter_mut().zip(&seeds) {
            *slot = m.get(seed).map(|a| a.data.clone());
        }
        let [Some(c), Some(ar), Some(mo), Some(main)] = &self.seed_data else {
            return Ok(()); // cold: seeds not in the map yet
        };

        // A present-but-undecodable seed is an error, not "still cold".
        let keys = self.venue.discovery_keys(c, ar, mo, main)?;
        self.venue.load(keys.iter().filter_map(|k| m.get(k).map(|a| (*k, a.owner, a.data.clone()))))
    }

    fn quote(&self, p: &QuoteParams) -> Result<Quote> {
        match self.venue.quote(p.input_mint, p.output_mint, p.amount, self.clock())? {
            // No bond fee is input-denominated (mint fee is receipt tokens,
            // swap fee is USD@1e6, redeem folds fees into outputs) - a
            // wrong-unit fee would mislead Jupiter's router more than zero.
            Some(q) => Ok(Quote { in_amount: p.amount, out_amount: q.amount_out, fee_amount: 0, fee_mint: p.input_mint }),
            None => bail!("reflect would revert: no route > bad route"),
        }
    }

    fn get_swap_and_account_metas(&self, sp: &SwapParams) -> Result<SwapAndAccountMetas> {
        // Jupiter takes ONLY the account list: its on-chain program writes the
        // instruction data (amount, min_out) from the route context, so build
        // with slippage 0 - the serialized data is discarded.
        let route = self.venue.route(sp.source_mint, sp.destination_mint)?;
        let ix = self.venue.build(sp.token_transfer_authority, sp.source_mint, sp.destination_mint, sp.in_amount, 0, self.clock())?;

        // Placeholder tags (see swap.rs) - the on-chain handler reads the
        // direction from the resolved account ORDER once real tags land.
        let swap = match route {
            BondRoute::Mint { .. } => Swap::ReflectBondsMint,
            BondRoute::Swap { .. } => Swap::ReflectBondsSwap,
            BondRoute::RedeemByReceipt { .. } => Swap::ReflectBondsRedeem,
        };
        Ok(SwapAndAccountMetas { swap, account_metas: ix.accounts })
    }

    fn supports_exact_out(&self) -> bool {
        false
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed(owner: Pubkey) -> KeyedAccount {
        KeyedAccount { key: Pubkey::new_unique(), account: Account { lamports: 1, data: vec![0u8; 8], owner, executable: false, rent_epoch: 0 } }
    }

    // A cold Amm exposes no mints, advertises its dynamic-account contract,
    // ExactIn only, and a real quote on it errors (nothing loaded).
    #[test]
    fn cold_amm_has_no_reserve_mints_and_quote_errs() {
        let ctx = AmmContext { clock_ref: ClockRef::default() };
        let amm = ReflectBondsAmm::from_keyed_account(&keyed(Pubkey::new_unique()), &ctx).expect("from_keyed_account");
        assert!(amm.get_reserve_mints().is_empty());
        assert!(amm.requires_update_for_reserve_mints());
        assert!(amm.has_dynamic_accounts());
        assert!(!amm.supports_exact_out());
        assert!(amm.quote(&QuoteParams { amount: 1, input_mint: Pubkey::new_unique(), output_mint: Pubkey::new_unique(), swap_mode: SwapMode::ExactIn }).is_err());
    }

    // Cold `get_accounts_to_update` is exactly the 4 bond seeds (not the
    // eventual full discovery set, which needs their bytes first).
    #[test]
    fn cold_accounts_to_update_is_the_four_seeds() {
        let ctx = AmmContext { clock_ref: ClockRef::default() };
        let owner = Pubkey::new_unique();
        let amm = ReflectBondsAmm::from_keyed_account(&keyed(owner), &ctx).expect("from_keyed_account");
        assert_eq!(amm.get_accounts_to_update(), amm.venue.seeds());
        assert_eq!(amm.get_accounts_to_update().len(), 4);
    }
}
