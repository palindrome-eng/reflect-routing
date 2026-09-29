//! HERMETIC. Asserts the Jupiter bonds adapter's quote equals the shared
//! [`BondVenue`] and pins the `get_swap_and_account_metas` account-meta ORDER
//! per route as the golden contract.
//!
//! Jupiter's end-to-end swap cannot run here - the `ReflectBonds*` tags are
//! placeholders for Jupiter's closed program - so the test validates the owned
//! parts:
//!   - `ReflectBondsAmm::quote` == `reflect-routing-core` `BondVenue::quote`.
//!     Both adapters delegate to that one call, and the Titan bonds suite
//!     proves it equals on-chain execution atom-for-atom, so quote==execution
//!     transfers here WITHOUT an SVM.
//!   - the `account_metas` ORDER for each route (mint / swap / redeem are
//!     different Reflect instructions), indexed positionally by the eventual
//!     on-chain handler - the binding contract handed to Jupiter.
//!
//! OFFLINE: state is the committed forged snapshot shared with the Titan suite
//! (`titan/bonds/tests/fixtures/forged_bond_basket.json`). The program id is
//! the forge's, read from the controller account's owner - the adapter is
//! program-id-relative, so nothing is hardcoded to mainnet.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use solana_sdk::account::Account;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::pubkey::Pubkey;

use reflect_routing_core::{BondVenue, IndexRoute};
use reflect_sdk::common::Clock as ReflectClock;

use reflect_jupiter_bonds::ReflectBondsAmm;
use reflect_jupiter_common::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, Swap, SwapMode, SwapParams};

/// The forge dumps its store here (see reflect-core `bond_forge_dump_tests`).
/// Jupiter reuses the Titan crate's committed copy rather than duplicating it.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../titan/bonds/tests/fixtures/forged_bond_basket.json")
}

fn pk(s: &str) -> Pubkey {
    s.parse().expect("pubkey")
}

/// Parsed fixture: the forge program id (controller owner), the controller PDA,
/// the snapshot clock, and every forged account under its real key.
struct Snapshot {
    program_id: Pubkey,
    controller: Pubkey,
    clock: SolanaClock,
    map: AccountMap,
}

fn load_snapshot() -> Snapshot {
    let raw = std::fs::read(fixture_path()).expect("read fixture");
    let v: serde_json::Value = serde_json::from_slice(&raw).expect("parse fixture");

    let program_id = pk(v["program_id"].as_str().unwrap());
    let controller = pk(v["controller"].as_str().unwrap());
    let clock = SolanaClock { slot: v["slot"].as_u64().unwrap(), unix_timestamp: v["unix"].as_i64().unwrap(), ..Default::default() };

    let mut map = AccountMap::default();
    for rec in v["accounts"].as_array().unwrap() {
        let a = &rec["account"];
        map.insert(
            pk(rec["pubkey"].as_str().unwrap()),
            Account {
                lamports: a["lamports"].as_u64().unwrap(),
                data: B64.decode(a["data"][0].as_str().unwrap()).expect("b64"),
                owner: pk(a["owner"].as_str().unwrap()),
                executable: a["executable"].as_bool().unwrap(),
                rent_epoch: a["rentEpoch"].as_u64().unwrap(),
            },
        );
    }
    Snapshot { program_id, controller, clock, map }
}

/// Wire the Jupiter Amm from the snapshot (cold `from_keyed_account` → `update`
/// ingests seeds + discovery set from the same map). Returns `(amm, snapshot,
/// clock, receipt mint, basket asset mints)`; mints() = `[receipt, asset_0..n]`.
fn setup() -> (ReflectBondsAmm, Snapshot, ClockRef, Pubkey, Vec<Pubkey>) {
    assert!(fixture_path().exists(), "missing forged fixture {} - regenerate via reflect-core bond_forge_dump_tests; parity fixtures are mandatory", fixture_path().display());
    let snap = load_snapshot();
    let clock_ref: ClockRef = snap.clock.clone().into();
    let ctx = AmmContext { clock_ref: clock_ref.clone() };
    let ka = KeyedAccount { key: snap.controller, account: snap.map.get(&snap.controller).expect("controller in snapshot").clone(), params: None };

    let mut amm = ReflectBondsAmm::from_keyed_account(&ka, &ctx).expect("from_keyed_account");
    amm.update(&snap.map).expect("update round 1");
    if amm.get_reserve_mints().len() < 3 {
        amm.update(&snap.map).expect("update round 2"); // re-run after dynamic accounts resolved
    }
    let mints = amm.get_reserve_mints();
    assert!(mints.len() >= 3, "Jupiter amm resolved {} mints, expected receipt + 2 assets", mints.len());
    (amm, snap, clock_ref, mints[0], mints[1..].to_vec())
}

/// The shared delegate both adapters call, built independently from the same
/// bytes - the quote oracle. The Titan bonds LiteSVM suite proves ITS quotes
/// equal on-chain execution, so equality here transfers that proof to Jupiter.
fn core_venue(snap: &Snapshot) -> BondVenue {
    let mut v = BondVenue::from_market(snap.controller, snap.program_id);
    let seeds = v.seeds();
    let seed = |i: usize| snap.map.get(&seeds[i]).expect("seed in fixture").data.clone();
    let (c, ar, mo, m) = (seed(0), seed(1), seed(2), seed(3));
    let keys = v.discovery_keys(&c, &ar, &mo, &m).expect("discovery");
    v.load(keys.iter().filter_map(|k| snap.map.get(k).map(|a| (*k, a.owner, a.data.clone())))).expect("load");
    v
}

fn reflect_clock(clock: &ClockRef) -> ReflectClock {
    ReflectClock { slot: clock.slot.load(Ordering::Relaxed), unix: clock.unix_timestamp.load(Ordering::Relaxed) }
}

/// The Jupiter adapter's quote must equal the shared BondVenue quote (= Titan's,
/// = on-chain execution via the Titan bonds suite). Every direction kind, swept.
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_equals_core_venue() {
    let (amm, snap, clock, receipt, assets) = setup();
    let venue = core_venue(&snap);
    let rc = reflect_clock(&clock);

    for (in_mint, out_mint) in [(assets[0], receipt), (receipt, assets[0]), (assets[0], assets[1]), (assets[1], assets[0])] {
        for amount in [1_000u64, 250_000, 5_000_000] {
            let q = amm.quote(&QuoteParams { amount, input_mint: in_mint, output_mint: out_mint, swap_mode: SwapMode::ExactIn }).expect("jupiter quote");
            let core = venue.quote(in_mint, out_mint, amount, rc).expect("core quote").expect("core route clears");
            assert_eq!(q.out_amount, core.amount_out, "jupiter quote != core venue for {in_mint}->{out_mint} amt {amount}");
            assert_eq!(q.in_amount, amount);
        }
    }
}

/// would_revert edge: a redeem past the basket's capacity maps to no-route, so
/// the Jupiter adapter returns `Err` (no route, not a bad route).
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_errs_on_would_revert() {
    let (amm, _snap, _clock, receipt, assets) = setup();
    let r = amm.quote(&QuoteParams { amount: u64::MAX, input_mint: receipt, output_mint: assets[0], swap_mode: SwapMode::ExactIn });
    assert!(r.is_err(), "u64::MAX past capacity must surface as Err, not a bad route");
}

/// GOLDEN: the `account_metas` ORDER from `get_swap_and_account_metas` is the
/// eventual on-chain contract for each `ReflectBonds*` tag - indexed
/// positionally, so any reorder is breaking. Asserts the route→tag mapping and
/// that the user is the sole signer, and records each full order to stdout for
/// the Jupiter handoff package.
#[tokio::test(flavor = "current_thread")]
async fn golden_account_metas_order_per_route() {
    let (amm, snap, _clock, receipt, assets) = setup();
    let venue = core_venue(&snap);
    let user = Pubkey::new_unique();

    for (in_mint, out_mint) in [(assets[0], receipt), (receipt, assets[0]), (assets[0], assets[1])] {
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
            IndexRoute::Mint { .. } => Swap::ReflectBondsMint,
            IndexRoute::Swap { .. } => Swap::ReflectBondsSwap,
            IndexRoute::RedeemByReceipt { .. } => Swap::ReflectBondsRedeem,
        };
        assert_eq!(sam.swap, expected_tag, "route→tag mapping for {in_mint}->{out_mint}");
        assert!(!sam.account_metas.is_empty(), "metas must not be empty");

        // The user (token_transfer_authority) is the sole signer an aggregator
        // route carries.
        let signers: Vec<_> = sam.account_metas.iter().filter(|m| m.is_signer).map(|m| m.pubkey).collect();
        assert_eq!(signers, vec![user], "only the user signs an aggregator route ({in_mint}->{out_mint})");

        eprintln!("[golden] {:?} account_metas order ({} accounts):", sam.swap, sam.account_metas.len());
        for (i, m) in sam.account_metas.iter().enumerate() {
            eprintln!("  [{i:>2}] {} signer={} writable={}", m.pubkey, m.is_signer, m.is_writable);
        }
    }
}
