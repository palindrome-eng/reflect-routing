//! Reflect **lending** routing: the account plumbing + SDK delegation shared by
//! the Jupiter and Titan shims. The SDK owns every bit of protocol math (load +
//! quote + build); [`LendingVenue`] is account plumbing plus the mint→operation
//! dispatch, delegated to `reflect-sdk`.

use anyhow::{anyhow, Context, Result};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::Clock;
use reflect_sdk::strategy::{lending_discovery_keys, pda, FetchedAccount, StrategyAccounts, TradablePair};

/// A token a venue trades. Aggregators take the subset they need (Jupiter wants
/// the pubkey; Titan wants decimals + token program for its `TokenInfo`).
#[derive(Debug, Clone, Copy)]
pub struct RouteMint {
    pub pubkey: Pubkey,
    pub decimals: u8,
    pub token_program: Pubkey,
}

/// A successful quote. `amount_out` is the integer atom output; `fee` is the
/// input-denominated fee (Jupiter surfaces it; Titan ignores it).
#[derive(Debug, Clone, Copy)]
pub struct RouteQuote {
    pub amount_out: u64,
    pub fee: u64,
}

/// Account bytes by key - the lookup a venue needs to discover and load state.
/// Each aggregator shim adapts its host's fetch model (Jupiter's `AccountMap`,
/// Titan's `AccountsCache`) to this; returns `(owner, data)`.
pub trait AccountBytes {
    fn get(&self, key: &Pubkey) -> Option<(Pubkey, Vec<u8>)>;
}

/// The Reflect lending strategy (deposit ↔ receipt), bound to one controller.
/// All protocol logic is `reflect-sdk`'s. `StrategyAccounts` is not `Clone`, so
/// the venue caches the raw bytes and rebuilds the loader per quote/build - the
/// per-call cost both aggregators already accept.
///
/// Load is two-phase: [`seeds`](Self::seeds) → fetch →
/// [`discovery_keys`](Self::discovery_keys) → fetch the full set →
/// [`load`](Self::load). The shim drives the fetching (its host owns the I/O);
/// the venue owns the key derivation and the SDK calls.
#[derive(Clone)]
pub struct LendingVenue {
    program_id: Pubkey,
    controller: Pubkey,
    bytes: Vec<FetchedAccount>,
    pair: Option<TradablePair>,
}

impl LendingVenue {
    /// Construct cold from the market account the aggregator lists: `market` is
    /// its key, `owner` its program id.
    pub fn from_market(market: Pubkey, owner: Pubkey) -> Self {
        // The controller account is owned by the Reflect program; the strategy
        // index lives in the controller bytes, read later by the SDK - so the
        // cold seed set needs no index: [controller, pool_registry].
        Self { program_id: owner, controller: market, bytes: vec![], pair: None }
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

    /// Given the seed accounts' bytes, the full set of keys to fetch. `None` ⇒
    /// the seeds aren't available yet (stay cold).
    pub fn discovery_keys(&self, seeds: &dyn AccountBytes) -> Option<Vec<Pubkey>> {
        let reg = pda::derive_pool_registry(&self.program_id).0;
        let (_, controller) = seeds.get(&self.controller)?;
        let (_, registry) = seeds.get(&reg)?;
        lending_discovery_keys(&controller, &registry, &self.program_id).ok()
    }

    /// Ingest the full account set; resolves the tradable pair. No-op (`Ok`)
    /// while still cold. Idempotent.
    ///
    /// # Errors
    /// If the fetched bytes don't decode into a valid controller.
    pub fn load(&mut self, accounts: &dyn AccountBytes) -> Result<()> {
        let Some(keys) = self.discovery_keys(accounts) else {
            return Ok(()); // cold: seeds not present yet
        };
        let bytes: Vec<FetchedAccount> = keys.iter().filter_map(|k| accounts.get(k).map(|(owner, data)| FetchedAccount { key: *k, owner, data })).collect();
        let accts = StrategyAccounts::from_controller(&self.program_id, &self.controller, bytes.clone()).map_err(|e| anyhow!("from_controller: {e:?}"))?;
        self.pair = accts.tradable_pair().ok(); // None for reserve-only strategies
        self.bytes = bytes;
        Ok(())
    }

    /// The mints this venue trades (empty until loaded): `[deposit, receipt]`.
    pub fn mints(&self) -> Vec<RouteMint> {
        // [deposit (idx 0), receipt (idx 1)] - Titan's bounds() relies on this order.
        let mint = |m: &reflect_sdk::strategy::MintInfo| RouteMint { pubkey: m.pubkey, decimals: m.decimals, token_program: m.token_program };
        self.pair.as_ref().map_or(vec![], |p| vec![mint(&p.deposit), mint(&p.receipt)])
    }

    fn loaded(&self) -> Result<StrategyAccounts> {
        StrategyAccounts::from_controller(&self.program_id, &self.controller, self.bytes.clone()).map_err(|e| anyhow!("from_controller: {e:?}"))
    }

    /// Quote `amount` of `input_mint` into its counterpart mint.
    ///
    /// `None` is "no route": the SDK predicts a revert (supply floor / cap) or the
    /// venue isn't loaded. `amount == 0` returns `Some(zero)` - the documented
    /// zero-input contract (`bounds()` binary-searches on it; never panics).
    ///
    /// # Errors
    /// If the venue is loaded but the SDK quote call itself fails (malformed state).
    pub fn quote(&self, input_mint: Pubkey, amount: u64, clock: Clock) -> Result<Option<RouteQuote>> {
        // Zero input is a hard contract: clean 0, never panic or error. Lending is
        // one pair; the SDK dispatches mint/redeem on the input mint alone.
        if amount == 0 {
            return Ok(Some(RouteQuote { amount_out: 0, fee: 0 }));
        }
        let q = self.loaded()?.quote_by_mints(input_mint, amount, clock).map_err(|e| anyhow!("quote: {e:?}"))?;
        // A predicted revert (supply floor / cap) is no route, not a bad route.
        Ok(q.would_revert.is_none().then_some(RouteQuote { amount_out: q.amount_out, fee: q.fee }))
    }

    /// Build the swap instruction. `min_out` is baked from `slippage_bps` and
    /// `headroom` (redeem dust); pass `0, 0` for a host that applies its own
    /// (Jupiter's program writes the instruction data from the route context).
    ///
    /// # Errors
    /// If the venue isn't loaded, or the SDK's mint/redeem build fails.
    pub fn build(&self, user: Pubkey, input_mint: Pubkey, amount: u64, slippage_bps: u16, headroom: u64, clock: Clock) -> Result<Instruction> {
        let accts = self.loaded()?;
        let pair = self.pair.as_ref().context("no tradable pair")?;
        let h = accts.lending(user, pair.deposit.pubkey, pair.deposit.token_program).map_err(|e| anyhow!("lending: {e:?}"))?;
        // The one dispatch a venue owns: input == deposit ⇒ mint, else redeem (the
        // SDK's TradablePair rule). Build with the request amount, not the quote.
        let (_q, built) = if input_mint == pair.deposit.pubkey {
            h.mint(amount, slippage_bps, clock).map_err(|e| anyhow!("mint: {e:?}"))?
        } else {
            h.redeem(amount, slippage_bps, headroom, clock).map_err(|e| anyhow!("redeem: {e:?}"))?
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
        let z = v.quote(Pubkey::new_unique(), 0, Clock { slot: 0, unix: 0 }).unwrap();
        assert!(matches!(z, Some(RouteQuote { amount_out: 0, .. })));
        // A real amount on an unloaded venue is an error (no bytes to load).
        assert!(v.quote(Pubkey::new_unique(), 1, Clock { slot: 0, unix: 0 }).is_err());
    }
}
