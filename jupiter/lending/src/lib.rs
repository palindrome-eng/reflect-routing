//! `reflect-amm-jupiter` — the Reflect lending strategy as a Jupiter `Amm`.
//!
//! The `Amm` trait + `Swap` tag are vendored (see [`swap`]) to stay on solana
//! 2.2 alongside `reflect-sdk`. The adapter ([`ReflectLendingAmm`]) holds no
//! protocol logic: it caches the fetched bytes and delegates every quote/build
//! to `reflect-sdk`, so a quote is byte-identical to execution.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use solana_sdk::account::Account;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::instruction::AccountMeta;
use solana_sdk::pubkey::Pubkey;

use reflect_sdk::common::Clock as ReflectClock;
use reflect_sdk::strategy::{lending_discovery_keys, pda, FetchedAccount, StrategyAccounts, TradablePair};

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

/// Mint (deposit → receipt) vs redeem (receipt → deposit) — the one dispatch the
/// adapter owns, on the SDK's `TradablePair` rule (`in == deposit ⇒ mint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Mint,
    Redeem,
}

fn direction(input_mint: &Pubkey, pair: &TradablePair) -> Direction {
    if *input_mint == pair.deposit.pubkey {
        Direction::Mint
    } else {
        Direction::Redeem
    }
}

/// A Reflect lending strategy as a Jupiter market (one controller = one market =
/// one mint pair). Caches the fetched bytes; rebuilds the SDK loader on demand
/// (the SDK is the only place protocol logic runs).
#[derive(Clone)]
pub struct ReflectLendingAmm {
    program_id: Pubkey,
    controller_key: Pubkey,
    clock_ref: ClockRef,
    // cold → warm two-phase cache:
    controller_data: Option<Vec<u8>>,
    pool_registry_data: Option<Vec<u8>>,
    // built in update(): the full fetched set (Clone, so the AMM stays Clone),
    // plus the resolved pair.
    bytes: Vec<FetchedAccount>,
    pair: Option<TradablePair>,
}

impl ReflectLendingAmm {
    fn clock(&self) -> ReflectClock {
        ReflectClock { slot: self.clock_ref.slot.load(Ordering::Relaxed), unix: self.clock_ref.unix_timestamp.load(Ordering::Relaxed) }
    }

    /// Rebuild the SDK loader from the cached bytes. `StrategyAccounts` is not
    /// `Clone`, so the AMM caches the raw bytes and reconstitutes the loader per
    /// quote/build — the same per-call cost the Jupiter model already accepts.
    fn loaded(&self) -> Result<StrategyAccounts> {
        StrategyAccounts::from_controller(&self.program_id, &self.controller_key, self.bytes.clone()).map_err(|e| anyhow!("from_controller: {e:?}"))
    }
}

impl Amm for ReflectLendingAmm {
    fn from_keyed_account(ka: &KeyedAccount, ctx: &AmmContext) -> Result<Self> {
        // The "market" Jupiter lists is the controller account, owned by the
        // Reflect program. We hold its key, so no strategy index is needed — the
        // SDK reads it from the controller bytes in update().
        let program_id = ka.account.owner;

        Ok(Self {
            program_id,
            controller_key: ka.key,
            clock_ref: ctx.clock_ref.clone(),
            controller_data: Some(ka.account.data.clone()),
            pool_registry_data: None,
            bytes: vec![],
            pair: None,
        })
    }

    fn label(&self) -> String {
        "Reflect".to_string()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn key(&self) -> Pubkey {
        self.controller_key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        match &self.pair {
            Some(p) => vec![p.deposit.pubkey, p.receipt.pubkey],
            None => vec![],
        }
    }

    fn requires_update_for_reserve_mints(&self) -> bool {
        true // the pair is known only after update()
    }

    fn has_dynamic_accounts(&self) -> bool {
        true // cold start returns seeds; re-called after update for the full set
    }

    /// Two-phase handshake (needs `has_dynamic_accounts() == true`): cold start
    /// returns the 2 seeds `[controller, pool_registry]`; once `update()` has
    /// cached their bytes, decode them into the full venue set. On decode failure
    /// fall back to the seeds (re-cold) rather than panic.
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let seeds = || vec![self.controller_key, pda::derive_pool_registry(&self.program_id).0];

        match (&self.controller_data, &self.pool_registry_data) {
            (Some(c), Some(r)) => lending_discovery_keys(c, r, &self.program_id).unwrap_or_else(|_| seeds()),
            _ => seeds(), // cold: [controller, registry]
        }
    }

    fn update(&mut self, m: &AccountMap) -> Result<()> {
        let reg_key = pda::derive_pool_registry(&self.program_id).0;
        self.controller_data = m.get(&self.controller_key).map(|a| a.data.clone());
        self.pool_registry_data = m.get(&reg_key).map(|a| a.data.clone());
        let (Some(c), Some(r)) = (&self.controller_data, &self.pool_registry_data) else {
            return Ok(()); // cold phase: seeds not in the map yet
        };

        let keys = lending_discovery_keys(c, r, &self.program_id).map_err(|e| anyhow!("discovery_keys: {e:?}"))?;
        let bytes: Vec<FetchedAccount> = keys.iter().filter_map(|k| m.get(k).map(|a| FetchedAccount { key: *k, owner: a.owner, data: a.data.clone() })).collect();

        let accts = StrategyAccounts::from_controller(&self.program_id, &self.controller_key, bytes.clone()).map_err(|e| anyhow!("from_controller: {e:?}"))?;
        self.pair = accts.tradable_pair().ok(); // None for reserve-only strategies
        self.bytes = bytes;

        Ok(())
    }

    fn quote(&self, p: &QuoteParams) -> Result<Quote> {
        // The SDK owns the mint→op dispatch + the (user-less) quote context.
        let aq = self.loaded()?.quote_by_mints(p.input_mint, p.amount, self.clock()).map_err(|e| anyhow!("quote: {e:?}"))?;

        if let Some(rc) = aq.would_revert {
            bail!("reflect would revert: {rc:?}"); // no route > bad route
        }

        Ok(Quote { in_amount: p.amount, out_amount: aq.amount_out, fee_amount: aq.fee, fee_mint: p.input_mint })
    }

    fn get_swap_and_account_metas(&self, sp: &SwapParams) -> Result<SwapAndAccountMetas> {
        let accts = self.loaded()?;
        let pair = self.pair.as_ref().context("no tradable pair")?;
        let h = accts.lending(sp.token_transfer_authority, pair.deposit.pubkey, pair.deposit.token_program).map_err(|e| anyhow!("lending: {e:?}"))?;

        // Jupiter takes ONLY the account list: its on-chain program builds the
        // instruction data (amount, min_out) from the route context. The amount
        // still drives account resolution, so build with sp.in_amount and
        // slippage 0 — the serialized data is discarded by Jupiter.
        let (_aq, built) = match direction(&sp.source_mint, pair) {
            Direction::Mint => h.mint(sp.in_amount, 0, self.clock()).map_err(|e| anyhow!("mint: {e:?}"))?,
            Direction::Redeem => h.redeem(sp.in_amount, 0, 0, self.clock()).map_err(|e| anyhow!("redeem: {e:?}"))?,
        };

        // One permissioned Reflect variant; the on-chain handler reads the
        // direction from the resolved account ORDER, so order is the contract.
        Ok(SwapAndAccountMetas { swap: Swap::ReflectS1, account_metas: built.instruction.accounts })
    }

    fn supports_exact_out(&self) -> bool {
        false // SDK lending is ExactIn; no honest inverse solve — don't lie
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_sdk::strategy::MintInfo;

    fn pair() -> TradablePair {
        TradablePair {
            deposit: MintInfo { pubkey: Pubkey::new_unique(), token_program: reflect_sdk::common::ids::TOKEN_PROGRAM, decimals: 6 },
            receipt: MintInfo { pubkey: Pubkey::new_unique(), token_program: reflect_sdk::common::ids::TOKEN_PROGRAM, decimals: 6 },
        }
    }

    // The one dispatch the adapter owns: deposit-in ⇒ mint, receipt-in ⇒ redeem.
    #[test]
    fn dir_dispatches_on_deposit_mint() {
        let p = pair();
        assert_eq!(direction(&p.deposit.pubkey, &p), Direction::Mint);
        assert_eq!(direction(&p.receipt.pubkey, &p), Direction::Redeem);
    }

    // No pair loaded ⇒ no reserve mints, and quote surfaces the missing pair.
    #[test]
    fn cold_amm_has_no_reserve_mints_and_quote_errs() {
        let amm = ReflectLendingAmm {
            program_id: Pubkey::new_unique(),
            controller_key: Pubkey::new_unique(),
            clock_ref: ClockRef::default(),
            controller_data: None,
            pool_registry_data: None,
            bytes: vec![],
            pair: None,
        };
        assert!(amm.get_reserve_mints().is_empty());
        assert!(amm.requires_update_for_reserve_mints());
        assert!(amm.has_dynamic_accounts());
        assert!(!amm.supports_exact_out());
        assert!(amm.quote(&QuoteParams { amount: 1, input_mint: Pubkey::new_unique(), output_mint: Pubkey::new_unique(), swap_mode: SwapMode::ExactIn }).is_err());
    }
}
