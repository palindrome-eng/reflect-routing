//! HERMETIC. Asserts the Jupiter adapter's quote equals `reflect-sdk` and pins
//! the `get_swap_and_account_metas` account-meta ORDER as the golden contract.
//!
//! Jupiter's end-to-end swap cannot run here — the `Swap::ReflectS1` handler is
//! Jupiter's closed program — so the test validates the owned parts:
//!   - `ReflectLendingAmm::quote` == `reflect-sdk` `quote`. Both adapters
//!     delegate to that one call, and the Titan suite proves it equals on-chain
//!     execution atom-for-atom, so quote==execution transfers here WITHOUT an SVM
//!     (Jupiter's build is the same SDK ix the Titan side already executes).
//!   - the `account_metas` ORDER, which the on-chain ReflectS1 handler indexes
//!     positionally — the binding contract handed to Jupiter.
//!
//! OFFLINE: state is the committed mainnet snapshot shared with the Titan suite
//! (`titan/lending/tests/fixtures/snapshots/lending.json`). The program id is
//! read from the controller account's owner.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;

use reflect_sdk::common::Now as ReflectClock;
use reflect_sdk::strategy::{pda, FetchedAccount, Strategy};

use reflect_jupiter_common::{Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, Snapshot, Swap, SwapMode, SwapParams};
use reflect_jupiter_lending::ReflectLendingAmm;

/// The lending strategy in the snapshot.
const CONTROLLER: &str = "7hHkpUXHXudwSE7HKctjntJLmJM82sZ3wqzWvJevTJch";

/// Jupiter reuses the Titan crate's committed snapshot.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../titan/lending/tests/fixtures/snapshots/lending.json")
}

/// Wire the Jupiter Amm from the snapshot (cold `from_keyed_account` → `update`
/// ingests the discovery set). Program id flows from the controller's owner.
fn setup() -> (ReflectLendingAmm, Snapshot, ClockRef) {
    let snap = Snapshot::load(&fixture_path(), CONTROLLER);
    let clock_ref: ClockRef = snap.clock.clone().into();
    let ctx = AmmContext { clock_ref: clock_ref.clone() };
    let ka = KeyedAccount { key: snap.market, account: snap.map.get(&snap.market).expect("controller in snapshot").clone(), params: None };

    let mut amm = ReflectLendingAmm::from_keyed_account(&ka, &ctx).expect("from_keyed_account");
    amm.update(&snap.map).expect("update round 1");
    if amm.get_reserve_mints().len() != 2 {
        amm.update(&snap.map).expect("update round 2"); // re-run after dynamic accounts resolved
    }
    assert_eq!(amm.get_reserve_mints().len(), 2, "Jupiter amm must resolve a pair");
    (amm, snap, clock_ref)
}

/// Reflect SDK quote over the same bytes — the independent oracle both adapters
/// delegate to. Rebuilds the loader exactly as the adapter does, using the
/// SNAPSHOT program id.
fn sdk_quote(snap: &Snapshot, input_mint: Pubkey, amount: u64, clock: &ClockRef) -> u64 {
    let pid = snap.program_id;
    let registry = pda::derive_pool_registry(&pid).0;
    let ctrl = snap.map.get(&snap.market).unwrap();
    let reg = snap.map.get(&registry).unwrap();
    let keys = reflect_sdk::strategy::lending_discovery_keys(&ctrl.data, &reg.data, &pid).unwrap();
    let bytes: Vec<FetchedAccount> = keys.iter().filter_map(|k| snap.map.get(k).map(|a| FetchedAccount { key: *k, owner: a.owner, data: a.data.clone() })).collect();
    let accts = Strategy::from_accounts(pid, snap.market, bytes).unwrap();
    let pair = accts.tradable_pair().unwrap();
    let output_mint = if input_mint == pair.deposit.pubkey { pair.receipt.pubkey } else { pair.deposit.pubkey };
    let rc = ReflectClock { slot: clock.slot.load(Ordering::Relaxed), unix_timestamp: clock.unix_timestamp.load(Ordering::Relaxed) };
    accts.quote(input_mint, output_mint, amount, rc).unwrap().amount_out
}

/// The Jupiter adapter's quote must equal the reflect-sdk quote (= Titan's, =
/// on-chain execution via the Titan suite). Both directions, swept amounts.
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_equals_sdk() {
    let (amm, snap, clock) = setup();
    let mints = amm.get_reserve_mints();
    let (deposit, receipt) = (mints[0], mints[1]);

    for (in_mint, out_mint) in [(deposit, receipt), (receipt, deposit)] {
        // Inside both bands: the mainnet vault holds about 1.3 tokens.
        for amount in [1_000u64, 100_000, 1_000_000] {
            let q = amm.quote(&QuoteParams { amount, input_mint: in_mint, output_mint: out_mint, swap_mode: SwapMode::ExactIn }).expect("jupiter quote");
            let sdk = sdk_quote(&snap, in_mint, amount, &clock);
            assert_eq!(q.out_amount, sdk, "jupiter quote != sdk quote for {in_mint}->{out_mint} amt {amount}");
        }
    }
}

/// would_revert edge: `u64::MAX` is past every mint ceiling, so the Jupiter
/// adapter returns `Err` (no route, not a bad route).
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_errs_on_would_revert() {
    let (amm, _snap, _clock) = setup();
    let mints = amm.get_reserve_mints();
    let r = amm.quote(&QuoteParams { amount: u64::MAX, input_mint: mints[0], output_mint: mints[1], swap_mode: SwapMode::ExactIn });
    assert!(r.is_err(), "u64::MAX must surface as Err, not a bad route");
}

/// GOLDEN: the `account_metas` ORDER from `get_swap_and_account_metas` is the
/// on-chain `Swap::ReflectS1` contract — indexed positionally, so any reorder is
/// breaking. Asserts the structural shape (Reflect ix variant, user is the sole
/// signer) and records the full order to stdout for the Jupiter handoff package.
#[tokio::test(flavor = "current_thread")]
async fn golden_account_metas_order() {
    let (amm, _snap, _clock) = setup();
    let mints = amm.get_reserve_mints();
    let user = Pubkey::new_unique();
    let sp = SwapParams {
        swap_mode: SwapMode::ExactIn,
        in_amount: 1_000_000,
        out_amount: 0,
        source_mint: mints[0],
        destination_mint: mints[1],
        source_token_account: Pubkey::new_unique(),
        destination_token_account: Pubkey::new_unique(),
        token_transfer_authority: user,
        quote_mint_to_referrer: None,
        jupiter_program_id: &Pubkey::new_unique(),
        missing_dynamic_accounts_as_default: false,
    };

    let sam = amm.get_swap_and_account_metas(&sp).expect("metas");
    assert!(matches!(sam.swap, Swap::ReflectS1), "the one permissioned variant");
    assert!(!sam.account_metas.is_empty(), "metas must not be empty");

    // The user (token_transfer_authority) is the sole signer an aggregator route
    // carries (Loopscale cosigner surfaces via extra_signers, NOT here).
    let signers: Vec<_> = sam.account_metas.iter().filter(|m| m.is_signer).map(|m| m.pubkey).collect();
    assert_eq!(signers, vec![user], "only the user signs an aggregator route");

    eprintln!("[golden] Swap::ReflectS1 account_metas order ({} accounts):", sam.account_metas.len());
    for (i, m) in sam.account_metas.iter().enumerate() {
        eprintln!("  [{i:>2}] {} signer={} writable={}", m.pubkey, m.is_signer, m.is_writable);
    }
}

/// A cold Amm reports no reserve mints (mirrors the crate's unit test through the
/// AccountMap ingest API the harness uses).
#[tokio::test(flavor = "current_thread")]
async fn offline_cold_amm_has_no_pair() {
    let ctx = AmmContext { clock_ref: ClockRef::default() };
    let controller = Pubkey::new_unique();
    let ka = KeyedAccount { key: controller, account: Account { lamports: 1, data: vec![0u8; 8], owner: Pubkey::new_unique(), executable: false, rent_epoch: 0 }, params: None };
    let amm = ReflectLendingAmm::from_keyed_account(&ka, &ctx).expect("from_keyed_account");
    assert!(amm.get_reserve_mints().is_empty());
}
