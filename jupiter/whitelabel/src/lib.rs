//! `reflect-jupiter-whitelabel` - the Reflect whitelabel branded pair as a
//! Jupiter `Amm`.
//!
//! The vendored `Amm` trait + `Swap` tag live in `reflect-jupiter-common`,
//! shared by every `jupiter/<product>` crate. This crate is a thin shim:
//! [`ReflectWhitelabelAmm`] maps [`WhitelabelVenue`] onto Jupiter's contract
//! (a 1-seed three-phase account handshake - state, asset, legs - atomic
//! clock, metas-only swap build). All routing/protocol logic lives in `reflect-routing-core` +
//! `reflect-sdk`.

use std::sync::atomic::Ordering;

use anyhow::{bail, Result};
use solana_sdk::pubkey::Pubkey;

use reflect_jupiter_common::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, Quote, QuoteParams, Swap, SwapAndAccountMetas, SwapParams};
use reflect_routing_core::{WhitelabelRoute, WhitelabelVenue};
use reflect_sdk::common::Clock as ReflectClock;

/// [`WhitelabelVenue`] presented as a Jupiter market. One seed account (the
/// `ProxyState` itself carries both mints); its `Asset` names the oracle
/// legs, so the handshake takes one more round than lending's.
#[derive(Clone)]
pub struct ReflectWhitelabelAmm {
    venue: WhitelabelVenue,
    clock_ref: ClockRef,
    market_data: Option<Vec<u8>>,
    asset_data: Option<Vec<u8>>,
}

impl ReflectWhitelabelAmm {
    fn clock(&self) -> ReflectClock {
        ReflectClock { slot: self.clock_ref.slot.load(Ordering::Relaxed), unix: self.clock_ref.unix_timestamp.load(Ordering::Relaxed) }
    }
}

impl Amm for ReflectWhitelabelAmm {
    fn from_keyed_account(ka: &KeyedAccount, ctx: &AmmContext) -> Result<Self> {
        // The listed account's owner selects the deployment (mainnet or
        // staging proxy); `venue.load` re-checks it against the fetched bytes.
        Ok(Self { venue: WhitelabelVenue::from_market(ka.account.owner, ka.key), clock_ref: ctx.clock_ref.clone(), market_data: Some(ka.account.data.clone()), asset_data: None })
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
        true // cold start returns the seed; re-called after update for the full set
    }

    /// Three-phase handshake: cold returns the seed; with the state bytes
    /// cached, the seed plus its `Asset`; with both cached, the full set. On
    /// derive failure fall back a round rather than panic.
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let seeds = self.venue.seeds();
        match (&self.market_data, &self.asset_data) {
            (Some(market), Some(asset)) => self.venue.discovery_keys(market, asset).unwrap_or_else(|_| seeds.clone()),
            (Some(market), None) => self.venue.asset_key(market).map_or_else(|_| seeds.clone(), |a| vec![seeds[0], a]),
            (None, _) => seeds, // cold: [market]
        }
    }

    fn update(&mut self, m: &AccountMap) -> Result<()> {
        let seeds = self.venue.seeds();

        self.market_data = m.get(&seeds[0]).map(|a| a.data.clone());
        let Some(market) = &self.market_data else {
            return Ok(()); // cold: seed not in the map yet
        };
        // A present-but-undecodable seed is an error, not "still cold".
        self.asset_data = m.get(&self.venue.asset_key(market)?).map(|a| a.data.clone());
        let Some(asset) = &self.asset_data else {
            return Ok(()); // round 2 pending: the asset is not in the map yet
        };

        let keys = self.venue.discovery_keys(market, asset)?;
        self.venue.load(keys.iter().filter_map(|k| m.get(k).map(|a| (*k, a.owner, a.data.clone()))))
    }

    fn quote(&self, p: &QuoteParams) -> Result<Quote> {
        match self.venue.quote(p.input_mint, p.amount, self.clock())? {
            // The crank fee is folded into pricing, never a scalar - a
            // wrong-unit fee would mislead Jupiter's router more than zero.
            Some(q) => Ok(Quote { in_amount: p.amount, out_amount: q.amount_out, fee_amount: 0, fee_mint: p.input_mint }),
            None => bail!("reflect would revert: no route > bad route"),
        }
    }

    fn get_swap_and_account_metas(&self, sp: &SwapParams) -> Result<SwapAndAccountMetas> {
        // Jupiter takes ONLY the account list: its on-chain program writes
        // the instruction data (amount, min_out) from the route context, so
        // build with slippage 0 - the serialized data is discarded.
        let route = self.venue.route(sp.source_mint)?;
        let ix = self.venue.build(sp.token_transfer_authority, sp.source_mint, sp.in_amount, 0, self.clock())?;

        // Wrap and unwrap share ONE account-meta order (the proxy program's
        // ix.rs), so the tag is the ONLY direction carrier - unlike
        // lending's single permissioned variant where order encodes it.
        let swap = match route {
            WhitelabelRoute::Wrap => Swap::ReflectProxyWrap,
            WhitelabelRoute::Unwrap => Swap::ReflectProxyUnwrap,
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
    use reflect_jupiter_common::SwapMode;
    use solana_sdk::account::Account;

    fn keyed(owner: Pubkey) -> KeyedAccount {
        KeyedAccount { key: Pubkey::new_unique(), account: Account { lamports: 1, data: vec![0u8; 8], owner, executable: false, rent_epoch: 0 }, params: None }
    }

    // A cold Amm exposes no reserve mints, advertises its dynamic-account
    // contract, ExactIn only, and a real quote on it errors (nothing loaded).
    #[test]
    fn cold_amm_has_no_reserve_mints_and_quote_errs() {
        let ctx = AmmContext { clock_ref: ClockRef::default() };
        let amm = ReflectWhitelabelAmm::from_keyed_account(&keyed(Pubkey::new_unique()), &ctx).expect("from_keyed_account");
        assert!(amm.get_reserve_mints().is_empty());
        assert!(amm.requires_update_for_reserve_mints());
        assert!(amm.has_dynamic_accounts());
        assert!(!amm.supports_exact_out());
        assert!(amm.quote(&QuoteParams { amount: 1, input_mint: Pubkey::new_unique(), output_mint: Pubkey::new_unique(), swap_mode: SwapMode::ExactIn }).is_err());
    }

    // Cold `get_accounts_to_update` is exactly the single market seed.
    #[test]
    fn cold_accounts_to_update_is_the_single_seed() {
        let ctx = AmmContext { clock_ref: ClockRef::default() };
        let owner = Pubkey::new_unique();
        let amm = ReflectWhitelabelAmm::from_keyed_account(&keyed(owner), &ctx).expect("from_keyed_account");
        assert_eq!(amm.get_accounts_to_update(), amm.venue.seeds());
        assert_eq!(amm.get_accounts_to_update().len(), 1);
    }
}
