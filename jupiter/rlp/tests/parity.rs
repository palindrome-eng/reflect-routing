//! HERMETIC. Asserts the Jupiter rlp adapter's quote equals the shared
//! [`RlpVenue`] and pins the `get_swap_and_account_metas` account-meta ORDER
//! per route as the golden contract.
//!
//! Jupiter's end-to-end swap cannot run here - the `ReflectRlp*` tags are
//! placeholders for Jupiter's closed program - so the test validates the
//! owned parts:
//!   - `ReflectRlpAmm::quote` == `reflect-routing-core` `RlpVenue::quote`.
//!     Both adapters delegate to that one call, and the Titan rlp suite
//!     (`titan/rlp/tests/parity.rs`) proves it equals on-chain execution
//!     against the dumped mainnet program and the oracle-stacking build.
//!   - the `account_metas` ORDER per route (deposit / swap are different
//!     Reflect instructions), indexed positionally by the eventual on-chain
//!     handler - the binding contract handed to Jupiter.
//!
//! State is hand-synthesized at the deployed layouts (Settings 379 B,
//! LiquidityPool 105 B, Asset 266 B leg-chain) with Deposit and Swap public,
//! so both routes clear.

use std::sync::atomic::Ordering;

use solana_sdk::account::Account;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::pubkey::Pubkey;

use reflect_routing_core::{RlpRoute, RlpVenue};
use reflect_sdk::common::Now as ReflectClock;
use reflect_sdk::rlp::ids::{PYTH_RECEIVER, RLP_PROGRAM};
use reflect_sdk::rlp::{pda, Action, OracleProvider, Role};

use reflect_jupiter_common::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, Swap, SwapMode, SwapParams};
use reflect_jupiter_rlp::ReflectRlpAmm;

const PYTH_DISC: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];
const LP_DISC: [u8; 8] = [66, 38, 17, 64, 188, 80, 68, 129];
const SETTINGS_DISC: [u8; 8] = [223, 179, 163, 190, 177, 224, 67, 173];
const ASSET_DISC: [u8; 8] = [234, 180, 241, 252, 139, 224, 160, 8];
const NOW: SolanaClock = SolanaClock { slot: 500_000, epoch_start_timestamp: 0, epoch: 0, leader_schedule_epoch: 0, unix_timestamp: 1_800_000_000 };
const SWAP_FEE_BPS: u16 = 25;

// LiquidityPool (105 B): disc + bump@8 + index@9 + lp_token@10 + cooldowns@42 +
// cooldown_duration@50 + deposit_cap Option<u64>@58 (None) + asset_count@59 + assets@60.
fn pool_buf(index: u8, lp_token: &Pubkey, assets: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 105];
    b[0..8].copy_from_slice(&LP_DISC);
    b[9] = index;
    b[10..42].copy_from_slice(lp_token.as_ref());
    b[59] = assets.len() as u8;
    b[60..60 + assets.len()].copy_from_slice(assets);
    b
}
// Settings (379 B): disc + bump@8 + pools@9 + assets@10 + access_map@11 (18 x [action,
// roles[18], count]) + killswitch u32@372 + swap_fee_bps u16@376 + supremo_count@378.
fn settings_buf(assets: u8, public: &[Action], swap_fee_bps: u16) -> Vec<u8> {
    let mut b = vec![0u8; 379];
    b[0..8].copy_from_slice(&SETTINGS_DISC);
    b[10] = assets;
    for (i, a) in public.iter().enumerate() {
        let base = 11 + i * 20;
        b[base] = *a as u8;
        b[base + 1] = Role::Public as u8;
        b[base + 19] = 1;
    }
    b[376..378].copy_from_slice(&swap_fee_bps.to_le_bytes());
    b
}
// Asset, leg-chain layout (266 B): disc + bump@8 + index@9 + mint@10 + legs[3] x 73 @42
// (oracle(32) feed_id(32) max_staleness u32 inverse u8 provider u8 max_confidence_bps u16
// decimals u8) + leg_count@261 + max_composed_confidence_bps@262 + flags@264 (bit 0 =
// chain resolved) + access_level@265 (0 = Public). One Pyth leg whose feed_id is the oracle key.
fn asset_buf(index: u8, mint: &Pubkey, oracle: &Pubkey) -> Vec<u8> {
    let mut b = vec![0u8; 266];
    b[0..8].copy_from_slice(&ASSET_DISC);
    b[9] = index;
    b[10..42].copy_from_slice(mint.as_ref());
    b[42..74].copy_from_slice(oracle.as_ref());
    b[74..106].copy_from_slice(oracle.as_ref());
    b[42 + 69] = OracleProvider::Pyth as u8;
    b[261] = 1;
    b[264] = 1;
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
// Pyth PriceUpdateV2 (134 B), Full, feed_id == the account key, conf 0.1%, 30 s / 30 slots old.
fn pyth_buf(key: &Pubkey, price: i64, exponent: i32) -> Vec<u8> {
    let mut b = vec![0u8; 134];
    b[0..8].copy_from_slice(&PYTH_DISC);
    b[40] = 1;
    b[41..73].copy_from_slice(key.as_ref());
    b[73..81].copy_from_slice(&price.to_le_bytes());
    b[81..89].copy_from_slice(&(price as u64 / 1_000).to_le_bytes());
    b[89..93].copy_from_slice(&exponent.to_le_bytes());
    b[93..101].copy_from_slice(&(NOW.unix_timestamp - 30).to_le_bytes());
    b[125..133].copy_from_slice(&(NOW.slot - 30).to_le_bytes());
    b
}
fn account(owner: Pubkey, data: Vec<u8>) -> Account {
    Account { lamports: 1_000_000, data, owner, executable: false, rent_epoch: 0 }
}

struct Fixture {
    pool: Pubkey,
    lp_token: Pubkey,
    asset_a: Pubkey,
    asset_b: Pubkey,
    clock: SolanaClock,
    map: AccountMap,
}

/// A 2-asset pool (index 0) at $1 / $2, LP supply 300e6 against 100 + 100 units in the
/// vaults ($300), Deposit and Swap public, 25 bps swap fee.
fn build_fixture() -> Fixture {
    let pid = &RLP_PROGRAM;
    let pool = pda::derive_liquidity_pool(0, pid).0;
    let lp_token = Pubkey::new_unique();
    let asset_a = Pubkey::new_unique();
    let asset_b = Pubkey::new_unique();
    let oracle_a = Pubkey::new_unique();
    let oracle_b = Pubkey::new_unique();

    let mut map = AccountMap::default();
    map.insert(pool, account(RLP_PROGRAM, pool_buf(0, &lp_token, &[0, 1])));
    map.insert(pda::derive_settings(pid).0, account(RLP_PROGRAM, settings_buf(2, &[Action::Deposit, Action::Swap], SWAP_FEE_BPS)));
    map.insert(pda::derive_asset(&asset_a, pid).0, account(RLP_PROGRAM, asset_buf(0, &asset_a, &oracle_a)));
    map.insert(pda::derive_asset(&asset_b, pid).0, account(RLP_PROGRAM, asset_buf(1, &asset_b, &oracle_b)));
    map.insert(lp_token, account(Pubkey::new_unique(), mint_buf(300_000_000, 6)));
    map.insert(asset_a, account(Pubkey::new_unique(), mint_buf(0, 6)));
    map.insert(asset_b, account(Pubkey::new_unique(), mint_buf(0, 6)));
    map.insert(pda::token_ata(&pool, &asset_a), account(Pubkey::new_unique(), token_buf(100_000_000)));
    map.insert(pda::token_ata(&pool, &asset_b), account(Pubkey::new_unique(), token_buf(100_000_000)));
    map.insert(oracle_a, account(PYTH_RECEIVER, pyth_buf(&oracle_a, 100_000_000, -8)));
    map.insert(oracle_b, account(PYTH_RECEIVER, pyth_buf(&oracle_b, 200_000_000, -8)));

    Fixture { pool, lp_token, asset_a, asset_b, clock: NOW, map }
}

/// Wire the Jupiter Amm from the fixture: cold `from_keyed_account` (params
/// carry the asset-mint list) -> `update` twice to warm (seeds, then the
/// derived discovery set).
fn setup(fx: &Fixture) -> (ReflectRlpAmm, ClockRef) {
    let clock_ref: ClockRef = fx.clock.clone().into();
    let ctx = AmmContext { clock_ref: clock_ref.clone() };
    let params = serde_json::json!({ "asset_mints": [fx.asset_a.to_string(), fx.asset_b.to_string()] });
    let ka = KeyedAccount { key: fx.pool, account: fx.map.get(&fx.pool).unwrap().clone(), params: Some(params) };

    let mut amm = ReflectRlpAmm::from_keyed_account(&ka, &ctx).expect("from_keyed_account");
    assert_eq!(amm.program_id(), RLP_PROGRAM, "the listed account's owner selects the deployment");
    amm.update(&fx.map).expect("update round 1");
    if amm.get_reserve_mints().len() < 3 {
        amm.update(&fx.map).expect("update round 2");
    }
    assert_eq!(amm.get_reserve_mints().len(), 3, "amm must resolve [lp, asset_a, asset_b]");
    (amm, clock_ref)
}

fn core_venue(fx: &Fixture) -> RlpVenue {
    let mut v = RlpVenue::from_market(RLP_PROGRAM, fx.pool, vec![fx.asset_a, fx.asset_b]);
    let pool_bytes = fx.map.get(&fx.pool).unwrap().data.clone();
    let asset_bytes: Vec<Vec<u8>> = [fx.asset_a, fx.asset_b].iter().map(|m| fx.map.get(&pda::derive_asset(m, &RLP_PROGRAM).0).unwrap().data.clone()).collect();
    let asset_slices: Vec<&[u8]> = asset_bytes.iter().map(Vec::as_slice).collect();
    let keys = v.discovery_keys(&pool_bytes, &asset_slices).expect("discovery");
    v.load(keys.iter().filter_map(|k| fx.map.get(k).map(|a| (*k, a.owner, a.data.clone())))).expect("load");
    v
}

fn reflect_clock(clock: &ClockRef) -> ReflectClock {
    ReflectClock { slot: clock.slot.load(Ordering::Relaxed), unix_timestamp: clock.unix_timestamp.load(Ordering::Relaxed) }
}

/// The Jupiter adapter's quote must equal the shared `RlpVenue` quote, over
/// both routable directions (deposit, swap); the swap fee is reported in the
/// OUTPUT mint.
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_equals_core_venue() {
    let fx = build_fixture();
    let (amm, clock) = setup(&fx);
    let venue = core_venue(&fx);
    let rc = reflect_clock(&clock);

    for (in_mint, out_mint) in [(fx.asset_a, fx.lp_token), (fx.asset_a, fx.asset_b), (fx.asset_b, fx.asset_a)] {
        for amount in [1_000u64, 250_000, 5_000_000] {
            let q = amm.quote(&QuoteParams { amount, input_mint: in_mint, output_mint: out_mint, swap_mode: SwapMode::ExactIn }).expect("jupiter quote");
            let core = venue.quote(in_mint, out_mint, amount, rc).expect("core quote").expect("core route clears");
            assert_eq!(q.out_amount, core.amount_out, "jupiter quote != core venue for {in_mint}->{out_mint} amt {amount}");
            assert_eq!(q.in_amount, amount);
            assert_eq!(q.fee_amount, core.fee);
            assert_eq!(q.fee_mint, out_mint);
            if out_mint == fx.lp_token {
                assert_eq!(q.fee_amount, 0, "deposits charge no fee");
            } else {
                assert!(q.fee_amount > 0, "swaps report the {SWAP_FEE_BPS} bps fee");
            }
        }
    }
}

/// would_revert edges: LP as input (cooldown withdrawal) and a stale oracle
/// both surface as `Err` (no route, not a bad route).
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_errs_on_would_revert() {
    let fx = build_fixture();
    let (amm, _clock) = setup(&fx);
    let lp_in = amm.quote(&QuoteParams { amount: 1_000, input_mint: fx.lp_token, output_mint: fx.asset_a, swap_mode: SwapMode::ExactIn });
    assert!(lp_in.is_err(), "LP withdrawal is not a synchronous quote");

    let mut stale = build_fixture();
    stale.clock.unix_timestamp += 200; // past the 120 s Pyth window
    let (amm, _clock) = setup(&stale);
    let r = amm.quote(&QuoteParams { amount: 1_000_000, input_mint: stale.asset_a, output_mint: stale.lp_token, swap_mode: SwapMode::ExactIn });
    assert!(r.is_err(), "a stale oracle must surface as Err, not a bad route");
}

/// GOLDEN: the `account_metas` ORDER from `get_swap_and_account_metas` per
/// `ReflectRlp*` tag - indexed positionally, so any reorder is breaking.
/// Asserts the route->tag mapping and that the user is the sole signer, and
/// records each full order to stdout for the Jupiter handoff package.
#[tokio::test(flavor = "current_thread")]
async fn golden_account_metas_order_per_route() {
    let fx = build_fixture();
    let (amm, _clock) = setup(&fx);
    let venue = core_venue(&fx);
    let user = Pubkey::new_unique();

    for (in_mint, out_mint) in [(fx.asset_a, fx.lp_token), (fx.asset_a, fx.asset_b)] {
        let sp = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 250_000,
            out_amount: 0,
            source_mint: in_mint,
            destination_mint: out_mint,
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: user,
            quote_mint_to_referrer: None,
            jupiter_program_id: &Pubkey::new_unique(),
            missing_dynamic_accounts_as_default: false,
        };
        let sam = amm.get_swap_and_account_metas(&sp).expect("metas");

        let route = venue.route(in_mint, out_mint).expect("route");
        let expected_tag = match route {
            RlpRoute::Deposit => Swap::ReflectRlpDeposit,
            RlpRoute::Swap => Swap::ReflectRlpSwap,
        };
        assert_eq!(sam.swap, expected_tag, "route→tag mapping for {in_mint}->{out_mint}");
        assert!(!sam.account_metas.is_empty(), "metas must not be empty");
        // Anchor `event_cpi`: the event authority and the program id are named
        // accounts, placed BEFORE the remaining-account tail.
        let keys: Vec<Pubkey> = sam.account_metas.iter().map(|m| m.pubkey).collect();
        assert!(keys.contains(&pda::derive_event_authority(&RLP_PROGRAM).0) && keys.contains(&RLP_PROGRAM), "event_cpi pair missing");

        let signers: Vec<_> = sam.account_metas.iter().filter(|m| m.is_signer).map(|m| m.pubkey).collect();
        assert_eq!(signers, vec![user], "only the user signs an aggregator route ({in_mint}->{out_mint})");

        eprintln!("[golden] {:?} account_metas order ({} accounts):", sam.swap, sam.account_metas.len());
        for (i, m) in sam.account_metas.iter().enumerate() {
            eprintln!("  [{i:>2}] {} signer={} writable={}", m.pubkey, m.is_signer, m.is_writable);
        }
    }
}
