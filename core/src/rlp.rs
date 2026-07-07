//! Reflect **rlp** (insurance pool) routing: account plumbing + SDK delegation
//! for restake (asset->LP) and swap (asset->asset).
//!
//! The venue owns what the SDK's thin quote deliberately skips: killswitch, swap
//! inventory, and asset access levels - so quote == execution stays exact.
//! LP->asset is the two-transaction cooldown withdrawal: NOT routable.
//!
//! # Known limitation: restake may be role-gated
//! Unlike swap (confirmed public by default - `PublicSwap` is assigned
//! `Role::PUBLIC` in the program's `AccessControl::new_defaults()`), `restake`
//! is gated by `Action::Restake`, which defaults to `Role::MANAGER` /
//! `Role::TESTEE` only (reflect-program-library/rlp
//! `programs/rlp/src/states/access.rs` `new_defaults()`,
//! `instructions/user/restake.rs` `action_check_protocol`). Whether a given
//! deployment's `Settings` marks `Restake` public is stored in the on-chain
//! `AccessMap`, which the SDK does not currently decode (`SettingsView` skips
//! straight from the count fields to the killswitch byte). This venue builds
//! restake with `permissions: None` (the SDK handle's own default) since that
//! is the only value an anonymous aggregator user can supply; if the live
//! deployment has NOT marked `Restake` public, the built instruction reverts
//! on submission (a caught simulation failure, not a fund-loss risk) rather
//! than silently misrouting. Revisit once the SDK exposes an
//! `is_public_action` accessor.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::Clock;
use reflect_sdk::rlp::{decode_asset_mint_and_oracle, ids, pda, pool_discovery_keys, Action, AssetView, CoreError, LiquidityPoolView, OracleRef, PoolAccounts, SettingsView};
use reflect_sdk::strategy::MintInfo;

use crate::{spl_mint_info, RouteQuote};

/// Which routable rlp op a mint pair maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlpRoute {
    /// asset -> LP token (deposit against pool USD value).
    Restake,
    /// asset -> asset at the oracle cross-rate.
    Swap,
}

/// The one Reflect insurance pool this market wraps.
#[derive(Clone)]
pub struct RlpVenue {
    market: Pubkey,
    asset_mints: Vec<Pubkey>,
    loaded: Option<Arc<PoolAccounts>>,
    restake_frozen: bool,       // killswitch bit for Action::Restake, parsed ONCE at load
    swap_frozen: bool,          // Settings.frozen() OR killswitch bit for Action::PublicSwap
    vaults: Vec<(Pubkey, u64)>, // (asset mint, pool vault balance): swap inventory guard
    mints: Vec<MintInfo>,       // [lp (idx 0), public asset_0..n] - mints[1..] IS the public set
}

impl RlpVenue {
    /// Construct cold from the `LiquidityPool` account the aggregator lists,
    /// plus the registered asset mints.
    pub fn from_market(market: Pubkey, asset_mints: Vec<Pubkey>) -> Self {
        Self { market, asset_mints, loaded: None, restake_frozen: false, swap_frozen: false, vaults: vec![], mints: vec![] }
    }

    /// THE rlp program_id.
    pub fn program_id(&self) -> Pubkey {
        ids::RLP_PROGRAM
    }

    /// The market/pool key (the `LiquidityPool` PDA).
    pub fn market(&self) -> Pubkey {
        self.market
    }

    /// The injected registered-asset mints (listing order).
    pub fn asset_mints(&self) -> &[Pubkey] {
        &self.asset_mints
    }

    /// Round-1 fetch set, FIXED order the shims rely on:
    /// `[pool, settings, asset_pda_0.., mint_0..]`. Asset PDAs and mint
    /// accounts (decimals) derive from the injected mints alone.
    pub fn seeds(&self) -> Vec<Pubkey> {
        let mut s = vec![self.market, pda::derive_settings().0];
        s.extend(self.asset_mints.iter().map(|m| pda::derive_asset(m).0));
        s.extend(self.asset_mints.iter().copied());
        s
    }

    /// Given the round-1 bytes (pool, settings, one slice per Asset PDA in
    /// seed order), the full fetch set. Decode errors SURFACE (each view's
    /// `from_bytes` validates its own discriminator).
    ///
    /// # Errors
    /// Wrong-PDA market; a registered-asset count that no longer matches the
    /// injected list (admin add/remove since listing - re-scan and re-list);
    /// an Asset account whose mint differs from the injected mint at that slot.
    pub fn discovery_keys(&self, pool: &[u8], settings: &[u8], assets: &[&[u8]]) -> Result<Vec<Pubkey>> {
        let pv = LiquidityPoolView::from_bytes(pool).map_err(|e| anyhow!("pool: {e:?}"))?;
        let index = pv.index().map_err(|e| anyhow!("pool.index: {e:?}"))?;
        if pda::derive_liquidity_pool(index).0 != self.market {
            bail!("account is not the canonical liquidity_pool PDA for its index");
        }
        let sv = SettingsView::from_bytes(settings).map_err(|e| anyhow!("settings: {e:?}"))?;
        let registered = sv.assets().map_err(|e| anyhow!("settings.assets: {e:?}"))? as usize;
        if registered != self.asset_mints.len() {
            bail!("registered asset count ({registered}) != injected list ({}); re-scan and re-list", self.asset_mints.len());
        }
        let lp_token = Pubkey::new_from_array(pv.lp_token().map_err(|e| anyhow!("pool.lp_token: {e:?}"))?);
        let mut oracle_refs = Vec::with_capacity(assets.len());
        for (i, bytes) in assets.iter().enumerate() {
            let (mint, oracle) = decode_asset_mint_and_oracle(bytes).map_err(|e| anyhow!("asset[{i}]: {e:?}"))?;
            if Pubkey::new_from_array(mint) != self.asset_mints[i] {
                bail!("asset[{i}] mint mismatch vs injected list");
            }
            oracle_refs.push((self.asset_mints[i], oracle));
        }
        // pool_discovery_keys = dedup([pool, lp_token] ++ per-asset [vault_ata, oracle]);
        // its ONLY overlap with the seed set is the pool key itself.
        let mut keys = self.seeds();
        keys.extend(pool_discovery_keys(index, lp_token, &oracle_refs).into_iter().filter(|k| *k != self.market));
        Ok(keys)
    }

    /// Ingest the fetched set. The round-2 guards (PDA, count, mint match)
    /// already ran in [`discovery_keys`](Self::discovery_keys) - both shims
    /// call it before load - so load only verifies ownership and decodes
    /// what it caches.
    ///
    /// # Errors
    /// If the pool account is missing/not rlp-owned, the settings account is
    /// missing, an asset account is missing, or fewer than one public asset
    /// resolves (an all-private or empty basket has no routable mints).
    pub fn load(&mut self, accounts: impl IntoIterator<Item = (Pubkey, Pubkey, Vec<u8>)>) -> Result<()> {
        let fetched: Vec<(Pubkey, Pubkey, Vec<u8>)> = accounts.into_iter().collect();
        let find = |k: Pubkey| fetched.iter().find(|(fk, _, _)| *fk == k);
        let (_, owner, pool) = find(self.market).context("pool account missing")?;
        if *owner != ids::RLP_PROGRAM {
            bail!("market owner {owner} is not the rlp program");
        }
        let pv = LiquidityPoolView::from_bytes(pool).map_err(|e| anyhow!("pool: {e:?}"))?;
        let index = pv.index().map_err(|e| anyhow!("index: {e:?}"))?;
        let lp_token = Pubkey::new_from_array(pv.lp_token().map_err(|e| anyhow!("lp_token: {e:?}"))?);

        // Killswitch flags, parsed ONCE here: the account only changes when
        // the venue reloads, so quote() checks two bools instead of
        // reparsing bytes on every call.
        let sv_bytes = &find(pda::derive_settings().0).context("settings missing")?.2;
        let sv = SettingsView::from_bytes(sv_bytes).map_err(|e| anyhow!("settings: {e:?}"))?;
        self.restake_frozen = sv.is_action_frozen(Action::Restake).map_err(|e| anyhow!("killswitch: {e:?}"))?;
        self.swap_frozen = sv.frozen().map_err(|e| anyhow!("frozen: {e:?}"))? || sv.is_action_frozen(Action::PublicSwap).map_err(|e| anyhow!("killswitch: {e:?}"))?;

        // Full asset set into the SDK loader (pool valuation covers EVERY
        // asset); only Public assets become routable mints (private needs a
        // permission account an aggregator user cannot have).
        let mut all_assets: Vec<(Pubkey, OracleRef)> = Vec::new();
        let mut public: Vec<Pubkey> = Vec::new();
        for (i, mint) in self.asset_mints.iter().enumerate() {
            let bytes = &find(pda::derive_asset(mint).0).context("asset account missing")?.2;
            let av = AssetView::from_bytes(bytes).map_err(|e| anyhow!("asset[{i}]: {e:?}"))?;
            all_assets.push((*mint, av.oracle().map_err(|e| anyhow!("asset[{i}].oracle: {e:?}"))?));
            if av.is_public().map_err(|e| anyhow!("asset[{i}].is_public: {e:?}"))? {
                public.push(*mint);
            }
        }
        let accts = PoolAccounts::from_account_map(index, all_assets, fetched.iter().map(|(k, _, d)| (*k, d.clone()))).map_err(|e| anyhow!("from_account_map: {e:?}"))?;

        // Vault balances (SPL TokenAccount amount @64) for the swap inventory guard.
        self.vaults = self
            .asset_mints
            .iter()
            .map(|m| -> Result<(Pubkey, u64)> {
                let (_, _, d) = find(pda::token_ata(&self.market, m)).context("pool vault missing")?;
                Ok((*m, u64::from_le_bytes(d.get(64..72).context("vault too short")?.try_into().unwrap())))
            })
            .collect::<Result<_>>()?;

        // Decimals: lp mint is in the discovery set; asset mints are seeds.
        let mut mints = vec![spl_mint_info(lp_token, &fetched)?];
        for m in &public {
            mints.push(spl_mint_info(*m, &fetched)?);
        }
        if mints.len() < 2 {
            bail!("no public routable assets");
        }
        self.mints = mints;
        self.loaded = Some(Arc::new(accts));
        Ok(())
    }

    /// The mints this venue trades (empty until loaded): `[lp (idx 0), public
    /// asset_0..n]`.
    pub fn mints(&self) -> Vec<MintInfo> {
        self.mints.clone()
    }

    fn loaded(&self) -> Result<&PoolAccounts> {
        self.loaded.as_deref().context("venue not loaded")
    }

    /// Classify a pair. LP as INPUT is the cooldown withdrawal: a hard
    /// error, not a zero - Jupiter surfaces it as no-route, Titan `bounds()`
    /// skips it.
    pub fn route(&self, input_mint: Pubkey, output_mint: Pubkey) -> Result<RlpRoute> {
        let lp = self.mints.first().context("venue not loaded")?.pubkey;
        if input_mint == lp {
            bail!("LP withdrawal is a two-transaction cooldown; not routable");
        }
        // mints[1..] IS the public asset set (private assets never listed).
        let is_public = |m: Pubkey| self.mints[1..].iter().any(|mi| mi.pubkey == m);
        if !is_public(input_mint) {
            bail!("mint {input_mint} is not a public registered asset");
        }
        if output_mint == lp {
            Ok(RlpRoute::Restake)
        } else if is_public(output_mint) && output_mint != input_mint {
            Ok(RlpRoute::Swap)
        } else {
            Err(anyhow!("mint pair does not classify"))
        }
    }

    /// Quote, ExactIn. The ladder ORDER is load-bearing: zero -> route ->
    /// killswitch -> SDK quote -> inventory.
    ///
    /// `None` is "no route": the SDK predicts a revert (stale oracle) or the
    /// venue predicts one (killswitch, insufficient swap inventory) - all
    /// on-chain revert conditions the raw SDK quote does not model itself.
    /// `amount == 0` returns `Some(zero)` before touching state.
    ///
    /// `fee` is ALWAYS 0: restake charges no fee, swap is at the raw oracle
    /// cross-rate with no fee - a wrong-unit fee would mislead a router more
    /// than zero does.
    ///
    /// # Errors
    /// If the venue isn't loaded, the mint pair doesn't classify, or the SDK
    /// quote call fails for a reason other than a stale oracle.
    pub fn quote(&self, input_mint: Pubkey, output_mint: Pubkey, amount: u64, clock: Clock) -> Result<Option<RouteQuote>> {
        if amount == 0 {
            return Ok(Some(RouteQuote { amount_out: 0, fee: 0 }));
        }
        let accts = self.loaded()?;
        let route = self.route(input_mint, output_mint)?;
        // Guard 1: killswitch / freeze, precomputed at load. The SDK quote
        // does NOT check these; quoting through a frozen action breaks
        // quote==execution.
        let frozen = match route {
            RlpRoute::Restake => self.restake_frozen,
            RlpRoute::Swap => self.swap_frozen,
        };
        if frozen {
            return Ok(None);
        }
        let q = match accts.quote_by_mints(input_mint, output_mint, amount, clock) {
            Ok(q) => q,
            Err(CoreError::StaleOracle) => return Ok(None), // 120s unix-time gate
            Err(e) => return Err(anyhow!("quote: {e:?}")),
        };
        // Guard 2: swap pays from pool inventory; the SDK quote reads only
        // the two oracles. An output the vault cannot cover reverts on-chain.
        if matches!(route, RlpRoute::Swap) {
            let inventory = self.vaults.iter().find(|(m, _)| *m == output_mint).map(|(_, a)| *a).unwrap_or(0);
            if q.amount_out > inventory {
                return Ok(None);
            }
        }
        Ok(Some(RouteQuote { amount_out: q.amount_out, fee: 0 }))
    }

    /// Build; `permissions` stays `None`.
    ///
    /// # Errors
    /// If the venue isn't loaded, the mint pair doesn't classify, or the
    /// SDK's restake/swap build fails.
    pub fn build(&self, user: Pubkey, input_mint: Pubkey, output_mint: Pubkey, amount: u64, slippage_bps: u16, clock: Clock) -> Result<Instruction> {
        let h = self.loaded()?.pool(user);

        let (_q, built) = match self.route(input_mint, output_mint)? {
            RlpRoute::Restake => h.restake(input_mint, amount, slippage_bps, clock).map_err(|e| anyhow!("restake: {e:?}"))?,
            RlpRoute::Swap => h.swap(input_mint, output_mint, amount, slippage_bps, clock).map_err(|e| anyhow!("swap: {e:?}"))?,
        };

        Ok(built.instruction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anchor account discriminators (reflect_protocol_core::rlp::discriminator::account).
    mod disc {
        pub const LIQUIDITY_POOL: [u8; 8] = [66, 38, 17, 64, 188, 80, 68, 129];
        pub const SETTINGS: [u8; 8] = [223, 179, 163, 190, 177, 224, 67, 173];
        pub const ASSET: [u8; 8] = [234, 180, 241, 252, 139, 224, 160, 8];
    }

    fn pk(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }
    fn clk(unix: i64) -> Clock {
        Clock { slot: 0, unix }
    }

    // LiquidityPool: 8 disc + bump + index + lp_token(32)@10 + cooldowns(8)@42 + cooldown_duration(8)@50.
    fn pool_buf(index: u8, lp_token: &Pubkey) -> Vec<u8> {
        let mut b = vec![0u8; 58];
        b[0..8].copy_from_slice(&disc::LIQUIDITY_POOL);
        b[9] = index;
        b[10..42].copy_from_slice(lp_token.as_ref());
        b
    }
    // Settings: 8 disc + bump + liquidity_pools@9 + assets@10 + frozen@11 + access(362) + killswitch@373.
    fn settings_buf(assets: u8, frozen: bool, killswitch: u8) -> Vec<u8> {
        let mut b = vec![0u8; 374];
        b[0..8].copy_from_slice(&disc::SETTINGS);
        b[10] = assets;
        b[11] = frozen as u8;
        b[373] = killswitch;
        b
    }
    // Asset: 8 disc + mint(32)@8 + oracle_tag@40 + oracle_key(32)@41 + access_level@73 (0=Public,1=Private).
    fn asset_buf(mint: &Pubkey, oracle: &Pubkey, private: bool) -> Vec<u8> {
        let mut b = vec![0u8; 74];
        b[0..8].copy_from_slice(&disc::ASSET);
        b[8..40].copy_from_slice(mint.as_ref());
        b[40] = 0; // Pyth
        b[41..73].copy_from_slice(oracle.as_ref());
        b[73] = private as u8;
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
        b
    }

    // Pyth PriceUpdateV2, Full-verification, price $1.00 (1e8 @ exp -8), fresh.
    const PYTH_DISC: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];
    fn pyth_buf(price: i64, exponent: i32, publish: i64) -> Vec<u8> {
        let mut b = vec![0u8; 101];
        b[0..8].copy_from_slice(&PYTH_DISC);
        b[40] = 1;
        b[73..81].copy_from_slice(&price.to_le_bytes());
        b[89..93].copy_from_slice(&exponent.to_le_bytes());
        b[93..101].copy_from_slice(&publish.to_le_bytes());
        b
    }

    type FetchedAccounts = Vec<(Pubkey, Pubkey, Vec<u8>)>;

    // A 2-asset pool (one private) genesis-empty, plus healthy vault inventory
    // for the swap direction. index 0.
    fn genesis(asset_a: Pubkey, asset_b: Pubkey, oracle_a: Pubkey, oracle_b: Pubkey, b_private: bool) -> (Pubkey, Pubkey, FetchedAccounts) {
        let pool = pda::derive_liquidity_pool(0).0;
        let lp_token = pk(200);
        let accounts = vec![
            (pool, ids::RLP_PROGRAM, pool_buf(0, &lp_token)),
            (pda::derive_settings().0, ids::RLP_PROGRAM, settings_buf(2, false, 0)),
            (pda::derive_asset(&asset_a).0, ids::RLP_PROGRAM, asset_buf(&asset_a, &oracle_a, false)),
            (pda::derive_asset(&asset_b).0, ids::RLP_PROGRAM, asset_buf(&asset_b, &oracle_b, b_private)),
            (lp_token, pk(201), mint_buf(0, 6)),
            (asset_a, pk(201), mint_buf(0, 6)),
            (asset_b, pk(201), mint_buf(0, 6)),
            (pda::token_ata(&pool, &asset_a), pk(202), token_buf(1_000_000_000)),
            (pda::token_ata(&pool, &asset_b), pk(202), token_buf(1_000_000_000)),
            (oracle_a, pk(203), pyth_buf(100_000_000, -8, 1_000)),
            (oracle_b, pk(203), pyth_buf(100_000_000, -8, 1_000)),
        ];
        (pool, lp_token, accounts)
    }

    #[test]
    fn cold_venue_contract() {
        let v = RlpVenue::from_market(Pubkey::new_unique(), vec![Pubkey::new_unique()]);
        assert!(v.mints().is_empty());
        let z = v.quote(Pubkey::new_unique(), Pubkey::new_unique(), 0, clk(0)).unwrap();
        assert!(matches!(z, Some(RouteQuote { amount_out: 0, .. })));
        assert!(v.quote(Pubkey::new_unique(), Pubkey::new_unique(), 1, clk(0)).is_err());
        assert!(v.route(Pubkey::new_unique(), Pubkey::new_unique()).is_err());
        assert!(v.build(Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), 1, 0, clk(0)).is_err());
    }

    #[test]
    fn seeds_order_is_pool_settings_assets_mints() {
        let a = pk(10);
        let b = pk(20);
        let v = RlpVenue::from_market(pda::derive_liquidity_pool(0).0, vec![a, b]);
        let seeds = v.seeds();
        assert_eq!(seeds.len(), 6);
        assert_eq!(seeds[0], pda::derive_liquidity_pool(0).0);
        assert_eq!(seeds[1], pda::derive_settings().0);
        assert_eq!(seeds[2], pda::derive_asset(&a).0);
        assert_eq!(seeds[3], pda::derive_asset(&b).0);
        assert_eq!(seeds[4], a);
        assert_eq!(seeds[5], b);
    }

    #[test]
    fn discovery_rejects_asset_count_mismatch() {
        let a = pk(10);
        let (pool, lp_token, accounts) = genesis(a, pk(20), pk(11), pk(21), false);
        let v = RlpVenue::from_market(pool, vec![a]); // only 1 injected, settings says 2
        let asset_bytes = [accounts[2].2.as_slice()];
        assert!(v.discovery_keys(&pool_buf(0, &lp_token), &accounts[1].2, &asset_bytes).is_err());
    }

    #[test]
    fn discovery_rejects_wrong_pool_pda() {
        let a = pk(10);
        let (_, lp_token, accounts) = genesis(a, pk(20), pk(11), pk(21), false);
        let v = RlpVenue::from_market(Pubkey::new_unique(), vec![a, pk(20)]);
        let asset_bytes = [accounts[2].2.as_slice(), accounts[3].2.as_slice()];
        assert!(v.discovery_keys(&pool_buf(0, &lp_token), &accounts[1].2, &asset_bytes).is_err());
    }

    #[test]
    fn load_rejects_wrong_owner() {
        let a = pk(10);
        let (pool, _, mut accounts) = genesis(a, pk(20), pk(11), pk(21), false);
        accounts[0].1 = Pubkey::new_unique();
        let mut v = RlpVenue::from_market(pool, vec![a, pk(20)]);
        assert!(v.load(accounts).is_err());
    }

    #[test]
    fn private_asset_excluded_from_mints_and_route() {
        let a = pk(10);
        let b = pk(20);
        let (pool, _, accounts) = genesis(a, b, pk(11), pk(21), true); // b is private
        let mut v = RlpVenue::from_market(pool, vec![a, b]);
        v.load(accounts).unwrap();
        assert_eq!(v.mints().len(), 2); // [lp, a] - b excluded
        assert!(!v.mints().iter().any(|m| m.pubkey == b));
        assert!(v.route(b, a).is_err());
        assert!(v.route(a, b).is_err());
    }

    #[test]
    fn lp_input_is_not_a_route() {
        let a = pk(10);
        let b = pk(20);
        let (pool, lp_token, accounts) = genesis(a, b, pk(11), pk(21), false);
        let mut v = RlpVenue::from_market(pool, vec![a, b]);
        v.load(accounts).unwrap();
        assert!(v.route(lp_token, a).is_err());
    }

    #[test]
    fn loaded_venue_quotes_restake_and_swap() {
        let a = pk(10);
        let b = pk(20);
        let (pool, lp_token, accounts) = genesis(a, b, pk(11), pk(21), false);
        let mut v = RlpVenue::from_market(pool, vec![a, b]);
        v.load(accounts).unwrap();

        assert_eq!(v.route(a, lp_token).unwrap(), RlpRoute::Restake);
        assert_eq!(v.route(a, b).unwrap(), RlpRoute::Swap);

        let restake_q = v.quote(a, lp_token, 1_000_000, clk(1_050)).unwrap().unwrap();
        assert_eq!(restake_q.amount_out, 1_000_000); // 1:1 first deposit
        assert_eq!(restake_q.fee, 0);

        let swap_q = v.quote(a, b, 1_000_000, clk(1_050)).unwrap().unwrap();
        assert_eq!(swap_q.amount_out, 1_000_000); // both priced at $1

        let ix = v.build(Pubkey::new_unique(), a, lp_token, 1_000_000, 0, clk(1_050)).unwrap();
        assert_eq!(ix.program_id, ids::RLP_PROGRAM);
    }

    #[test]
    fn killswitch_gates_quote() {
        let a = pk(10);
        let b = pk(20);
        let pool = pda::derive_liquidity_pool(0).0;
        let lp_token = pk(200);
        let (_, _, mut accounts) = genesis(a, b, pk(11), pk(21), false);
        // Freeze Restake (ordinal 0, bit 0) - swap stays live.
        accounts[1].2 = settings_buf(2, false, 0b0000_0001);
        let mut v = RlpVenue::from_market(pool, vec![a, b]);
        v.load(accounts).unwrap();
        assert!(v.quote(a, lp_token, 1_000_000, clk(1_050)).unwrap().is_none());
        assert!(v.quote(a, b, 1_000_000, clk(1_050)).unwrap().is_some());
    }

    #[test]
    fn global_frozen_gates_swap_not_restake() {
        let a = pk(10);
        let b = pk(20);
        let pool = pda::derive_liquidity_pool(0).0;
        let lp_token = pk(200);
        let (_, _, mut accounts) = genesis(a, b, pk(11), pk(21), false);
        accounts[1].2 = settings_buf(2, true, 0); // global frozen
        let mut v = RlpVenue::from_market(pool, vec![a, b]);
        v.load(accounts).unwrap();
        assert!(v.quote(a, b, 1_000_000, clk(1_050)).unwrap().is_none());
        assert!(v.quote(a, lp_token, 1_000_000, clk(1_050)).unwrap().is_some());
    }

    #[test]
    fn swap_clamped_by_destination_vault() {
        let a = pk(10);
        let b = pk(20);
        let (pool, _, accounts) = genesis(a, b, pk(11), pk(21), false);
        let mut v = RlpVenue::from_market(pool, vec![a, b]);
        v.load(accounts).unwrap();
        // Vault holds 1e9; ask for way more than that can pay out.
        assert!(v.quote(a, b, 2_000_000_000, clk(1_050)).unwrap().is_none());
        assert!(v.quote(a, b, 1_000, clk(1_050)).unwrap().is_some());
    }
}
