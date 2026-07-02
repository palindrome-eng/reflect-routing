//! Reflect **bonds** routing: the account plumbing + SDK delegation shared by
//! the Jupiter and Titan shims. The SDK owns all protocol math, including the
//! mint-pair → operation classification ([`BondRoute`]); [`BondVenue`] is
//! account plumbing only.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::Clock;
use reflect_sdk::strategy::{bond_discovery_keys, pda, StrategyAccounts, TokenInput};

pub use reflect_sdk::strategy::{BondMints, BondRoute, MintInfo};

use crate::RouteQuote;

/// The Reflect bond basket (1 receipt + N assets), bound to one controller.
/// All protocol logic is `reflect-sdk`'s; the built [`StrategyAccounts`] is
/// cached behind an `Arc` once per [`load`](Self::load), so quote/build reuse
/// it and `Clone` stays cheap.
///
/// Load is two-phase: [`seeds`](Self::seeds) → fetch →
/// [`discovery_keys`](Self::discovery_keys) → fetch the full set →
/// [`load`](Self::load). The shim drives the fetching (its host owns the I/O);
/// the venue owns the key derivation and the SDK calls.
#[derive(Clone)]
pub struct BondVenue {
    program_id: Pubkey,
    controller: Pubkey,
    loaded: Option<Arc<StrategyAccounts>>,
    mints: Option<BondMints>,
}

impl BondVenue {
    /// Construct cold from the market account the aggregator lists: `market` is
    /// its key, `owner` its program id.
    pub fn from_market(market: Pubkey, owner: Pubkey) -> Self {
        Self { program_id: owner, controller: market, loaded: None, mints: None }
    }

    /// The owning Reflect program id.
    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    /// The market/pool key (one market = one basket controller).
    pub fn market(&self) -> Pubkey {
        self.controller
    }

    /// The accounts to fetch before the full set is known:
    /// `[controller, asset_registry, multiocular, main]` — this ORDER is the
    /// contract [`discovery_keys`](Self::discovery_keys) consumes. Unlike
    /// lending's 2 seeds, bonds need 4 (the basket, registry, and oracle-route
    /// facts all live off the controller).
    pub fn seeds(&self) -> Vec<Pubkey> {
        vec![self.controller, pda::derive_asset_registry(&self.program_id).0, pda::derive_multiocular(&self.program_id).0, pda::derive_main(&self.program_id).0]
    }

    /// Given the four seed accounts' bytes (controller, asset_registry,
    /// multiocular, main — [`seeds`](Self::seeds) order), the full set of keys
    /// to fetch.
    pub fn discovery_keys(&self, controller: &[u8], asset_registry: &[u8], multiocular: &[u8], main: &[u8]) -> Result<Vec<Pubkey>> {
        bond_discovery_keys(controller, asset_registry, multiocular, main, &self.program_id).map_err(|e| anyhow!("discovery_keys: {e:?}"))
    }

    /// Ingest the fetched `(key, owner, data)` set: build the SDK loader once,
    /// cache it, and resolve the basket's mints.
    ///
    /// # Errors
    /// If the fetched bytes don't decode into a valid controller, or the mint
    /// accounts are missing/undecodable — unlike lending's reserve-only case,
    /// every bond basket has assets, so a bond controller with unreadable
    /// mints is a malformed market, not a legitimate empty one.
    pub fn load(&mut self, accounts: impl IntoIterator<Item = (Pubkey, Pubkey, Vec<u8>)>) -> Result<()> {
        let accts = StrategyAccounts::from_account_map(&self.program_id, &self.controller, accounts).map_err(|e| anyhow!("from_controller: {e:?}"))?;
        let mints = accts.bond_mints().map_err(|e| anyhow!("bond_mints: {e:?}"))?;
        self.mints = Some(mints);
        self.loaded = Some(Arc::new(accts));
        Ok(())
    }

    /// The mints this venue trades (empty until loaded): `[receipt, asset_0..n]`
    /// in basket order.
    pub fn mints(&self) -> Vec<MintInfo> {
        self.mints.as_ref().map_or(vec![], |m| core::iter::once(m.receipt).chain(m.assets.iter().map(|(_, mi)| *mi)).collect())
    }

    fn loaded(&self) -> Result<&StrategyAccounts> {
        self.loaded.as_deref().context("venue not loaded")
    }

    /// Classify `input_mint → output_mint` into a [`BondRoute`] — which of the
    /// three bond ops it maps to. Exposed so the Jupiter shim can pick the
    /// right `Swap` tag before building.
    ///
    /// # Errors
    /// If the venue isn't loaded, or either mint is not the receipt or a
    /// basket asset.
    pub fn route(&self, input_mint: Pubkey, output_mint: Pubkey) -> Result<BondRoute> {
        self.loaded()?.bond_route(input_mint, output_mint).map_err(|e| anyhow!("route: {e:?}"))
    }

    /// Quote `amount` of `input_mint` into `output_mint`, ExactIn.
    ///
    /// `None` is "no route": the SDK predicts a revert (supply floor / cap /
    /// a partial-fill redeem that can't absorb the full receipt burn).
    /// `amount == 0` returns `Some(zero)` before touching `loaded` - the
    /// documented zero-input contract (`bounds()` binary-searches on it;
    /// never panics, even on a cold venue).
    ///
    /// `RouteQuote.fee` is ALWAYS `0`: no bond fee is input-denominated (mint
    /// fee is receipt tokens, swap fee is USD@1e6, redeem folds fees into
    /// outputs) - a wrong-unit fee would mislead a router more than zero does.
    ///
    /// # Errors
    /// If the venue isn't loaded, or the SDK quote call fails (malformed
    /// state or an unrelated mint pair).
    pub fn quote(&self, input_mint: Pubkey, output_mint: Pubkey, amount: u64, clock: Clock) -> Result<Option<RouteQuote>> {
        if amount == 0 {
            return Ok(Some(RouteQuote { amount_out: 0, fee: 0 }));
        }
        let q = self.loaded()?.bond_quote_by_mints(input_mint, output_mint, amount, clock).map_err(|e| anyhow!("quote: {e:?}"))?;
        Ok(q.would_revert.is_none().then_some(RouteQuote { amount_out: q.amount_out, fee: 0 }))
    }

    /// Build the swap instruction. `min_out` is baked from `slippage_bps`
    /// alone - there is no redeem `headroom` parameter for bonds (that is
    /// lending-redeem-specific dust absorption).
    ///
    /// For the redeem direction the baked `min_out` is a USD@1e6 net-withdraw
    /// floor (the on-chain redeem instruction's unit), NOT out-token atoms;
    /// token-level output protection is the aggregator router's job, not this
    /// instruction's.
    ///
    /// # Errors
    /// If the venue isn't loaded, the mint pair doesn't classify, or the
    /// SDK's mint/swap/redeem build fails.
    pub fn build(&self, user: Pubkey, input_mint: Pubkey, output_mint: Pubkey, amount: u64, slippage_bps: u16, clock: Clock) -> Result<Instruction> {
        let accts = self.loaded()?;
        let route = accts.bond_route(input_mint, output_mint).map_err(|e| anyhow!("route: {e:?}"))?;
        let h = accts.bonds(user).map_err(|e| anyhow!("bonds: {e:?}"))?;
        let (_q, built) = match route {
            // usd_value = 0 is the on-chain "spend exactly the transfers" sentinel: ExactIn.
            BondRoute::Mint { in_asset_id } => h.mint(0, &[TokenInput { asset_id: in_asset_id, amount }], slippage_bps, clock).map_err(|e| anyhow!("mint: {e:?}"))?,
            BondRoute::Swap { in_asset_id, out_asset_id } => h.swap(in_asset_id, amount, out_asset_id, slippage_bps, clock).map_err(|e| anyhow!("swap: {e:?}"))?,
            // Ceiling, not a target: u64::MAX never over-allocates (the
            // allocator clamps to pool value in checked u128 math).
            BondRoute::RedeemByReceipt { out_asset_id } => {
                h.redeem_by_receipt(amount, &[TokenInput { asset_id: out_asset_id, amount: u64::MAX }], slippage_bps, clock).map_err(|e| anyhow!("redeem_by_receipt: {e:?}"))?
            },
        };
        Ok(built.instruction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_venue_has_no_mints_and_zero_quote_is_zero() {
        let v = BondVenue::from_market(Pubkey::new_unique(), Pubkey::new_unique());
        assert!(v.mints().is_empty());
        // Zero input is routable-with-zero-output, never an error, even unloaded.
        let z = v.quote(Pubkey::new_unique(), Pubkey::new_unique(), 0, Clock { slot: 0, unix: 0 }).unwrap();
        assert!(matches!(z, Some(RouteQuote { amount_out: 0, fee: 0 })));
        // A real amount on an unloaded venue is an error (nothing cached).
        assert!(v.quote(Pubkey::new_unique(), Pubkey::new_unique(), 1, Clock { slot: 0, unix: 0 }).is_err());
        assert!(v.route(Pubkey::new_unique(), Pubkey::new_unique()).is_err());
        assert!(v.build(Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), 1, 0, Clock { slot: 0, unix: 0 }).is_err());
    }

    #[test]
    fn seeds_are_controller_then_registry_multiocular_main_in_order() {
        let market = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let v = BondVenue::from_market(market, owner);
        let seeds = v.seeds();
        assert_eq!(seeds.len(), 4);
        assert_eq!(seeds[0], market);
        assert_eq!(seeds[1], pda::derive_asset_registry(&owner).0);
        assert_eq!(seeds[2], pda::derive_multiocular(&owner).0);
        assert_eq!(seeds[3], pda::derive_main(&owner).0);
    }
}
