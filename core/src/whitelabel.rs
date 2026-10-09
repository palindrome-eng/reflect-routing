//! Reflect **whitelabel** routing: the account plumbing + SDK delegation
//! shared by the Jupiter and Titan shims.
//!
//! The SDK quote applies the program's gates in handler order (zero amounts,
//! oracle legs, deposit cap, vault undercollateralisation): its `Err` IS the
//! predicted revert, so the venue maps it to "no route". `ProxyState.frozen`
//! freezes the integrator FEE, not trading, and is not a gate.
//!
//! Listing contract: the market's `Asset` must be active and its oracle chain
//! resolved; an inactive or never-resolved asset fails [`WhitelabelVenue::load`].
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::Now;
use reflect_sdk::proxy::{asset_key, state_keys, AssetView, FetchedAccount, ProxyStateView, WhitelabelAccounts};
use reflect_sdk::strategy::MintInfo;

use crate::{spl_mint_info, RouteQuote};

/// Which branded-pair op an input mint maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhitelabelRoute {
    /// stablecoin -> branded: share mint at the cranked price.
    Wrap,
    /// branded -> stablecoin: share burn at the cranked price.
    Unwrap,
}

/// One branded market (a `ProxyState` PDA) on one proxy deployment.
/// All protocol logic is `reflect-sdk`'s; the built [`WhitelabelAccounts`] is
/// cached behind an `Arc` once per [`load`](Self::load), so quote/build reuse
/// it and `Clone` stays cheap.
///
/// Load is three-phase, because the oracle leg keys live in the `Asset` the
/// state names: [`seeds`](Self::seeds) -> fetch -> [`asset_key`](Self::asset_key)
/// -> fetch -> [`discovery_keys`](Self::discovery_keys) -> fetch the full set
/// -> [`load`](Self::load).
#[derive(Clone)]
pub struct WhitelabelVenue {
    program_id: Pubkey,
    market: Pubkey,
    loaded: Option<Arc<WhitelabelAccounts>>,
    mints: Option<[MintInfo; 2]>, // [stablecoin (idx 0), branded (idx 1)]
}

impl WhitelabelVenue {
    /// Construct cold from the `ProxyState` account the aggregator lists:
    /// `market` is its key, `program_id` its owner (mainnet or staging proxy).
    pub fn from_market(program_id: Pubkey, market: Pubkey) -> Self {
        Self { program_id, market, loaded: None, mints: None }
    }

    /// The owning proxy program id.
    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    /// The market/pool key (the `ProxyState` PDA).
    pub fn market(&self) -> Pubkey {
        self.market
    }

    /// One seed: the `ProxyState` itself carries both mints.
    pub fn seeds(&self) -> Vec<Pubkey> {
        vec![self.market]
    }

    /// Round 2: the `Asset` PDA of the state's stablecoin.
    ///
    /// # Errors
    /// If the bytes are not a valid `ProxyState`.
    pub fn asset_key(&self, proxy_state: &[u8]) -> Result<Pubkey> {
        let state = ProxyStateView::from_bytes(proxy_state).map_err(|e| anyhow!("proxy_state: {e:?}"))?;
        Ok(asset_key(&state, &self.program_id))
    }

    /// Round 3: the full fetch set - the market, its asset, the vault, both
    /// mints and every oracle leg account.
    ///
    /// # Errors
    /// If either byte slice fails to decode.
    pub fn discovery_keys(&self, proxy_state: &[u8], asset: &[u8]) -> Result<Vec<Pubkey>> {
        let state = ProxyStateView::from_bytes(proxy_state).map_err(|e| anyhow!("proxy_state: {e:?}"))?;
        let av = AssetView::from_bytes(asset).map_err(|e| anyhow!("asset: {e:?}"))?;
        let mut keys = vec![self.market, asset_key(&state, &self.program_id), Pubkey::new_from_array(state.stablecoin_mint())];
        keys.extend(state_keys(&state, &self.market, &av).map_err(|e| anyhow!("state_keys: {e:?}"))?);
        Ok(keys)
    }

    /// Ingest the fetched `(key, owner, data)` set: verify ownership, gate on
    /// the asset, build the SDK loader once, resolve both mints' decimals.
    ///
    /// # Errors
    /// If the market account is missing or not owned by `program_id`, the
    /// asset is missing, inactive or never resolved, or either mint account is
    /// missing/short.
    pub fn load(&mut self, accounts: impl IntoIterator<Item = (Pubkey, Pubkey, Vec<u8>)>) -> Result<()> {
        let fetched: Vec<(Pubkey, Pubkey, Vec<u8>)> = accounts.into_iter().collect();
        let find = |k: Pubkey| fetched.iter().find(|(fk, _, _)| *fk == k);
        let (_, owner, state) = find(self.market).context("market account missing")?;
        if *owner != self.program_id {
            bail!("market owner {owner} is not the proxy program {}", self.program_id);
        }
        let sv = ProxyStateView::from_bytes(state).map_err(|e| anyhow!("proxy_state: {e:?}"))?;
        let branded = Pubkey::new_from_array(sv.branded_mint());
        let stablecoin = Pubkey::new_from_array(sv.stablecoin_mint());

        let asset = &find(asset_key(&sv, &self.program_id)).context("asset account missing")?.2;
        let av = AssetView::from_bytes(asset).map_err(|e| anyhow!("asset: {e:?}"))?;
        if !av.active() {
            bail!("asset for {stablecoin} is inactive; not routable");
        }
        if !av.is_chain_resolved() {
            bail!("asset for {stablecoin} has an unresolved oracle chain; not routable");
        }

        let bytes = fetched.iter().map(|(k, o, d)| FetchedAccount { key: *k, owner: *o, data: d.clone() }).collect();
        let accts = WhitelabelAccounts::from_fetched(self.program_id, branded, bytes).map_err(|e| anyhow!("from_fetched: {e:?}"))?;

        self.mints = Some([spl_mint_info(stablecoin, &fetched)?, spl_mint_info(branded, &fetched)?]);
        self.loaded = Some(Arc::new(accts));
        Ok(())
    }

    /// The mints this venue trades (empty until loaded): `[stablecoin,
    /// branded]` - deposit-then-receipt order, mirroring lending's
    /// `[deposit, receipt]`.
    pub fn mints(&self) -> Vec<MintInfo> {
        self.mints.as_ref().map_or(vec![], |m| m.to_vec())
    }

    fn loaded(&self) -> Result<&WhitelabelAccounts> {
        self.loaded.as_deref().context("venue not loaded")
    }

    /// Classify the input mint.
    pub fn route(&self, input_mint: Pubkey) -> Result<WhitelabelRoute> {
        let m = self.mints.as_ref().context("venue not loaded")?;
        if input_mint == m[0].pubkey {
            Ok(WhitelabelRoute::Wrap)
        } else if input_mint == m[1].pubkey {
            Ok(WhitelabelRoute::Unwrap)
        } else {
            Err(anyhow!("mint {input_mint} is not in this market's pair"))
        }
    }

    /// Quote `amount` of `input_mint` into its counterpart, ExactIn. `None` is
    /// "no route": the SDK predicts the revert the program would raise.
    /// `amount == 0` returns `Some(zero)` before touching state.
    ///
    /// `fee` is ALWAYS 0: the crank folds the integrator fee into principal
    /// and commission, never into a per-trade scalar.
    ///
    /// # Errors
    /// If the venue isn't loaded or the mint isn't in the pair.
    pub fn quote(&self, input_mint: Pubkey, amount: u64, clock: Now) -> Result<Option<RouteQuote>> {
        if amount == 0 {
            return Ok(Some(RouteQuote { amount_out: 0, fee: 0 }));
        }
        let accts = self.loaded()?;
        self.route(input_mint)?;
        Ok(accts.quote_by_mints(input_mint, amount, clock.into()).ok().map(|q| RouteQuote { amount_out: q.amount_out, fee: 0 }))
    }

    /// Build the wrap/unwrap instruction; `min_out` is baked from
    /// `slippage_bps` (floored at 1: the program rejects a zero minimum).
    ///
    /// # Errors
    /// If the venue isn't loaded, the mint isn't in the pair, or the SDK
    /// refuses the quote the build embeds.
    pub fn build(&self, user: Pubkey, input_mint: Pubkey, amount: u64, slippage_bps: u16, clock: Now) -> Result<Instruction> {
        let h = self.loaded()?.whitelabel(user);
        let (_q, built) = match self.route(input_mint)? {
            WhitelabelRoute::Wrap => h.wrap(amount, slippage_bps, clock.into()).map_err(|e| anyhow!("wrap: {e:?}"))?,
            WhitelabelRoute::Unwrap => h.unwrap(amount, slippage_bps, clock.into()).map_err(|e| anyhow!("unwrap: {e:?}"))?,
        };
        Ok(built.instruction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_sdk::proxy::ids::{PROXY_PROGRAM, PYTH_RECEIVER};
    use reflect_sdk::proxy::pda;

    const NOW: Now = Now { slot: 500_000, unix_timestamp: 1_800_000_000 };

    fn pk(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    // ProxyState (156 B): branded(32) stablecoin(32) fee u16@64 principal u64@66
    // commission u64@74 authority(32)@82 bump@114 frozen@115 deposit_cap u64@116 pending(32)@124.
    fn proxy_state_buf(branded: &Pubkey, stablecoin: &Pubkey, fee: u16, principal: u64, commission: u64, bump: u8) -> Vec<u8> {
        let mut b = vec![0u8; 156];
        b[0..32].copy_from_slice(branded.as_ref());
        b[32..64].copy_from_slice(stablecoin.as_ref());
        b[64..66].copy_from_slice(&fee.to_le_bytes());
        b[66..74].copy_from_slice(&principal.to_le_bytes());
        b[74..82].copy_from_slice(&commission.to_le_bytes());
        b[114] = bump;
        b
    }
    // Asset, prod layout (98 B): stablecoin(32) oracle(32)@32 active@64 bump@65 feed_id(32)@66.
    fn asset_buf(stablecoin: &Pubkey, oracle: &Pubkey, active: bool) -> Vec<u8> {
        let mut b = vec![0u8; 98];
        b[0..32].copy_from_slice(stablecoin.as_ref());
        b[32..64].copy_from_slice(oracle.as_ref());
        b[64] = active as u8;
        b[66..98].copy_from_slice(oracle.as_ref()); // feed_id == oracle key, matched below
        b
    }
    fn mint_buf(supply: u64, decimals: u8) -> Vec<u8> {
        let mut b = vec![0u8; 82];
        b[36..44].copy_from_slice(&supply.to_le_bytes());
        b[44] = decimals;
        b
    }
    fn token_buf(amount: u64) -> Vec<u8> {
        let mut b = vec![0u8; 165];
        b[64..72].copy_from_slice(&amount.to_le_bytes());
        b[108] = 1;
        b
    }
    // Pyth PriceUpdateV2 (134 B), Full, feed_id == the account key, conf 0.1%, posted 30 slots ago.
    fn pyth_buf(key: &Pubkey, price: i64, exponent: i32) -> Vec<u8> {
        let mut b = vec![0u8; 134];
        b[0..8].copy_from_slice(&[34, 241, 35, 99, 157, 126, 244, 205]);
        b[40] = 1;
        b[41..73].copy_from_slice(key.as_ref());
        b[73..81].copy_from_slice(&price.to_le_bytes());
        b[81..89].copy_from_slice(&(price as u64 / 1_000).to_le_bytes());
        b[89..93].copy_from_slice(&exponent.to_le_bytes());
        b[93..101].copy_from_slice(&(NOW.unix_timestamp - 30).to_le_bytes());
        b[125..133].copy_from_slice(&(NOW.slot - 30).to_le_bytes());
        b
    }

    type FetchedAccounts = Vec<(Pubkey, Pubkey, Vec<u8>)>;

    /// A branded market at par ($1, principal == supply == vault) at the CANONICAL PDA for `branded`.
    fn genesis(branded: Pubkey, active: bool) -> (Pubkey, FetchedAccounts) {
        let pid = &PROXY_PROGRAM;
        let stablecoin = pk(99);
        let oracle = pk(50);
        let (proxy_state, bump) = pda::derive_proxy_state(&branded, pid);
        let accounts = vec![
            (proxy_state, PROXY_PROGRAM, proxy_state_buf(&branded, &stablecoin, 1_000, 900_000_000, 0, bump)),
            (pda::derive_asset(&stablecoin, pid).0, PROXY_PROGRAM, asset_buf(&stablecoin, &oracle, active)),
            (stablecoin, pk(202), mint_buf(0, 6)),
            (pda::proxy_state_vault(&proxy_state, &stablecoin), pk(201), token_buf(900_000_000)),
            (branded, pk(202), mint_buf(900_000_000, 6)),
            (oracle, PYTH_RECEIVER, pyth_buf(&oracle, 100_000_000, -8)),
        ];
        (proxy_state, accounts)
    }

    fn loaded(proxy_state: Pubkey, accounts: FetchedAccounts) -> Result<WhitelabelVenue> {
        let mut v = WhitelabelVenue::from_market(PROXY_PROGRAM, proxy_state);
        let asset = v.asset_key(&accounts[0].2)?;
        assert_eq!(asset, accounts[1].0);
        let keys = v.discovery_keys(&accounts[0].2, &accounts[1].2)?;
        for (k, _, _) in &accounts {
            assert!(keys.contains(k), "{k} missing from discovery");
        }
        v.load(keys.iter().filter_map(|k| accounts.iter().find(|(fk, _, _)| fk == k).cloned()))?;
        Ok(v)
    }

    #[test]
    fn cold_venue_has_no_mints_and_zero_quote_is_zero() {
        let v = WhitelabelVenue::from_market(PROXY_PROGRAM, Pubkey::new_unique());
        assert!(v.mints().is_empty());
        let z = v.quote(Pubkey::new_unique(), 0, NOW).unwrap();
        assert!(matches!(z, Some(RouteQuote { amount_out: 0, .. })));
        assert!(v.quote(Pubkey::new_unique(), 1, NOW).is_err());
        assert!(v.route(Pubkey::new_unique()).is_err());
        assert!(v.build(Pubkey::new_unique(), Pubkey::new_unique(), 1, 0, NOW).is_err());
        assert_eq!(v.seeds(), vec![v.market()]);
    }

    #[test]
    fn load_rejects_wrong_owner() {
        let (proxy_state, mut accounts) = genesis(pk(7), true);
        accounts[0].1 = Pubkey::new_unique();
        assert!(loaded(proxy_state, accounts).is_err());
    }

    #[test]
    fn load_rejects_market_not_at_canonical_pda() {
        let (_, mut accounts) = genesis(pk(7), true);
        let bogus = Pubkey::new_unique();
        accounts[0].0 = bogus;
        let mut v = WhitelabelVenue::from_market(PROXY_PROGRAM, bogus);
        assert!(v.load(accounts).is_err());
    }

    #[test]
    fn load_rejects_inactive_asset() {
        let (proxy_state, accounts) = genesis(pk(7), false);
        assert!(loaded(proxy_state, accounts).is_err());
    }

    #[test]
    fn loaded_venue_quotes_and_builds_both_directions() {
        let branded = pk(7);
        let (proxy_state, accounts) = genesis(branded, true);
        let stablecoin = accounts[2].0;
        let v = loaded(proxy_state, accounts).unwrap();
        assert_eq!(v.mints().len(), 2);
        assert_eq!(v.route(stablecoin).unwrap(), WhitelabelRoute::Wrap);
        assert_eq!(v.route(branded).unwrap(), WhitelabelRoute::Unwrap);

        // At par with virtual offsets 10^6: 1 USDC wraps to ~1 branded, and back.
        let q = v.quote(stablecoin, 1_000_000, NOW).unwrap().unwrap();
        assert!((999_990..=1_000_000).contains(&q.amount_out), "{}", q.amount_out);
        assert_eq!(q.fee, 0);
        let q = v.quote(branded, 1_000_000, NOW).unwrap().unwrap();
        assert!((999_990..=1_000_000).contains(&q.amount_out), "{}", q.amount_out);

        let ix = v.build(Pubkey::new_unique(), stablecoin, 1_000_000, 50, NOW).unwrap();
        assert_eq!(ix.program_id, PROXY_PROGRAM);
        assert_eq!(ix.accounts.len(), 10);
        let ix = v.build(Pubkey::new_unique(), branded, 1_000_000, 50, NOW).unwrap();
        assert_eq!(ix.accounts.len(), 10);
    }

    #[test]
    fn stale_oracle_is_no_route_not_an_error() {
        let (proxy_state, accounts) = genesis(pk(7), true);
        let stablecoin = accounts[2].0;
        let v = loaded(proxy_state, accounts).unwrap();
        // Posted 30 slots before NOW; the prod build's window is 200 slots.
        assert!(v.quote(stablecoin, 1_000_000, Now { slot: NOW.slot + 200, ..NOW }).unwrap().is_none());
    }
}
