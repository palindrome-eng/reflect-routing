//! Reflect **lending + bonds** routing: the account plumbing + SDK delegation
//! shared by the Jupiter and Titan shims. The SDK owns all protocol math;
//! [`LendingVenue`] / [`bonds::BondVenue`] are account plumbing plus the
//! mint→operation dispatch.

mod bonds;
mod rlp;
mod whitelabel;

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::ids::TOKEN_PROGRAM;
use reflect_sdk::common::Now;
use reflect_sdk::strategy::{lending_discovery_keys, pda, FetchedAccount, LendingCore, Strategy, TradablePair};

pub use bonds::BondVenue;
pub use reflect_sdk::strategy::{IndexMints, IndexRoute, MintInfo};
pub use rlp::{RlpRoute, RlpVenue};
pub use whitelabel::{WhitelabelRoute, WhitelabelVenue};

/// A product core settled at one clock, kept by a venue between quotes. A quote at that clock
/// reads it; a quote at any other clock settles for itself (the Jupiter adapters pass their own
/// clock per quote).
#[derive(Clone)]
pub(crate) struct Settled<K>(Option<(Now, K)>);

impl<K: Clone> Settled<K> {
    pub(crate) const NONE: Self = Self(None);

    /// Keep `core` for `now`; `None` (a failed settlement) keeps nothing, so `at` reports no
    /// route at that clock.
    pub(crate) fn set(&mut self, now: Now, core: Option<K>) {
        self.0 = core.map(|k| (now, k));
    }

    /// The core at `clock`: the kept one when the clocks match, else `settle()`'s.
    pub(crate) fn at(&self, clock: Now, settle: impl FnOnce() -> Option<K>) -> Option<Cow<'_, K>> {
        match &self.0 {
            Some((at, k)) if *at == clock => Some(Cow::Borrowed(k)),
            _ => settle().map(Cow::Owned),
        }
    }
}

/// Decode an SPL mint account fetched alongside a venue's account set.
pub(crate) fn spl_mint_info(mint: Pubkey, fetched: &[(Pubkey, Pubkey, Vec<u8>)]) -> Result<MintInfo> {
    let (_, _, data) = fetched.iter().find(|(k, _, _)| *k == mint).context("mint account missing")?;
    Ok(MintInfo { pubkey: mint, decimals: *data.get(44).context("mint account too short")?, token_program: TOKEN_PROGRAM })
}

/// A successful quote.
#[derive(Debug, Clone, Copy)]
pub struct RouteQuote {
    /// Integer atom output of the counterpart mint.
    pub amount_out: u64,
    /// Fee the venue reports; each venue documents its mint (lending: input; rlp swap: output).
    pub fee: u64,
}

/// Titan's marginal price, output atoms per input atom, read off the quote curve alone.
///
/// `amount == 0` is the spot: the secant through the origin at `probe`, one whole input
/// unit. Otherwise the forward secant from `(amount, out)` over `h = max(amount / 100, probe)`,
/// backward when the upper probe has no route. Floor noise is bounded by `1 / h` of the
/// rate, under Titan's 1e-5 mean-value tolerance for any whole-unit window, and a forward
/// secant on a concave curve sits between the true slope and the chord Titan checks it
/// against. `None` when no probe quotes.
///
/// ponytail: interim until the SDK exposes an exact amount-free `spot`; one extra core
/// quote per call.
pub fn marginal_price(quote: impl Fn(u64) -> Option<u64>, amount: u64, out: u64, probe: u64) -> Option<f64> {
    let probe = probe.max(1);
    if amount == 0 {
        return Some(quote(probe)? as f64 / probe as f64);
    }
    let h = (amount / 100).max(probe);
    let secant = |a: u64, out_a: u64, b: u64, out_b: u64| Some((out_b as f64 - out_a as f64) / (b - a) as f64);
    match amount.checked_add(h).and_then(|hi| Some((hi, quote(hi)?))) {
        Some((hi, out_hi)) => secant(amount, out, hi, out_hi),
        None => {
            let lo = amount.saturating_sub(h);
            secant(lo, quote(lo)?, amount, out)
        },
    }
}

/// The Reflect lending strategy (deposit ↔ receipt), bound to one controller.
/// All protocol logic is `reflect-sdk`'s; the built [`Strategy`] is
/// cached behind an `Arc` once per [`load`](Self::load), so quote/build reuse
/// it (Titan's `bounds()` quotes dozens of times) and `Clone` stays cheap.
///
/// Load is two-phase: [`seeds`](Self::seeds) → fetch →
/// [`discovery_keys`](Self::discovery_keys) → fetch the full set →
/// [`load`](Self::load). The shim drives the fetching (its host owns the I/O);
/// the venue owns the key derivation and the SDK calls.
#[derive(Clone)]
pub struct LendingVenue {
    program_id: Pubkey,
    controller: Pubkey,
    loaded: Option<Arc<Strategy>>,
    pair: Option<TradablePair>,
    /// The strategy settled for quoting; see [`Settled`].
    core: Settled<LendingCore>,
}

impl LendingVenue {
    /// Construct cold from the market account the aggregator lists: `market` is
    /// its key, `owner` its program id.
    pub fn from_market(market: Pubkey, owner: Pubkey) -> Self {
        // The controller account is owned by the Reflect program; the strategy
        // index lives in the controller bytes, read later by the SDK - so the
        // cold seed set needs no index: [controller, pool_registry].
        Self { program_id: owner, controller: market, loaded: None, pair: None, core: Settled::NONE }
    }

    /// The owning Reflect program id.
    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    /// The market/pool key (one market = one mint pair).
    pub fn market(&self) -> Pubkey {
        self.controller
    }

    /// The accounts to fetch before the full set is known.
    pub fn seeds(&self) -> Vec<Pubkey> {
        vec![self.controller, pda::derive_pool_registry(&self.program_id).0]
    }

    /// Given the two seed accounts' bytes, the full set of keys to fetch
    /// (controller and registry are included, for refetch-in-one-batch).
    pub fn discovery_keys(&self, controller: &[u8], registry: &[u8]) -> Result<Vec<Pubkey>> {
        lending_discovery_keys(controller, registry, &self.program_id).map_err(|e| anyhow!("discovery_keys: {e:?}"))
    }

    /// Ingest the fetched `(key, owner, data)` set: build the SDK loader once,
    /// cache it, and resolve the tradable pair.
    ///
    /// # Errors
    /// If the fetched bytes don't decode into a valid controller.
    pub fn load(&mut self, accounts: impl IntoIterator<Item = (Pubkey, Pubkey, Vec<u8>)>) -> Result<()> {
        let bytes = accounts.into_iter().map(|(key, owner, data)| FetchedAccount { key, owner, data }).collect();
        let accts = Strategy::from_accounts(self.program_id, self.controller, bytes).map_err(|e| anyhow!("from_accounts: {e:?}"))?;
        self.pair = accts.tradable_pair().ok(); // None for reserve-only strategies
        self.loaded = Some(Arc::new(accts));
        Ok(())
    }

    /// The mints this venue trades (empty until loaded): `[deposit, receipt]`.
    pub fn mints(&self) -> Vec<MintInfo> {
        // [deposit (idx 0), receipt (idx 1)] - Titan's bounds() relies on this order.
        self.pair.as_ref().map_or(vec![], |p| vec![p.deposit, p.receipt])
    }

    /// Settle the loaded strategy at `now`: one yield settlement, venue accrual and gate
    /// evaluation, so every quote is arithmetic over owned state. A settlement that fails
    /// leaves no core, which `quote` reports as no route.
    ///
    /// # Errors
    /// If the venue isn't loaded.
    pub fn settle(&mut self, now: Now) -> Result<()> {
        self.core.set(now, self.loaded()?.lending_core(now).ok());
        Ok(())
    }

    fn loaded(&self) -> Result<&Strategy> {
        self.loaded.as_deref().context("venue not loaded")
    }

    /// Quote `amount` of `input_mint` into its counterpart mint.
    ///
    /// `None` is "no route": the SDK predicts a revert (supply floor / cap).
    /// `amount == 0` returns `Some(zero)` - the documented zero-input contract
    /// (`bounds()` binary-searches on it; never panics).
    ///
    /// # Errors
    /// If the venue isn't loaded, or the SDK quote call fails (malformed state).
    pub fn quote(&self, input_mint: Pubkey, amount: u64, clock: Now) -> Result<Option<RouteQuote>> {
        // Zero input is a hard contract: clean 0, never panic or error. Lending is
        // one pair; the SDK dispatches mint/redeem on the input mint alone.
        if amount == 0 {
            return Ok(Some(RouteQuote { amount_out: 0, fee: 0 }));
        }
        let pair = self.pair.as_ref().context("no tradable pair")?;
        let loaded = self.loaded()?;
        let Some(k) = self.core.at(clock, || loaded.lending_core(clock).ok()) else { return Ok(None) };
        let q = if input_mint == pair.deposit.pubkey {
            k.quote_mint(amount)
        } else if input_mint == pair.receipt.pubkey {
            k.quote_redeem(amount)
        } else {
            bail!("mint {input_mint} is not in this strategy's pair");
        }
        .map_err(|e| anyhow!("quote: {e:?}"))?;
        // A predicted revert (supply floor / cap / venue limit) is no route, not a bad route.
        Ok(q.would_revert.is_none().then_some(RouteQuote { amount_out: q.amount_out, fee: q.fee }))
    }

    /// Build the swap instruction. `min_out` is baked from `slippage_bps`; pass `0`
    /// for a host that applies its own (Jupiter's program writes the instruction
    /// data from the route context).
    ///
    /// # Errors
    /// If the venue isn't loaded, or the SDK's mint/redeem build fails.
    pub fn build(&self, user: Pubkey, input_mint: Pubkey, amount: u64, slippage_bps: u16, clock: Now) -> Result<Instruction> {
        let accts = self.loaded()?;
        let pair = self.pair.as_ref().context("no tradable pair")?;
        let h = accts.lending(user).map_err(|e| anyhow!("lending: {e:?}"))?;
        // The one dispatch a venue owns: input == deposit ⇒ mint, else redeem (the
        // SDK's TradablePair rule). Build with the request amount, not the quote.
        let (_q, built) = if input_mint == pair.deposit.pubkey {
            h.mint(amount, slippage_bps, clock).map_err(|e| anyhow!("mint: {e:?}"))?
        } else {
            h.redeem(amount, slippage_bps, clock).map_err(|e| anyhow!("redeem: {e:?}"))?
        };
        Ok(built.instruction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_venue_has_no_mints_and_zero_quote_is_zero() {
        let v = LendingVenue::from_market(Pubkey::new_unique(), Pubkey::new_unique());
        assert!(v.mints().is_empty());
        // Zero input is routable-with-zero-output, never an error, even unloaded.
        let z = v.quote(Pubkey::new_unique(), 0, Now { slot: 0, unix_timestamp: 0 }).unwrap();
        assert!(matches!(z, Some(RouteQuote { amount_out: 0, .. })));
        // A real amount on an unloaded venue is an error (nothing cached).
        assert!(v.quote(Pubkey::new_unique(), 1, Now { slot: 0, unix_timestamp: 0 }).is_err());
    }
}

#[cfg(test)]
mod marginal_price_tests {
    use super::marginal_price;

    // out = floor(in * 3 / 2): spot and every marginal equal 1.5 to within a floor.
    #[test]
    fn linear_route_is_constant() {
        let q = |a: u64| Some(a * 3 / 2);
        assert_eq!(marginal_price(q, 0, 0, 1_000_000), Some(1.5));
        let m = marginal_price(q, 123_457, q(123_457).unwrap(), 1_000_000).unwrap();
        assert!((m - 1.5).abs() < 1e-3, "{m}");
    }

    // out = in - in^2 / 1e9: slope 1 - 2x/1e9, decreasing in size.
    #[test]
    fn curved_route_slope_decreases() {
        let q = |a: u64| Some(a - (a as u128 * a as u128 / 1_000_000_000) as u64);
        let small = marginal_price(q, 1_000_000, q(1_000_000).unwrap(), 1_000_000).unwrap();
        let large = marginal_price(q, 100_000_000, q(100_000_000).unwrap(), 1_000_000).unwrap();
        // Forward secant over 1%: slope at 1e6 is 0.998, at 1e8 it is 0.8, each minus h/1e9.
        assert!(small > large && (small - 0.997).abs() < 1e-3 && (large - 0.799).abs() < 1e-3, "{small} {large}");
    }

    // Above the cap the upper probe has no route: the secant falls back to the lower side.
    #[test]
    fn capped_route_uses_backward_secant() {
        let q = |a: u64| (a <= 10_000_000).then_some(a * 2);
        assert_eq!(marginal_price(q, 10_000_000, 20_000_000, 1_000_000), Some(2.0));
        assert_eq!(marginal_price(q, 20_000_000, 0, 1_000_000), None);
    }
}
