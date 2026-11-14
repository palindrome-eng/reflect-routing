//! Off-chain `quote(in, out, amount)` must equal the on-chain execution delta,
//! atom-exact, for every bond direction (asset→receipt mint, receipt→asset
//! redeem, asset→asset swap), over an amount sweep.
//!
//! One program, `reflect_main.so`, the DUMPED mainnet binary, over a mainnet
//! snapshot of one index strategy (see `SOURCES.md`).
//!
//! MANDATORY: a missing fixture is a test FAILURE, not a skip.

use std::path::PathBuf;

use litesvm::LiteSVM;
use solana_sdk::clock::Clock;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
use solana_sdk::transaction::Transaction;

use reflect_sdk::strategy::ids::REFLECT_PROGRAM;

use reflect_titan_bonds::ReflectBondVenue;
use reflect_titan_common::harness::*;
use reflect_titan_common::trading_venue::TradingVenue;

/// The index strategy in `snapshots/bond_basket.json`.
const CONTROLLER: Pubkey = solana_sdk::pubkey!("9PugLepDrG7H5rThZM5bZPJGbRg3Pf6WmDk6vU2yb4dk");

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The main program dump lives with the lending suite; one copy serves both.
fn main_program() -> Vec<u8> {
    program_bytes(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../lending/tests/fixtures"), "reflect_main")
}

/// The mainnet program over the snapshot, at the snapshot's own clock, and the venue
/// quoting off that SAME state.
async fn setup_live() -> (ReflectBondVenue, LiteSVM) {
    let snap = snapshot(&fixtures(), "bond_basket");
    let mut svm = LiteSVM::new();
    svm.add_program(REFLECT_PROGRAM, &main_program());
    load(&mut svm, &snap);
    set_clock(&mut svm, &Clock { slot: snap.slot, unix_timestamp: snap.unix_timestamp, ..Default::default() });
    let venue: ReflectBondVenue = venue_over(&SvmCache(&svm), &CONTROLLER).await;
    assert!(venue.get_token_info().len() >= 3, "expected the receipt and at least two basket assets");
    (venue, svm)
}

/// Whole tokens of each basket asset that [`setup`] mints into the strategy.
const DEPTH_TOKENS: u64 = 100_000;

/// [`setup_live`], then one committed mint of [`DEPTH_TOKENS`] of every basket asset through
/// the program. The mainnet strategy holds under one dollar, which pays no redeem or swap of
/// a routable size; the mints give every pool depth, so every direction of the basket runs.
async fn setup() -> (ReflectBondVenue, LiteSVM) {
    let (venue, mut svm) = setup_live().await;
    let whale = funded_user(&mut svm);
    let receipt = venue.get_token(0).unwrap().pubkey;
    for asset in &venue.get_token_info()[1..] {
        fund_direction(&mut svm, &whale, &venue, asset.pubkey, receipt);
        let ix = venue.generate_swap_instruction(exact_in(asset.pubkey, receipt, DEPTH_TOKENS * 10u64.pow(asset.decimals as u32)), whale.pubkey()).expect("depth mint");
        let tx = Transaction::new_signed_with_payer(&[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), ix], Some(&whale.pubkey()), &[&whale], svm.latest_blockhash());
        svm.send_transaction(tx).unwrap_or_else(|e| panic!("depth mint of {} failed: {:?}\n{}", asset.pubkey, e.err, e.meta.logs.join("\n")));
        svm.expire_blockhash();
    }
    let venue: ReflectBondVenue = venue_over(&SvmCache(&svm), &CONTROLLER).await;
    let n = venue.get_token_info().len();
    assert_eq!(venue.directions_num().len(), n * (n - 1), "every direction of the deepened basket must quote");
    (venue, svm)
}

/// Sweep every token pair of `venue` over its own bounds: the dest delta equals the quote,
/// atom-exact. A pair with no bounds must quote no-liquidity. Returns the count of executed
/// `(mint, redeem, swap)` directions.
fn sweep(venue: &ReflectBondVenue, svm: &mut LiteSVM) -> (usize, usize, usize) {
    let user = funded_user(svm);
    let receipt = venue.get_token(0).unwrap().pubkey;
    let mut rng = Rng::new(0x626f_6e64_5345_4544);
    let (mut mints, mut redeems, mut swaps) = (0, 0, 0);
    let n = venue.get_token_info().len() as u8;

    for (i, j) in (0..n).flat_map(|i| (0..n).filter(move |&j| j != i).map(move |j| (i, j))) {
        let (in_mint, out_mint) = (venue.get_token(i as usize).unwrap().pubkey, venue.get_token(j as usize).unwrap().pubkey);
        let Ok((lb, ub)) = venue.bounds(i, j) else {
            let q = venue.quote(exact_in(in_mint, out_mint, 1_000_000)).expect("quote");
            assert!(q.not_enough_liquidity, "{in_mint}->{out_mint}: no bounds, so the quote must flag no-liquidity");
            continue;
        };
        let mut amounts = vec![lb, ub];
        amounts.extend((0..6).map(|_| rng.log_uniform(lb, ub)));
        for amount in amounts {
            let req = exact_in(in_mint, out_mint, amount);
            let quote = venue.quote(req.clone()).expect("quote");
            assert!(!quote.not_enough_liquidity && quote.expected_output > 0, "{amount} {in_mint}->{out_mint} is inside the bounds {lb}..={ub}");
            // The source holds `u64::MAX` and the destination is empty before each run.
            fund_direction(svm, &user, venue, in_mint, out_mint);
            let (spent, gained) = simulate(svm, venue, &user, req).unwrap_or_else(|| panic!("{amount} {in_mint}->{out_mint} did not execute"));
            assert_eq!(gained, quote.expected_output, "dest delta != quote ({amount} {in_mint}->{out_mint})");
            // Mint and redeem may consume LESS than the request: the deposit allocator clamps
            // a single-sided deposit to the basket's imbalance caps, and the receipt-exact
            // redeem re-derives the burn from the achieved gross. Only overspend is a bug.
            assert!(spent <= amount, "source overspent ({amount} {in_mint}->{out_mint})");
            if in_mint != receipt && out_mint != receipt {
                assert_eq!(spent, amount, "swap must consume the exact input ({amount} {in_mint}->{out_mint})");
            }
        }
        match (in_mint == receipt, out_mint == receipt) {
            (false, true) => mints += 1,
            (true, false) => redeems += 1,
            _ => swaps += 1,
        }
        eprintln!("parity ok: {in_mint}->{out_mint} over {lb}..={ub}");
    }
    (mints, redeems, swaps)
}

/// The strategy exactly as it is on mainnet: every pair it can pay executes at the quote.
#[tokio::test(flavor = "current_thread")]
async fn parity_mainnet_state_swept() {
    let (venue, mut svm) = setup_live().await;
    let (mints, redeems, swaps) = sweep(&venue, &mut svm);
    eprintln!("directions executed: {mints} mint, {redeems} redeem, {swaps} swap");
    assert_eq!(mints, venue.get_token_info().len() - 1, "every basket asset must mint");
}

/// Every direction of the deepened basket: asset→receipt (mint), receipt→asset (redeem),
/// asset→asset (swap).
#[tokio::test(flavor = "current_thread")]
async fn parity_all_directions_swept() {
    let (venue, mut svm) = setup().await;
    let assets = venue.get_token_info().len() - 1;
    assert_eq!(sweep(&venue, &mut svm), (assets, assets, assets * (assets - 1)));
}

/// An absurd request maps to no-liquidity (never a lying quote), and the venue
/// stays quote-consistent on the zero edge (`bounds()` relies on both).
#[tokio::test(flavor = "current_thread")]
async fn edges_zero_and_overask() {
    let (venue, _svm) = setup().await;
    let (receipt, asset) = (venue.get_token(0).unwrap().pubkey, venue.get_token(1).unwrap().pubkey);

    let zero = venue.quote(exact_in(asset, receipt, 0)).expect("zero quote");
    assert_eq!(zero.expected_output, 0);
    assert!(!zero.not_enough_liquidity, "zero input is routable-with-zero-output");
    // Titan `price`: a positive spot at zero input that, on the linear mint, prices a
    // real fill to within Titan's 1e-5 mean-value tolerance.
    assert!(zero.price > 0.0, "spot must be positive, got {}", zero.price);
    let q = venue.quote(exact_in(asset, receipt, 1_000_000)).expect("probe quote");
    assert!((q.price * 1_000_000.0 - q.expected_output as f64).abs() <= q.expected_output as f64 * 1e-5 + 1.0, "price {} vs out {} at 1_000_000", q.price, q.expected_output);

    let over = venue.quote(exact_in(receipt, asset, u64::MAX)).expect("over quote");
    assert!(over.not_enough_liquidity, "over-ask must flag no-liquidity");
}

/// Titan's shared correctness contract (upstream `tests/common`) over the mainnet basket.
#[tokio::test(flavor = "current_thread")]
async fn titan_contract() {
    let (venue, mut svm) = setup().await;
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
    let (venue, _svm) = setup().await;
    assert_quotes_do_not_allocate(&venue);
}

/// Titan's performance contract (upstream `construction` / `quoting_speed`).
#[tokio::test(flavor = "current_thread")]
async fn quote_speed() {
    let (venue, _svm) = setup().await;
    assert_quote_speed(&venue, 0x7174_616e_5345_4544, 1_000.0);
}

/// Every basket direction executes inside the route program under TitanPDA custody.
#[tokio::test(flavor = "current_thread")]
async fn titan_route() {
    let Some(router) = router_program() else { return };
    let (venue, mut svm) = setup().await;
    assert_titan_route(&mut svm, &venue, &router);
}

/// Rewrite `snapshots/bond_basket.json` from mainnet: `RPC_URL=<url> cargo test -p
/// reflect-titan-bonds --test parity capture_snapshot -- --ignored`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "network: rewrites the snapshot from mainnet"]
async fn capture_snapshot() {
    let cache = RpcCache::new(&std::env::var("RPC_URL").expect("RPC_URL"));
    let venue: ReflectBondVenue = venue_over(&cache, &CONTROLLER).await;
    capture(&cache, &venue, &fixtures(), "bond_basket");
}
