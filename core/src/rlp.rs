//! Reflect **rlp** (insurance pool) routing: account plumbing + SDK delegation
//! for deposit (asset->LP) and swap (asset->asset).
//!
//! The SDK quote applies every program gate in handler order (killswitch, role,
//! pool membership, frozen reserves, oracle legs, deposit cap, swap inventory):
//! its `Err` IS the predicted revert, so the venue maps it to "no route".
//! LP->asset is the two-transaction cooldown withdrawal: NOT routable.
//!
//! Listing contract: the injected mint set must be exactly the pool's assets
//! (`LiquidityPool.assets` lists registry indices, and the `Asset` PDA is keyed
//! by mint, so the lister supplies the mints); `Action::Swap` must be assigned
//! `Role::Public` for asset->asset routes to clear (on mainnet prod it is
//! crank-only, so only deposits route there).

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::Now;
use reflect_sdk::rlp::{asset_keys, decode_asset_registry, pda, state_keys, AssetView, FetchedAccount, InsurancePool, LiquidityPoolView};
use reflect_sdk::strategy::MintInfo;

use crate::{spl_mint_info, RouteQuote};

/// Which routable rlp op a mint pair maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlpRoute {
    /// asset -> LP token (deposit against pool USD value).
    Deposit,
    /// asset -> asset at the oracle cross-rate, less price impact and the swap fee.
    Swap,
}

/// The one Reflect insurance pool this market wraps.
#[derive(Clone)]
pub struct RlpVenue {
    program_id: Pubkey,
    market: Pubkey,
    asset_mints: Vec<Pubkey>,
    loaded: Option<Arc<InsurancePool>>,
    mints: Vec<MintInfo>, // [lp (idx 0), public asset_0..n in pool order] - mints[1..] IS the public set
}

impl RlpVenue {
    /// Construct cold from the `LiquidityPool` account the aggregator lists:
    /// `market` is its key, `program_id` its owner (mainnet or staging rlp),
    /// `asset_mints` the pool's asset mints.
    pub fn from_market(program_id: Pubkey, market: Pubkey, asset_mints: Vec<Pubkey>) -> Self {
        Self { program_id, market, asset_mints, loaded: None, mints: vec![] }
    }

    /// The owning rlp program id.
    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    /// The market/pool key (the `LiquidityPool` PDA).
    pub fn market(&self) -> Pubkey {
        self.market
    }

    /// The injected asset mints (listing order).
    pub fn asset_mints(&self) -> &[Pubkey] {
        &self.asset_mints
    }

    /// Round-1 fetch set, FIXED order the shims rely on:
    /// `[pool, settings, asset_pda_0..n-1]`. Asset PDAs derive from the
    /// injected mints alone.
    pub fn seeds(&self) -> Vec<Pubkey> {
        let mut s = vec![self.market, pda::derive_settings(&self.program_id).0];
        s.extend(asset_keys(&self.asset_mints, &self.program_id));
        s
    }

    /// Given the round-1 bytes (pool, one slice per Asset PDA in seed order),
    /// the full fetch set: the seeds, the LP mint, and per asset its mint, pool
    /// vault and every oracle leg account.
    ///
    /// # Errors
    /// Decode failures.
    pub fn discovery_keys(&self, pool: &[u8], assets: &[&[u8]]) -> Result<Vec<Pubkey>> {
        let pv = LiquidityPoolView::from_bytes(pool).map_err(|e| anyhow!("pool: {e:?}"))?;
        let views = self
            .asset_mints
            .iter()
            .zip(assets)
            .enumerate()
            .map(|(i, (m, d))| Ok((*m, AssetView::from_bytes(d).map_err(|e| anyhow!("asset[{i}]: {e:?}"))?)))
            .collect::<Result<Vec<_>>>()?;
        let mut keys = self.seeds();
        keys.extend(state_keys(&pv, &self.market, &views).map_err(|e| anyhow!("state_keys: {e:?}"))?);
        Ok(keys)
    }

    /// Ingest the fetched set: verify ownership, match the injected mints to
    /// the pool's asset list, build the SDK loader once.
    ///
    /// # Errors
    /// If the pool account is missing or not owned by `program_id`, an asset
    /// account is missing or undecodable, the injected mints are not exactly
    /// the pool's assets, or no public asset resolves.
    pub fn load(&mut self, accounts: impl IntoIterator<Item = (Pubkey, Pubkey, Vec<u8>)>) -> Result<()> {
        let fetched: Vec<(Pubkey, Pubkey, Vec<u8>)> = accounts.into_iter().collect();
        let find = |k: Pubkey| fetched.iter().find(|(fk, _, _)| *fk == k);
        let (_, owner, pool) = find(self.market).context("pool account missing")?;
        if *owner != self.program_id {
            bail!("market owner {owner} is not the rlp program {}", self.program_id);
        }
        let pv = LiquidityPoolView::from_bytes(pool).map_err(|e| anyhow!("pool: {e:?}"))?;

        let asset_pdas = asset_keys(&self.asset_mints, &self.program_id);
        let asset_bytes = asset_pdas.iter().map(|k| Ok((*k, find(*k).context("asset account missing")?.2.as_slice()))).collect::<Result<Vec<_>>>()?;
        let registry = decode_asset_registry(asset_bytes).map_err(|e| anyhow!("asset registry: {e:?}"))?;
        if registry.len() != pv.asset_indices().len() {
            bail!("injected {} mints but the pool lists {} assets; re-list", registry.len(), pv.asset_indices().len());
        }

        let bytes = fetched.iter().map(|(k, o, d)| FetchedAccount { key: *k, owner: *o, data: d.clone() }).collect();
        let accts = InsurancePool::from_accounts(self.program_id, pv.index(), bytes).map_err(|e| anyhow!("from_accounts: {e:?}"))?;

        // Only Public assets become routable mints: a Private asset needs a
        // permissions account an aggregator user cannot supply.
        let mut mints = vec![spl_mint_info(accts.lp_token(), &fetched)?];
        for r in accts.assets() {
            let av = AssetView::from_bytes(&find(r.key).context("asset account missing")?.2).map_err(|e| anyhow!("asset {}: {e:?}", r.index))?;
            if av.is_public() {
                mints.push(spl_mint_info(r.mint, &fetched)?);
            }
        }
        if mints.len() < 2 {
            bail!("no public routable assets");
        }
        self.mints = mints;
        self.loaded = Some(Arc::new(accts));
        Ok(())
    }

    /// The mints this venue trades (empty until loaded): `[lp (idx 0), public
    /// asset_0..n in pool order]`.
    pub fn mints(&self) -> Vec<MintInfo> {
        self.mints.clone()
    }

    fn loaded(&self) -> Result<&InsurancePool> {
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
        let is_public = |m: Pubkey| self.mints[1..].iter().any(|mi| mi.pubkey == m);
        if !is_public(input_mint) {
            bail!("mint {input_mint} is not a public pool asset");
        }
        if output_mint == lp {
            Ok(RlpRoute::Deposit)
        } else if is_public(output_mint) && output_mint != input_mint {
            Ok(RlpRoute::Swap)
        } else {
            Err(anyhow!("mint pair does not classify"))
        }
    }

    /// Quote, ExactIn. `None` is "no route": the SDK predicts the revert the
    /// program would raise (killswitch, role, stale or unresolved oracle, cap,
    /// swap inventory, ...). `amount == 0` returns `Some(zero)` before touching
    /// state.
    ///
    /// `fee` is the swap fee in OUTPUT units (what `swap_fee_bps` removed);
    /// deposits charge none.
    ///
    /// # Errors
    /// If the venue isn't loaded or the mint pair doesn't classify.
    pub fn quote(&self, input_mint: Pubkey, output_mint: Pubkey, amount: u64, clock: Now) -> Result<Option<RouteQuote>> {
        if amount == 0 {
            return Ok(Some(RouteQuote { amount_out: 0, fee: 0 }));
        }
        let accts = self.loaded()?;
        self.route(input_mint, output_mint)?;
        Ok(accts.quote(input_mint, output_mint, amount, clock).ok().map(|q| RouteQuote { amount_out: q.amount_out, fee: q.fee }))
    }

    /// Build the deposit/swap instruction; `min_out` is baked from `slippage_bps`.
    ///
    /// # Errors
    /// If the venue isn't loaded, the mint pair doesn't classify, or the SDK
    /// refuses the quote the build embeds.
    pub fn build(&self, user: Pubkey, input_mint: Pubkey, output_mint: Pubkey, amount: u64, slippage_bps: u16, clock: Now) -> Result<Instruction> {
        let pool = self.loaded()?;
        let (_q, built) = match self.route(input_mint, output_mint)? {
            RlpRoute::Deposit => pool.deposit(user, input_mint, amount, slippage_bps, clock).map_err(|e| anyhow!("deposit: {e:?}"))?,
            RlpRoute::Swap => pool.swap(user, input_mint, output_mint, amount, slippage_bps, clock).map_err(|e| anyhow!("swap: {e:?}"))?,
        };
        Ok(built.instruction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_sdk::rlp::ids::{PYTH_RECEIVER, RLP_PROGRAM};
    use reflect_sdk::rlp::{Action, Role};

    // Anchor account discriminators (reflect_protocol_core::rlp::discriminator::account).
    mod disc {
        pub const LIQUIDITY_POOL: [u8; 8] = [66, 38, 17, 64, 188, 80, 68, 129];
        pub const SETTINGS: [u8; 8] = [223, 179, 163, 190, 177, 224, 67, 173];
        pub const ASSET: [u8; 8] = [234, 180, 241, 252, 139, 224, 160, 8];
    }

    const NOW: Now = Now { slot: 500_000, unix_timestamp: 1_800_000_000 };

    fn pk(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    // LiquidityPool (105 B): disc + bump@8 + index@9 + lp_token@10 + cooldowns@42 +
    // cooldown_duration@50 + deposit_cap Option<u64>@58 (None) + asset_count@59 + assets@60.
    fn pool_buf(index: u8, lp_token: &Pubkey, assets: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 105];
        b[0..8].copy_from_slice(&disc::LIQUIDITY_POOL);
        b[9] = index;
        b[10..42].copy_from_slice(lp_token.as_ref());
        b[59] = assets.len() as u8;
        b[60..60 + assets.len()].copy_from_slice(assets);
        b
    }
    // Settings (379 B): disc + bump@8 + pools@9 + assets@10 + access_map@11 (18 x [action,
    // roles[18], count]) + killswitch u32@372 + swap_fee_bps u16@376 + supremo_count@378.
    fn settings_buf(assets: u8, public: &[Action], killswitch: u32, swap_fee_bps: u16) -> Vec<u8> {
        let mut b = vec![0u8; 379];
        b[0..8].copy_from_slice(&disc::SETTINGS);
        b[10] = assets;
        for (i, a) in public.iter().enumerate() {
            let base = 11 + i * 20;
            b[base] = *a as u8;
            b[base + 1] = Role::Public as u8;
            b[base + 19] = 1;
        }
        b[372..376].copy_from_slice(&killswitch.to_le_bytes());
        b[376..378].copy_from_slice(&swap_fee_bps.to_le_bytes());
        b
    }
    // Asset, legacy layout (108 B): disc + bump@8 + index@9 + mint@10 + oracle tag@42 (0 =
    // Pyth) + key@43 + feed_id@75 + access_level@107.
    fn asset_buf(index: u8, mint: &Pubkey, oracle: &Pubkey, private: bool) -> Vec<u8> {
        let mut b = vec![0u8; 108];
        b[0..8].copy_from_slice(&disc::ASSET);
        b[9] = index;
        b[10..42].copy_from_slice(mint.as_ref());
        b[43..75].copy_from_slice(oracle.as_ref());
        b[75..107].copy_from_slice(oracle.as_ref()); // feed_id == oracle key, matched below
        b[107] = private as u8;
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
    // Pyth PriceUpdateV2 (134 B), Full, feed_id == the account key, conf 0.1%, published 30 s ago.
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

    /// A 2-asset pool (index 0, assets [0, 1]) at $1 / $2, LP supply 300e6 against 100 + 100
    /// units in the vaults ($300), Deposit and Swap public, 25 bps swap fee.
    fn genesis(asset_a: Pubkey, asset_b: Pubkey, b_private: bool) -> (Pubkey, Pubkey, FetchedAccounts) {
        let pid = &RLP_PROGRAM;
        let pool = pda::derive_liquidity_pool(0, pid).0;
        let lp_token = pk(200);
        let (oracle_a, oracle_b) = (pk(11), pk(21));
        let accounts = vec![
            (pool, RLP_PROGRAM, pool_buf(0, &lp_token, &[0, 1])),
            (pda::derive_settings(pid).0, RLP_PROGRAM, settings_buf(2, &[Action::Deposit, Action::Swap], 0, 25)),
            (pda::derive_asset(&asset_a, pid).0, RLP_PROGRAM, asset_buf(0, &asset_a, &oracle_a, false)),
            (pda::derive_asset(&asset_b, pid).0, RLP_PROGRAM, asset_buf(1, &asset_b, &oracle_b, b_private)),
            (lp_token, pk(201), mint_buf(300_000_000, 6)),
            (asset_a, pk(201), mint_buf(0, 6)),
            (asset_b, pk(201), mint_buf(0, 6)),
            (pda::token_ata(&pool, &asset_a), pk(202), token_buf(100_000_000)),
            (pda::token_ata(&pool, &asset_b), pk(202), token_buf(100_000_000)),
            (oracle_a, PYTH_RECEIVER, pyth_buf(&oracle_a, 100_000_000, -8)),
            (oracle_b, PYTH_RECEIVER, pyth_buf(&oracle_b, 200_000_000, -8)),
        ];
        (pool, lp_token, accounts)
    }

    fn loaded(asset_a: Pubkey, asset_b: Pubkey, accounts: FetchedAccounts) -> RlpVenue {
        let mut v = RlpVenue::from_market(RLP_PROGRAM, accounts[0].0, vec![asset_a, asset_b]);
        let assets = [accounts[2].2.as_slice(), accounts[3].2.as_slice()];
        let keys = v.discovery_keys(&accounts[0].2, &assets).unwrap();
        v.load(keys.iter().filter_map(|k| accounts.iter().find(|(fk, _, _)| fk == k).cloned())).unwrap();
        v
    }

    #[test]
    fn cold_venue_contract() {
        let v = RlpVenue::from_market(RLP_PROGRAM, Pubkey::new_unique(), vec![Pubkey::new_unique()]);
        assert!(v.mints().is_empty());
        let z = v.quote(Pubkey::new_unique(), Pubkey::new_unique(), 0, NOW).unwrap();
        assert!(matches!(z, Some(RouteQuote { amount_out: 0, .. })));
        assert!(v.quote(Pubkey::new_unique(), Pubkey::new_unique(), 1, NOW).is_err());
        assert!(v.route(Pubkey::new_unique(), Pubkey::new_unique()).is_err());
        assert!(v.build(Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), 1, 0, NOW).is_err());
    }

    #[test]
    fn seeds_order_is_pool_settings_assets() {
        let (a, b) = (pk(10), pk(20));
        let pool = pda::derive_liquidity_pool(0, &RLP_PROGRAM).0;
        let v = RlpVenue::from_market(RLP_PROGRAM, pool, vec![a, b]);
        assert_eq!(v.seeds(), vec![pool, pda::derive_settings(&RLP_PROGRAM).0, pda::derive_asset(&a, &RLP_PROGRAM).0, pda::derive_asset(&b, &RLP_PROGRAM).0]);
    }

    #[test]
    fn discovery_covers_lp_mints_vaults_and_legs() {
        let (a, b) = (pk(10), pk(20));
        let (pool, lp_token, accounts) = genesis(a, b, false);
        let v = RlpVenue::from_market(RLP_PROGRAM, pool, vec![a, b]);
        let keys = v.discovery_keys(&accounts[0].2, &[&accounts[2].2, &accounts[3].2]).unwrap();
        for (k, _, _) in &accounts {
            assert!(keys.contains(k), "{k} missing from discovery");
        }
        assert_eq!(keys[4], lp_token);
    }

    #[test]
    fn load_rejects_mint_set_not_matching_the_pool() {
        let (a, b) = (pk(10), pk(20));
        let (pool, _, accounts) = genesis(a, b, false);
        let mut v = RlpVenue::from_market(RLP_PROGRAM, pool, vec![a]); // pool lists two assets
        assert!(v.load(accounts).is_err());
    }

    #[test]
    fn load_rejects_market_not_at_canonical_pda() {
        let (a, b) = (pk(10), pk(20));
        let (_, _, mut accounts) = genesis(a, b, false);
        let bogus = Pubkey::new_unique();
        accounts[0].0 = bogus;
        let mut v = RlpVenue::from_market(RLP_PROGRAM, bogus, vec![a, b]);
        assert!(v.load(accounts).is_err());
    }

    #[test]
    fn load_rejects_wrong_owner() {
        let (a, b) = (pk(10), pk(20));
        let (pool, _, mut accounts) = genesis(a, b, false);
        accounts[0].1 = Pubkey::new_unique();
        let mut v = RlpVenue::from_market(RLP_PROGRAM, pool, vec![a, b]);
        assert!(v.load(accounts).is_err());
    }

    #[test]
    fn private_asset_excluded_from_mints_and_route() {
        let (a, b) = (pk(10), pk(20));
        let (_, _, accounts) = genesis(a, b, true);
        let v = loaded(a, b, accounts);
        assert_eq!(v.mints().len(), 2); // [lp, a] - b excluded
        assert!(v.route(b, a).is_err());
        assert!(v.route(a, b).is_err());
    }

    #[test]
    fn lp_input_is_not_a_route() {
        let (a, b) = (pk(10), pk(20));
        let (_, lp_token, accounts) = genesis(a, b, false);
        assert!(loaded(a, b, accounts).route(lp_token, a).is_err());
    }

    #[test]
    fn loaded_venue_quotes_deposit_and_swap() {
        let (a, b) = (pk(10), pk(20));
        let (_, lp_token, accounts) = genesis(a, b, false);
        let v = loaded(a, b, accounts);
        assert_eq!(v.route(a, lp_token).unwrap(), RlpRoute::Deposit);
        assert_eq!(v.route(a, b).unwrap(), RlpRoute::Swap);

        // $300 pool, 300e6 LP: 10 A ($10) mints 10e6 LP, no fee.
        let dq = v.quote(a, lp_token, 10_000_000, NOW).unwrap().unwrap();
        assert_eq!((dq.amount_out, dq.fee), (10_000_000, 0));

        // 1 A ($1) into B ($2): 500_000 gross, 0.5% impact on a 100e6 reserve, 25 bps fee.
        let sq = v.quote(a, b, 1_000_000, NOW).unwrap().unwrap();
        let after_impact = 500_000u128 * (10_000 - 500_000 * 10_000 / (100_000_000 + 500_000)) / 10_000;
        assert_eq!(sq.amount_out as u128, after_impact * (10_000 - 25) / 10_000);
        // The SDK reports the fee as the output it removed, reconstructed from the net.
        assert_eq!(sq.fee as u128, sq.amount_out as u128 * 10_000 / (10_000 - 25) - sq.amount_out as u128);

        let ix = v.build(Pubkey::new_unique(), a, lp_token, 1_000_000, 50, NOW).unwrap();
        assert_eq!(ix.program_id, RLP_PROGRAM);
        let ix = v.build(Pubkey::new_unique(), a, b, 1_000_000, 50, NOW).unwrap();
        assert_eq!(ix.program_id, RLP_PROGRAM);
    }

    #[test]
    fn predicted_reverts_are_no_route() {
        let (a, b) = (pk(10), pk(20));
        let (_, lp_token, accounts) = genesis(a, b, false);
        let v = loaded(a, b, accounts.clone());
        // Stale Pyth leg (120 s window on the prod build).
        let late = Now { unix_timestamp: NOW.unix_timestamp + 200, ..NOW };
        assert!(v.quote(a, lp_token, 1_000_000, late).unwrap().is_none());
        // Swap output rounding to zero (1 atom of a $1 asset into a $2 asset).
        assert!(v.quote(a, b, 1, NOW).unwrap().is_none());

        // Killswitch on Deposit (bit 0): deposit gated, swap live.
        let mut frozen = accounts.clone();
        frozen[1].2 = settings_buf(2, &[Action::Deposit, Action::Swap], 1 << Action::Deposit as u32, 25);
        let v = loaded(a, b, frozen);
        assert!(v.quote(a, lp_token, 1_000_000, NOW).unwrap().is_none());
        assert!(v.quote(a, b, 1_000_000, NOW).unwrap().is_some());

        // Swap not public (the mainnet prod default): swap gated, deposit live.
        let mut gated = accounts;
        gated[1].2 = settings_buf(2, &[Action::Deposit], 0, 25);
        let v = loaded(a, b, gated);
        assert!(v.quote(a, b, 1_000_000, NOW).unwrap().is_none());
        assert!(v.quote(a, lp_token, 1_000_000, NOW).unwrap().is_some());
    }
}
