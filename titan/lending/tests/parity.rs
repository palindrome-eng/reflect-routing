//! Off-chain `quote(amount)` must equal the on-chain execution delta, atom-exact,
//! both directions, over an amount sweep. Both adapters delegate quote/build to
//! the same `reflect-sdk` math, so executing the Titan venue's
//! [`ReflectLendingVenue::generate_swap_instruction`] (a complete Reflect
//! mint/redeem ix) proves the property for Titan AND Jupiter.
//!
//! Five programs, all DUMPED mainnet binaries (see `SOURCES.md`): `reflect_main.so`
//! and the four venue programs a lending mint or redeem calls (Kamino Lend, Kamino
//! Farms, Jupiter Lend, Jupiter Liquidity), over a mainnet snapshot of one lending
//! strategy.
//!
//! MANDATORY: a missing fixture is a test FAILURE, not a skip.

use std::path::PathBuf;

use litesvm::LiteSVM;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;

use reflect_sdk::strategy::ids::REFLECT_PROGRAM;

use reflect_titan_common::harness::*;
use reflect_titan_common::trading_venue::TradingVenue;
use reflect_titan_lending::ReflectLendingVenue;

/// The lending strategy in `snapshots/lending.json`.
const CONTROLLER: Pubkey = solana_sdk::pubkey!("7hHkpUXHXudwSE7HKctjntJLmJM82sZ3wqzWvJevTJch");

/// The venue programs the strategy calls: `(program id, dump name)`.
const VENUE_PROGRAMS: [(Pubkey, &str); 4] = [
    (solana_sdk::pubkey!("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"), "kamino_lend"),
    (solana_sdk::pubkey!("FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr"), "kamino_farms"),
    (solana_sdk::pubkey!("jup3YeL8QhtSx1e253b2FDvsMNC87fDrgQZivbrndc9"), "jupiter_lend"),
    (solana_sdk::pubkey!("jupeiUmn818Jg1ekPURTpr4mFo29p46vygyykFJ3wZC"), "jupiter_liquidity"),
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The mainnet programs over the snapshot, at the snapshot's own clock so that every venue
/// refresh sees its oracle as fresh, and the venue quoting off that SAME state. Returns the
/// venue, the SVM, and the deposit and receipt mints.
async fn setup() -> (ReflectLendingVenue, LiteSVM, Pubkey, Pubkey) {
    let snap = snapshot(&fixtures(), "lending");
    let mut svm = LiteSVM::new();
    svm.add_program(REFLECT_PROGRAM, &program_bytes(&fixtures(), "reflect_main"));
    for (id, name) in VENUE_PROGRAMS {
        svm.add_program(id, &program_bytes(&fixtures(), name));
    }
    load(&mut svm, &snap);
    set_clock(&mut svm, &Clock { slot: snap.slot, unix_timestamp: snap.unix_timestamp, ..Default::default() });
    let venue: ReflectLendingVenue = venue_over(&SvmCache(&svm), &CONTROLLER).await;
    let toks = venue.get_token_info();
    assert_eq!(toks.len(), 2, "snapshot must yield a tradable venue");
    let (deposit, receipt) = (toks[0].pubkey, toks[1].pubkey);
    (venue, svm, deposit, receipt)
}

/// Mint and redeem: `delta == quote`, atom-exact. Both directions, at both bounds and over
/// a log-uniform sweep of 50 amounts each.
#[tokio::test(flavor = "current_thread")]
async fn parity_swept_both_directions() {
    let (venue, mut svm, deposit, receipt) = setup().await;
    let user = funded_user(&mut svm);
    let mut rng = Rng::new(0x6c65_6e64_5345_4544);

    for (i, j, in_mint, out_mint) in [(0, 1, deposit, receipt), (1, 0, receipt, deposit)] {
        let dir = if in_mint == deposit { "mint" } else { "redeem" };
        let (lb, ub) = venue.bounds(i, j).unwrap_or_else(|e| panic!("{dir} bounds: {e:?}"));
        fund_direction(&mut svm, &user, &venue, in_mint, out_mint);
        let mut amounts = vec![lb, ub];
        amounts.extend((0..50).map(|_| rng.log_uniform(lb, ub)));
        for amount in amounts {
            let q = venue.quote(exact_in(in_mint, out_mint, amount)).expect("quote");
            assert!(!q.not_enough_liquidity, "amount {amount} unexpectedly outside the routable band {lb}..={ub}");
            let delta = simulate_delta(&mut svm, &venue, &user, exact_in(in_mint, out_mint, amount)).unwrap_or_else(|| panic!("{dir} of {amount} did not execute"));
            assert_eq!(delta, q.expected_output, "atom-exact {dir} parity broke: amount {amount}: quote {} sim {delta}", q.expected_output);
        }
        eprintln!("parity ok: {dir} over {lb}..={ub}");
    }
}

/// mint → redeem ≈ identity within rounding tolerance (~1%). Quote-only.
#[tokio::test(flavor = "current_thread")]
async fn parity_roundtrip_identity() {
    let (venue, _svm, deposit, receipt) = setup().await;
    // Half of the largest redeem the vault pays, so that both legs are inside their bands.
    let amount = venue.bounds(1, 0).expect("redeem bounds").1 / 2;

    let q1 = venue.quote(exact_in(deposit, receipt, amount)).expect("mint quote");
    assert!(q1.expected_output > 0);
    let q2 = venue.quote(exact_in(receipt, deposit, q1.expected_output)).expect("redeem quote");

    let diff = (amount as i64 - q2.expected_output as i64).unsigned_abs();
    assert!(diff <= amount / 100, "roundtrip drifted >1%: {amount} -> {} -> {} (diff {diff})", q1.expected_output, q2.expected_output);
}

/// Edges: amount 0 → clean 0 (never panics); and a REDEEM worth more than the
/// vault surfaces `not_enough_liquidity` (the adapter's would_revert path).
/// Quote-only.
#[tokio::test(flavor = "current_thread")]
async fn edges_zero_and_redeem_floor() {
    let (venue, _svm, deposit, receipt) = setup().await;
    let (_, rhi) = venue.bounds(1, 0).expect("redeem bounds");
    let probe = rhi / 2;
    for (a, b) in [(deposit, receipt), (receipt, deposit)] {
        let z = venue.quote(exact_in(a, b, 0)).expect("zero quote");
        assert_eq!(z.expected_output, 0);
        assert!(!z.not_enough_liquidity, "amount 0 is not a liquidity failure");
        // Titan `price`: a positive spot at zero input that, on this linear route, prices
        // a real fill to within Titan's 1e-5 mean-value tolerance.
        assert!(z.price > 0.0, "spot must be positive, got {}", z.price);
        let q = venue.quote(exact_in(a, b, probe)).expect("probe quote");
        assert!((q.price * probe as f64 - q.expected_output as f64).abs() <= q.expected_output as f64 * 1e-5 + 1.0, "price {} vs out {} at {probe}", q.price, q.expected_output);
    }

    // A burn worth more than the vault underflows on chain → would_revert →
    // not_enough_liquidity. The ceiling lives at bounds(1,0).hi.
    let over = venue.quote(exact_in(receipt, deposit, rhi.saturating_mul(4))).expect("over quote");
    assert!(over.not_enough_liquidity, "redeem past the vault must report no liquidity");
}

/// Above the mint band the adapter quote reports `not_enough_liquidity`, AND on-chain
/// `mint_lending` reverts. Quote and execution AGREE at the ceiling: no over-promise.
#[tokio::test(flavor = "current_thread")]
async fn mint_quote_caps_above_mintable() {
    let (venue, mut svm, deposit, receipt) = setup().await;
    let (_, ub) = venue.bounds(0, 1).expect("mint bounds");
    let over = ub.checked_add(ub / 2).expect("the mint ceiling leaves room above it");

    let q = venue.quote(exact_in(deposit, receipt, over)).expect("quote");
    assert!(q.not_enough_liquidity, "a mint above the band must report no liquidity");
    assert_eq!(q.expected_output, 0, "no-liquidity quote outputs 0");

    let user = funded_user(&mut svm);
    fund_direction(&mut svm, &user, &venue, deposit, receipt);
    assert!(simulate_delta(&mut svm, &venue, &user, exact_in(deposit, receipt, over)).is_none(), "a mint above the band must revert on-chain");
}

/// Titan's shared correctness contract (upstream `tests/common`) over the mainnet market.
#[tokio::test(flavor = "current_thread")]
async fn titan_contract() {
    let (venue, mut svm, _, _) = setup().await;
    assert_titan_contract(&mut svm, &venue, 0x7174_616e_5345_4544);
}

// Titan's allocation guard: active in debug builds, where `assert_no_alloc` aborts on a heap
// allocation inside the guarded closure; release builds (the speed test) run unguarded.
#[cfg(debug_assertions)]
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Titan's performance contract (upstream `construction` / `quoting_speed`).
#[tokio::test(flavor = "current_thread")]
async fn quotes_do_not_allocate() {
    let (venue, _svm, _, _) = setup().await;
    assert_quotes_do_not_allocate(&venue);
}

/// Titan's performance contract (upstream `construction` / `quoting_speed`).
#[tokio::test(flavor = "current_thread")]
async fn quote_speed() {
    let (venue, _svm, _, _) = setup().await;
    assert_quote_speed(&venue, 0x7174_616e_5345_4544, 1_000.0);
}

/// Mint and redeem execute inside the route program under TitanPDA custody.
#[tokio::test(flavor = "current_thread")]
async fn titan_route() {
    let Some(router) = router_program() else { return };
    let (venue, mut svm, _, _) = setup().await;
    assert_titan_route(&mut svm, &venue, &router);
}

/// Rewrite `snapshots/lending.json` from mainnet: `RPC_URL=<url> cargo test -p
/// reflect-titan-lending --test parity capture_snapshot -- --ignored`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "network: rewrites the snapshot from mainnet"]
async fn capture_snapshot() {
    let cache = RpcCache::new(&std::env::var("RPC_URL").expect("RPC_URL"));
    let venue: ReflectLendingVenue = venue_over(&cache, &CONTROLLER).await;
    capture(&cache, &venue, &fixtures(), "lending");
}
