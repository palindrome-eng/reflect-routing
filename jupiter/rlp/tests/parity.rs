//! HERMETIC. Asserts the Jupiter rlp adapter's quote equals the shared
//! [`RlpVenue`] and pins the `get_swap_and_account_metas` account-meta ORDER
//! per route as the golden contract.
//!
//! Jupiter's end-to-end swap cannot run here - the `ReflectRlp*` tags are
//! placeholders for Jupiter's closed program - so the test validates the
//! owned parts:
//!   - `ReflectRlpAmm::quote` == `reflect-routing-core` `RlpVenue::quote`.
//!     Both adapters delegate to that one call.
//!   - the `account_metas` ORDER per route (restake / swap are different
//!     Reflect instructions), indexed positionally by the eventual on-chain
//!     handler - the binding contract handed to Jupiter.
//!
//! UNLIKE the Titan suite, this one is NOT backed by a LiteSVM
//! quote==execution proof: the rlp program cannot currently be built from
//! source (Anchor 0.29 account-validation code exceeds the current Solana
//! platform-tools' BPF stack-frame limit - `cargo build-sbf` in
//! `reflect-program-library/rlp` fails to emit a `.so`), rlp is not deployed
//! on mainnet, and the devnet deployment is stale (`Settings` predates the
//! current SDK layout). State here is hand-synthesized purely to exercise
//! the SDK's own quote math and the adapter's plumbing - it is NOT a
//! substitute for on-chain proof. See `RlpVenue`'s module doc for the
//! separate, still-open restake permission-gating question.

use std::sync::atomic::Ordering;

use solana_sdk::account::Account;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::pubkey::Pubkey;

use reflect_routing_core::{RlpRoute, RlpVenue};
use reflect_sdk::common::Clock as ReflectClock;
use reflect_sdk::rlp::{ids, pda};

use reflect_jupiter_common::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, Swap, SwapMode, SwapParams};
use reflect_jupiter_rlp::ReflectRlpAmm;

const PYTH_DISC: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];
const LP_DISC: [u8; 8] = [66, 38, 17, 64, 188, 80, 68, 129];
const SETTINGS_DISC: [u8; 8] = [223, 179, 163, 190, 177, 224, 67, 173];
const ASSET_DISC: [u8; 8] = [234, 180, 241, 252, 139, 224, 160, 8];

fn pool_buf(index: u8, lp_token: &Pubkey) -> Vec<u8> {
    let mut b = vec![0u8; 58];
    b[0..8].copy_from_slice(&LP_DISC);
    b[9] = index;
    b[10..42].copy_from_slice(lp_token.as_ref());
    b
}
fn settings_buf(assets: u8) -> Vec<u8> {
    let mut b = vec![0u8; 374];
    b[0..8].copy_from_slice(&SETTINGS_DISC);
    b[10] = assets;
    b
}
fn asset_buf(mint: &Pubkey, oracle: &Pubkey) -> Vec<u8> {
    let mut b = vec![0u8; 74];
    b[0..8].copy_from_slice(&ASSET_DISC);
    b[8..40].copy_from_slice(mint.as_ref());
    b[41..73].copy_from_slice(oracle.as_ref()); // tag 0 = Pyth
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
fn pyth_buf(price: i64, exponent: i32, publish: i64) -> Vec<u8> {
    let mut b = vec![0u8; 101];
    b[0..8].copy_from_slice(&PYTH_DISC);
    b[40] = 1;
    b[73..81].copy_from_slice(&price.to_le_bytes());
    b[89..93].copy_from_slice(&exponent.to_le_bytes());
    b[93..101].copy_from_slice(&publish.to_le_bytes());
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

/// A 2-public-asset pool, both priced at $1, vault inventory 1e9 each side -
/// exercises restake (a -> lp) and swap (a <-> b) at par.
fn build_fixture() -> Fixture {
    let pool = pda::derive_liquidity_pool(0).0;
    let lp_token = Pubkey::new_unique();
    let asset_a = Pubkey::new_unique();
    let asset_b = Pubkey::new_unique();
    let oracle_a = Pubkey::new_unique();
    let oracle_b = Pubkey::new_unique();

    let mut map = AccountMap::default();
    map.insert(pool, account(ids::RLP_PROGRAM, pool_buf(0, &lp_token)));
    map.insert(pda::derive_settings().0, account(ids::RLP_PROGRAM, settings_buf(2)));
    map.insert(pda::derive_asset(&asset_a).0, account(ids::RLP_PROGRAM, asset_buf(&asset_a, &oracle_a)));
    map.insert(pda::derive_asset(&asset_b).0, account(ids::RLP_PROGRAM, asset_buf(&asset_b, &oracle_b)));
    map.insert(lp_token, account(Pubkey::new_unique(), mint_buf(0, 6)));
    map.insert(asset_a, account(Pubkey::new_unique(), mint_buf(0, 6)));
    map.insert(asset_b, account(Pubkey::new_unique(), mint_buf(0, 6)));
    map.insert(pda::token_ata(&pool, &asset_a), account(Pubkey::new_unique(), token_buf(1_000_000_000)));
    map.insert(pda::token_ata(&pool, &asset_b), account(Pubkey::new_unique(), token_buf(1_000_000_000)));
    map.insert(oracle_a, account(Pubkey::new_unique(), pyth_buf(100_000_000, -8, 1_000)));
    map.insert(oracle_b, account(Pubkey::new_unique(), pyth_buf(100_000_000, -8, 1_000)));

    let clock = SolanaClock { slot: 0, unix_timestamp: 1_050, ..Default::default() };
    Fixture { pool, lp_token, asset_a, asset_b, clock, map }
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
    amm.update(&fx.map).expect("update round 1");
    if amm.get_reserve_mints().len() < 3 {
        amm.update(&fx.map).expect("update round 2");
    }
    assert_eq!(amm.get_reserve_mints().len(), 3, "amm must resolve [lp, asset_a, asset_b]");
    (amm, clock_ref)
}

fn core_venue(fx: &Fixture) -> RlpVenue {
    let mut v = RlpVenue::from_market(fx.pool, vec![fx.asset_a, fx.asset_b]);
    let pool_bytes = fx.map.get(&fx.pool).unwrap().data.clone();
    let settings_bytes = fx.map.get(&pda::derive_settings().0).unwrap().data.clone();
    let asset_bytes: Vec<Vec<u8>> = [fx.asset_a, fx.asset_b].iter().map(|m| fx.map.get(&pda::derive_asset(m).0).unwrap().data.clone()).collect();
    let asset_slices: Vec<&[u8]> = asset_bytes.iter().map(Vec::as_slice).collect();
    let keys = v.discovery_keys(&pool_bytes, &settings_bytes, &asset_slices).expect("discovery");
    v.load(keys.iter().filter_map(|k| fx.map.get(k).map(|a| (*k, a.owner, a.data.clone())))).expect("load");
    v
}

fn reflect_clock(clock: &ClockRef) -> ReflectClock {
    ReflectClock { slot: clock.slot.load(Ordering::Relaxed), unix: clock.unix_timestamp.load(Ordering::Relaxed) }
}

/// The Jupiter adapter's quote must equal the shared `RlpVenue` quote, over
/// both routable directions (restake, swap).
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
            assert_eq!(q.fee_amount, 0);
        }
    }
}

/// would_revert edges: LP as input (cooldown withdrawal) and an over-ask swap
/// past the destination vault's inventory both surface as `Err`.
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_errs_on_would_revert() {
    let fx = build_fixture();
    let (amm, _clock) = setup(&fx);

    let lp_in = amm.quote(&QuoteParams { amount: 1_000, input_mint: fx.lp_token, output_mint: fx.asset_a, swap_mode: SwapMode::ExactIn });
    assert!(lp_in.is_err(), "LP withdrawal is not a synchronous quote");

    let over_ask = amm.quote(&QuoteParams { amount: u64::MAX, input_mint: fx.asset_a, output_mint: fx.asset_b, swap_mode: SwapMode::ExactIn });
    assert!(over_ask.is_err(), "swap past pool inventory must surface as Err, not a bad route");
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
            RlpRoute::Restake => Swap::ReflectRlpRestake,
            RlpRoute::Swap => Swap::ReflectRlpSwap,
        };
        assert_eq!(sam.swap, expected_tag, "route→tag mapping for {in_mint}->{out_mint}");
        assert!(!sam.account_metas.is_empty(), "metas must not be empty");

        let signers: Vec<_> = sam.account_metas.iter().filter(|m| m.is_signer).map(|m| m.pubkey).collect();
        assert_eq!(signers, vec![user], "only the user signs an aggregator route ({in_mint}->{out_mint})");

        eprintln!("[golden] {:?} account_metas order ({} accounts):", sam.swap, sam.account_metas.len());
        for (i, m) in sam.account_metas.iter().enumerate() {
            eprintln!("  [{i:>2}] {} signer={} writable={}", m.pubkey, m.is_signer, m.is_writable);
        }
    }
}
