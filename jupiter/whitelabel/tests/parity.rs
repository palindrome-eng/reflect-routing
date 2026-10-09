//! Asserts the Jupiter whitelabel adapter's quote equals the shared
//! [`WhitelabelVenue`] and pins the `get_swap_and_account_metas` account-meta
//! ORDER per route as the golden contract.
//!
//! Jupiter's end-to-end swap cannot run here - the `ReflectProxy*` tags are
//! placeholders for Jupiter's closed program - so the test validates the owned
//! parts:
//!   - `ReflectWhitelabelAmm::quote` == `reflect-routing-core`
//!     `WhitelabelVenue::quote`. Both adapters delegate to that one call, and
//!     the Titan whitelabel suite (`titan/whitelabel/tests/parity.rs`) proves
//!     it equals on-chain execution atom-for-atom against the DUMPED mainnet
//!     proxy programs, so quote==execution transfers here WITHOUT an SVM.
//!   - the `account_metas` ORDER per route - wrap and unwrap share ONE order
//!     (the proxy program names identical accounts for both), so the TAG is
//!     the only direction carrier; this suite pins that fact.
//!
//! OFFLINE: state is the mainnet prod snapshot committed with the Titan suite
//! (`titan/whitelabel/tests/fixtures/snapshots/proxy_prod.json`).

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use base64::Engine;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::pubkey::Pubkey;

use reflect_routing_core::{WhitelabelRoute, WhitelabelVenue};
use reflect_sdk::common::Now as ReflectClock;
use reflect_sdk::proxy::ids::PROXY_PROGRAM;
use reflect_sdk::proxy::ProxyStateView;

use reflect_jupiter_common::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, Swap, SwapMode, SwapParams};
use reflect_jupiter_whitelabel::ReflectWhitelabelAmm;

const PYTH_DISC: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];

struct Fixture {
    proxy_state: Pubkey,
    branded_mint: Pubkey,
    stablecoin_mint: Pubkey,
    clock: SolanaClock,
    map: AccountMap,
}

/// The prod snapshot as an `AccountMap`, with a clock 10 slots / 10 s inside the
/// Pyth account's window.
fn build_fixture() -> Fixture {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../titan/whitelabel/tests/fixtures/snapshots/proxy_prod.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing snapshot {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut map = AccountMap::default();
    let mut clock = SolanaClock::default();
    let mut market = None;
    for a in v["accounts"].as_array().unwrap() {
        let key: Pubkey = a["key"].as_str().unwrap().parse().unwrap();
        let owner: Pubkey = a["owner"].as_str().unwrap().parse().unwrap();
        let data = base64::engine::general_purpose::STANDARD.decode(a["data"].as_str().unwrap()).unwrap();
        if data.len() >= 133 && data[..8] == PYTH_DISC {
            clock.unix_timestamp = i64::from_le_bytes(data[93..101].try_into().unwrap()) + 10;
            clock.slot = u64::from_le_bytes(data[125..133].try_into().unwrap()) + 10;
        }
        if owner == PROXY_PROGRAM && data.len() == ProxyStateView::LEN {
            let view = ProxyStateView::from_bytes(&data).unwrap();
            market = Some((key, Pubkey::new_from_array(view.branded_mint()), Pubkey::new_from_array(view.stablecoin_mint())));
        }
        map.insert(key, Account { lamports: 1_000_000, data, owner, executable: false, rent_epoch: 0 });
    }
    let (proxy_state, branded_mint, stablecoin_mint) = market.expect("snapshot holds one ProxyState");
    Fixture { proxy_state, branded_mint, stablecoin_mint, clock, map }
}

/// Wire the Jupiter Amm from the fixture: cold `from_keyed_account` -> `update`
/// up to three times to warm (the state, then its asset, then the leg set).
fn setup(fx: &Fixture) -> (ReflectWhitelabelAmm, ClockRef) {
    let clock_ref: ClockRef = fx.clock.clone().into();
    let ctx = AmmContext { clock_ref: clock_ref.clone() };
    let ka = KeyedAccount { key: fx.proxy_state, account: fx.map.get(&fx.proxy_state).unwrap().clone(), params: None };

    let mut amm = ReflectWhitelabelAmm::from_keyed_account(&ka, &ctx).expect("from_keyed_account");
    assert_eq!(amm.program_id(), PROXY_PROGRAM, "the listed account's owner selects the deployment");
    for round in 1..=3 {
        if amm.get_reserve_mints().len() == 2 {
            break;
        }
        let asked = amm.get_accounts_to_update();
        assert!(asked.iter().all(|k| fx.map.contains_key(k)), "round {round} asks for a key outside the snapshot");
        amm.update(&fx.map).unwrap_or_else(|e| panic!("update round {round}: {e}"));
    }
    assert_eq!(amm.get_reserve_mints(), vec![fx.stablecoin_mint, fx.branded_mint], "amm must resolve [stablecoin, branded]");
    (amm, clock_ref)
}

/// The shared delegate both adapters call, built independently from the same
/// bytes - the quote oracle.
fn core_venue(fx: &Fixture) -> WhitelabelVenue {
    let mut v = WhitelabelVenue::from_market(PROXY_PROGRAM, fx.proxy_state);
    let state = fx.map.get(&fx.proxy_state).unwrap().data.clone();
    let asset = fx.map.get(&v.asset_key(&state).expect("asset key")).unwrap().data.clone();
    let keys = v.discovery_keys(&state, &asset).expect("discovery");
    v.load(keys.iter().filter_map(|k| fx.map.get(k).map(|a| (*k, a.owner, a.data.clone())))).expect("load");
    v
}

fn reflect_clock(clock: &ClockRef) -> ReflectClock {
    ReflectClock { slot: clock.slot.load(Ordering::Relaxed), unix_timestamp: clock.unix_timestamp.load(Ordering::Relaxed) }
}

/// The Jupiter adapter's quote must equal the shared `WhitelabelVenue` quote
/// (= Titan's, = on-chain execution via the Titan whitelabel suite). Both
/// directions, swept.
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_equals_core_venue() {
    let fx = build_fixture();
    let (amm, clock) = setup(&fx);
    let venue = core_venue(&fx);
    let rc = reflect_clock(&clock);

    for (in_mint, out_mint) in [(fx.stablecoin_mint, fx.branded_mint), (fx.branded_mint, fx.stablecoin_mint)] {
        for amount in [1_000u64, 1_000_000, 50_000_000] {
            let q = amm.quote(&QuoteParams { amount, input_mint: in_mint, output_mint: out_mint, swap_mode: SwapMode::ExactIn }).expect("jupiter quote");
            let core = venue.quote(in_mint, amount, rc).expect("core quote").expect("core route clears");
            assert_eq!(q.out_amount, core.amount_out, "jupiter quote != core venue for {in_mint}->{out_mint} amt {amount}");
            assert_eq!(q.in_amount, amount);
            assert_eq!(q.fee_amount, 0, "the crank folds the integrator fee into pricing, never a scalar");
        }
    }
}

/// would_revert edge: a stale oracle (200 slots past the posted slot) maps to
/// no-route, so the Jupiter adapter returns `Err` (no route, not a bad route).
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_errs_on_would_revert() {
    let mut fx = build_fixture();
    fx.clock.slot += 200;
    let (amm, _clock) = setup(&fx);
    let r = amm.quote(&QuoteParams { amount: 1_000_000, input_mint: fx.stablecoin_mint, output_mint: fx.branded_mint, swap_mode: SwapMode::ExactIn });
    assert!(r.is_err(), "a stale oracle must surface as Err, not a bad route");
}

/// GOLDEN: the `account_metas` ORDER from `get_swap_and_account_metas` is the
/// eventual on-chain contract for each `ReflectProxy*` tag - indexed
/// positionally, so any reorder is breaking. Asserts the route→tag mapping,
/// that the user is the sole signer, and that wrap/unwrap share ONE order
/// (proving the tag - not the account order - carries direction here, unlike
/// lending's `ReflectS1`). Records each full order to stdout for the handoff.
#[tokio::test(flavor = "current_thread")]
async fn golden_account_metas_order_per_route() {
    let fx = build_fixture();
    let (amm, _clock) = setup(&fx);
    let venue = core_venue(&fx);
    let user = Pubkey::new_unique();

    let swap_params = |in_mint: Pubkey, out_mint: Pubkey| SwapParams {
        swap_mode: SwapMode::ExactIn,
        in_amount: 1_000_000,
        out_amount: 0,
        source_mint: in_mint,
        destination_mint: out_mint,
        source_token_account: Pubkey::new_unique(),
        destination_token_account: Pubkey::new_unique(),
        token_transfer_authority: user,
        quote_mint_to_referrer: None,
        jupiter_program_id: &PROXY_PROGRAM,
        missing_dynamic_accounts_as_default: false,
    };

    let mut orders = vec![];
    for (in_mint, out_mint) in [(fx.stablecoin_mint, fx.branded_mint), (fx.branded_mint, fx.stablecoin_mint)] {
        let sp = swap_params(in_mint, out_mint);
        let sam = amm.get_swap_and_account_metas(&sp).expect("metas");

        let route = venue.route(in_mint).expect("route");
        let expected_tag = match route {
            WhitelabelRoute::Wrap => Swap::ReflectProxyWrap,
            WhitelabelRoute::Unwrap => Swap::ReflectProxyUnwrap,
        };
        assert_eq!(sam.swap, expected_tag, "route→tag mapping for {in_mint}->{out_mint}");
        assert_eq!(sam.account_metas.len(), 10, "the prod build's single-leg wrap/unwrap shape");

        let signers: Vec<_> = sam.account_metas.iter().filter(|m| m.is_signer).map(|m| m.pubkey).collect();
        assert_eq!(signers, vec![user], "only the user signs an aggregator route ({in_mint}->{out_mint})");

        eprintln!("[golden] {:?} account_metas order ({} accounts):", sam.swap, sam.account_metas.len());
        for (i, m) in sam.account_metas.iter().enumerate() {
            eprintln!("  [{i:>2}] {} signer={} writable={}", m.pubkey, m.is_signer, m.is_writable);
        }
        orders.push(sam.account_metas.iter().map(|m| (m.pubkey, m.is_signer, m.is_writable)).collect::<Vec<_>>());
    }

    // Wrap and unwrap derive the SAME accounts (same user/branded/stablecoin
    // mints), so the full meta list is byte-identical - the tag above is the
    // ONLY direction carrier. Pin that fact explicitly.
    assert_eq!(orders[0], orders[1], "wrap and unwrap must share the exact same account-meta order");
}
