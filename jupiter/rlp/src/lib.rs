//! `reflect-jupiter-rlp` - the Reflect rlp insurance pool as a Jupiter `Amm`.
//!
//! The vendored `Amm` trait + `Swap` tag live in `reflect-jupiter-common`,
//! which also carries `KeyedAccount.params` (per-market config Jupiter
//! supplies at listing). For rlp markets that is
//! `{"asset_mints": ["<base58>", ...]}`, the pool's asset mints (the pool
//! lists registry indices and the `Asset` PDA is keyed by mint; see
//! [`RlpVenue`] docs). This crate is a thin shim:
//! [`ReflectRlpAmm`] maps [`RlpVenue`] onto Jupiter's contract (a
//! runtime-sized two-phase account handshake, atomic clock, metas-only swap
//! build). All routing/protocol logic lives in `reflect-routing-core` +
//! `reflect-sdk`.

use std::sync::atomic::Ordering;

use anyhow::{bail, Context, Result};
use solana_sdk::pubkey::Pubkey;

use reflect_jupiter_common::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, Quote, QuoteParams, Swap, SwapAndAccountMetas, SwapParams};
use reflect_routing_core::{RlpRoute, RlpVenue};
use reflect_sdk::common::Now as ReflectClock;

/// [`RlpVenue`] presented as a Jupiter market. The seed count scales with the
/// pool's asset count (2 + N: pool, settings, N asset PDAs), so the cached
/// seed bytes are a runtime-sized `Vec`, not a fixed array.
#[derive(Clone)]
pub struct ReflectRlpAmm {
    venue: RlpVenue,
    clock_ref: ClockRef,
    seed_data: Vec<Option<Vec<u8>>>,
}

impl ReflectRlpAmm {
    fn clock(&self) -> ReflectClock {
        ReflectClock { slot: self.clock_ref.slot.load(Ordering::Relaxed), unix_timestamp: self.clock_ref.unix_timestamp.load(Ordering::Relaxed) }
    }
}

impl Amm for ReflectRlpAmm {
    fn from_keyed_account(ka: &KeyedAccount, ctx: &AmmContext) -> Result<Self> {
        // The listed account's owner selects the deployment (mainnet or
        // staging rlp); RlpVenue::load re-checks it against the fetched bytes.
        let mints: Vec<Pubkey> = ka
            .params
            .as_ref()
            .and_then(|p| p.get("asset_mints"))
            .and_then(|v| v.as_array())
            .context("rlp market listing must carry params.asset_mints (see crate docs)")?
            .iter()
            .map(|v| -> Result<Pubkey> { Ok(v.as_str().context("asset mint must be a base58 string")?.parse()?) })
            .collect::<Result<_>>()?;
        let venue = RlpVenue::from_market(ka.account.owner, ka.key, mints);
        let seed_data = vec![None; venue.seeds().len()];
        Ok(Self { venue, clock_ref: ctx.clock_ref.clone(), seed_data })
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
        // [lp, public asset_0..n]; LP-input pairs are not routable (the
        // cooldown withdrawal) - quote() surfaces that as an Err.
        self.venue.mints().iter().map(|m| m.pubkey).collect()
    }

    fn requires_update_for_reserve_mints(&self) -> bool {
        true // the mints are known only after update()
    }

    fn has_dynamic_accounts(&self) -> bool {
        true // cold start returns seeds; re-called after update for the full set
    }

    /// Two-phase handshake: cold returns the seeds; once `update` has cached
    /// their bytes, the venue derives the full set. On derive failure fall
    /// back to the seeds (re-cold) rather than panic.
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let seeds = self.venue.seeds();
        if self.seed_data.iter().all(Option::is_some) {
            let asset_bytes: Vec<&[u8]> = self.seed_data[2..].iter().map(|d| d.as_deref().unwrap()).collect();
            self.venue.discovery_keys(self.seed_data[0].as_deref().unwrap(), &asset_bytes).unwrap_or_else(|_| seeds.clone())
        } else {
            seeds // cold: [pool, settings, asset_pda_0..n-1]
        }
    }

    fn update(&mut self, m: &AccountMap) -> Result<()> {
        let seeds = self.venue.seeds();

        for (slot, seed) in self.seed_data.iter_mut().zip(&seeds) {
            *slot = m.get(seed).map(|a| a.data.clone());
        }
        if !self.seed_data.iter().all(Option::is_some) {
            return Ok(()); // cold: seeds not in the map yet
        }

        // A present-but-undecodable seed is an error, not "still cold".
        let asset_bytes: Vec<&[u8]> = self.seed_data[2..].iter().map(|d| d.as_deref().unwrap()).collect();
        let keys = self.venue.discovery_keys(self.seed_data[0].as_deref().unwrap(), &asset_bytes)?;
        self.venue.load(keys.iter().filter_map(|k| m.get(k).map(|a| (*k, a.owner, a.data.clone()))))
    }

    fn quote(&self, p: &QuoteParams) -> Result<Quote> {
        match self.venue.quote(p.input_mint, p.output_mint, p.amount, self.clock())? {
            // The swap fee is taken from the OUTPUT (`swap_fee_bps` after price
            // impact); deposits charge none.
            Some(q) => Ok(Quote { in_amount: p.amount, out_amount: q.amount_out, fee_amount: q.fee, fee_mint: p.output_mint }),
            None => bail!("reflect would revert: no route > bad route"),
        }
    }

    fn get_swap_and_account_metas(&self, sp: &SwapParams) -> Result<SwapAndAccountMetas> {
        // Jupiter takes ONLY the account list: its on-chain program writes
        // the instruction data (amount, min_out) from the route context, so
        // build with slippage 0 - the serialized data is discarded.
        let route = self.venue.route(sp.source_mint, sp.destination_mint)?;
        let ix = self.venue.build(sp.token_transfer_authority, sp.source_mint, sp.destination_mint, sp.in_amount, 0, self.clock())?;
        let swap = match route {
            RlpRoute::Deposit => Swap::ReflectRlpDeposit,
            RlpRoute::Swap => Swap::ReflectRlpSwap,
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

    fn keyed_with_params(params: Option<serde_json::Value>) -> KeyedAccount {
        KeyedAccount { key: Pubkey::new_unique(), account: Account { lamports: 1, data: vec![0u8; 8], owner: Pubkey::new_unique(), executable: false, rent_epoch: 0 }, params }
    }

    #[test]
    fn from_keyed_account_requires_params() {
        let ctx = AmmContext { clock_ref: ClockRef::default() };
        assert!(ReflectRlpAmm::from_keyed_account(&keyed_with_params(None), &ctx).is_err());
    }

    #[test]
    fn params_parse_roundtrip() {
        let ctx = AmmContext { clock_ref: ClockRef::default() };
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let params = serde_json::json!({ "asset_mints": [a.to_string(), b.to_string()] });
        let amm = ReflectRlpAmm::from_keyed_account(&keyed_with_params(Some(params)), &ctx).expect("from_keyed_account");
        assert_eq!(amm.venue.asset_mints(), &[a, b]);
        assert_eq!(amm.get_accounts_to_update().len(), 4); // pool + settings + 2 asset pdas
    }

    // A cold Amm exposes no reserve mints, advertises its dynamic-account
    // contract, ExactIn only, and a real quote on it errors (nothing loaded).
    #[test]
    fn cold_amm_has_no_reserve_mints_and_quote_errs() {
        let ctx = AmmContext { clock_ref: ClockRef::default() };
        let params = serde_json::json!({ "asset_mints": [Pubkey::new_unique().to_string()] });
        let amm = ReflectRlpAmm::from_keyed_account(&keyed_with_params(Some(params)), &ctx).expect("from_keyed_account");
        assert!(amm.get_reserve_mints().is_empty());
        assert!(amm.requires_update_for_reserve_mints());
        assert!(amm.has_dynamic_accounts());
        assert!(!amm.supports_exact_out());
        assert!(amm.quote(&QuoteParams { amount: 1, input_mint: Pubkey::new_unique(), output_mint: Pubkey::new_unique(), swap_mode: SwapMode::ExactIn }).is_err());
    }
}
