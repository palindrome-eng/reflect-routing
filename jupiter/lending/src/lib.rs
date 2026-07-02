//! `reflect-amm-jupiter` - the Reflect lending strategy as a Jupiter `Amm`.
//!
//! The `Amm` trait + `Swap` tag are vendored (see [`swap`]) to stay on solana
//! 2.2 alongside `reflect-sdk`. This crate is a thin shim: [`ReflectLendingAmm`]
//! maps [`LendingVenue`] onto Jupiter's contract (two-phase account handshake,
//! atomic clock, metas-only swap build). All routing/protocol logic lives in
//! `reflect-routing-core` + `reflect-sdk`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use solana_sdk::account::Account;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::instruction::AccountMeta;
use solana_sdk::pubkey::Pubkey;

use reflect_routing_core::LendingVenue;
use reflect_sdk::common::Clock as ReflectClock;

mod swap;
pub use swap::{AccountsType, RemainingAccountsInfo, RemainingAccountsSlice, Side, Swap};

/// ExactIn / ExactOut. The SDK's lending math is ExactIn only; the adapter
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

/// [`LendingVenue`] presented as a Jupiter market.
#[derive(Clone)]
pub struct ReflectLendingAmm {
    venue: LendingVenue,
    clock_ref: ClockRef,
    controller_data: Option<Vec<u8>>,
    pool_registry_data: Option<Vec<u8>>,
}

impl ReflectLendingAmm {
    fn clock(&self) -> ReflectClock {
        ReflectClock { slot: self.clock_ref.slot.load(Ordering::Relaxed), unix: self.clock_ref.unix_timestamp.load(Ordering::Relaxed) }
    }
}

impl Amm for ReflectLendingAmm {
    fn from_keyed_account(ka: &KeyedAccount, ctx: &AmmContext) -> Result<Self> {
        Ok(Self {
            venue: LendingVenue::from_market(ka.key, ka.account.owner),
            clock_ref: ctx.clock_ref.clone(),
            controller_data: Some(ka.account.data.clone()),
            pool_registry_data: None,
        })
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
        match (&self.controller_data, &self.pool_registry_data) {
            (Some(c), Some(r)) => self.venue.discovery_keys(c, r).unwrap_or_else(|_| seeds.clone()),
            _ => seeds, // cold: [controller, registry]
        }
    }

    fn update(&mut self, m: &AccountMap) -> Result<()> {
        let seeds = self.venue.seeds();

        self.controller_data = m.get(&seeds[0]).map(|a| a.data.clone());
        self.pool_registry_data = m.get(&seeds[1]).map(|a| a.data.clone());
        let (Some(c), Some(r)) = (&self.controller_data, &self.pool_registry_data) else {
            return Ok(()); // cold: seeds not in the map yet
        };

        // A present-but-undecodable seed is an error, not "still cold".
        let keys = self.venue.discovery_keys(c, r)?;
        self.venue.load(keys.iter().filter_map(|k| m.get(k).map(|a| (*k, a.owner, a.data.clone()))))
    }

    fn quote(&self, p: &QuoteParams) -> Result<Quote> {
        match self.venue.quote(p.input_mint, p.amount, self.clock())? {
            Some(q) => Ok(Quote { in_amount: p.amount, out_amount: q.amount_out, fee_amount: q.fee, fee_mint: p.input_mint }),
            None => bail!("reflect would revert: no route > bad route"),
        }
    }

    fn get_swap_and_account_metas(&self, sp: &SwapParams) -> Result<SwapAndAccountMetas> {
        // Jupiter takes ONLY the account list: its on-chain program writes the
        // instruction data (amount, min_out) from the route context, so build with
        // slippage 0 / headroom 0 - the serialized data is discarded.
        let ix = self.venue.build(sp.token_transfer_authority, sp.source_mint, sp.in_amount, 0, 0, self.clock())?;

        // One permissioned Reflect variant; the on-chain handler reads the
        // direction from the resolved account ORDER, so order is the contract.
        Ok(SwapAndAccountMetas { swap: Swap::ReflectS1, account_metas: ix.accounts })
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

    // A cold Amm exposes no pair, advertises its dynamic-account contract, and a
    // real quote on it errors (nothing loaded). Mirrors the offline parity test.
    #[test]
    fn cold_amm_has_no_reserve_mints_and_quote_errs() {
        let ctx = AmmContext { clock_ref: ClockRef::default() };
        let amm = ReflectLendingAmm::from_keyed_account(&keyed(Pubkey::new_unique()), &ctx).expect("from_keyed_account");
        assert!(amm.get_reserve_mints().is_empty());
        assert!(amm.requires_update_for_reserve_mints());
        assert!(amm.has_dynamic_accounts());
        assert!(!amm.supports_exact_out());
        assert!(amm.quote(&QuoteParams { amount: 1, input_mint: Pubkey::new_unique(), output_mint: Pubkey::new_unique(), swap_mode: SwapMode::ExactIn }).is_err());
    }
}
