//! Off-chain `quote(in, out, amount)` must equal the on-chain execution delta,
//! atom-exact, for every bond direction (asset→receipt mint, receipt→asset
//! redeem, asset→asset swap), over an amount sweep.
//!
//! Regenerate:
//! `cd reflect-core/programs/reflect-main && DUMP_FORGE_SNAPSHOT=<this crate>/tests/fixtures/forged_bond_basket.json cargo test --test bond_forge_dump_tests`

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use litesvm::LiteSVM;
use solana_account::Account;
use solana_pubkey::Pubkey;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

use reflect_titan_bonds::ReflectBondVenue;
use reflect_titan_common::account_caching::{AccountCacheError, AccountsCache};
use reflect_titan_common::trading_venue::{FromAccount, QuoteRequest, SwapType, TradingVenue};

fn manifest_rel(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn fixture_path() -> PathBuf {
    manifest_rel("tests/fixtures/forged_bond_basket.json")
}

fn pk(s: &str) -> Pubkey {
    s.parse().expect("pubkey")
}

/// Parsed fixture: the forge program id, the controller PDA, the deterministic
/// user (LiteSVM verifies signatures, so the secret rides in the fixture), the
/// forge Clock, and every forged account keyed by pubkey.
struct Snapshot {
    program_id: Pubkey,
    controller: Pubkey,
    user: Keypair,
    clock: SolanaClock,
    map: HashMap<Pubkey, Account>,
}

/// Read `forged_bond_basket.json` (the same cached-fixture shape as lending's
/// `forged_reserve_kamino.json`). Synthesizes the Clock sysvar account from the
/// dumped slot/unix so the adapter's `update_state` resolves CLOCK_SYSVAR.
fn load_snapshot() -> Snapshot {
    let raw = std::fs::read(fixture_path()).expect("read fixture");
    let v: serde_json::Value = serde_json::from_slice(&raw).expect("parse fixture");

    let program_id = pk(v["program_id"].as_str().unwrap());
    let controller = pk(v["controller"].as_str().unwrap());
    let user = Keypair::from_bytes(&B64.decode(v["user_secret"].as_str().unwrap()).unwrap()).expect("user keypair");
    // No cloned external programs here (unlike lending's Kamino), so the forge
    // clock is used verbatim: the synthetic oracles publish at 1_700_000_000
    // with 300s staleness and the forge pins unix to +60.
    let clock = SolanaClock { slot: v["slot"].as_u64().unwrap(), unix_timestamp: v["unix"].as_i64().unwrap(), ..Default::default() };

    let mut map = HashMap::new();
    for rec in v["accounts"].as_array().unwrap() {
        let key = pk(rec["pubkey"].as_str().unwrap());
        let a = &rec["account"];
        let data = B64.decode(a["data"][0].as_str().unwrap()).expect("b64");
        map.insert(
            key,
            Account {
                lamports: a["lamports"].as_u64().unwrap(),
                data,
                owner: pk(a["owner"].as_str().unwrap()),
                executable: a["executable"].as_bool().unwrap(),
                rent_epoch: a["rentEpoch"].as_u64().unwrap(),
            },
        );
    }

    // The forge never pushes the Clock sysvar into its account store; the
    // adapter fetches it for the quote timestamp. Synthesize it from the meta.
    let clock_id = sdk_to_pk(&solana_sdk::sysvar::clock::ID);
    map.insert(
        clock_id,
        Account { lamports: 1, data: bincode::serialize(&clock).expect("ser clock"), owner: sdk_to_pk(&solana_sdk::sysvar::id()), executable: false, rent_epoch: 0 },
    );

    Snapshot { program_id, controller, user, clock, map }
}

/// [`AccountsCache`] over the snapshot. Contract: order-preserving, miss → None.
struct SnapshotCache {
    map: HashMap<Pubkey, Account>,
}

#[async_trait]
impl AccountsCache for SnapshotCache {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, AccountCacheError> {
        Ok(self.map.get(pubkey).cloned())
    }
    async fn get_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>, AccountCacheError> {
        Ok(pubkeys.iter().map(|k| self.map.get(k).cloned()).collect())
    }
}

// solana_account/solana_pubkey (adapter) and solana_sdk (litesvm) are the same
// wire layout under unified 2.2, but distinct types; bridge by bytes regardless.
fn sdk_key(k: &Pubkey) -> solana_sdk::pubkey::Pubkey {
    solana_sdk::pubkey::Pubkey::new_from_array(k.to_bytes())
}
fn sdk_to_pk(k: &solana_sdk::pubkey::Pubkey) -> Pubkey {
    Pubkey::new_from_array(k.to_bytes())
}
fn to_sdk_account(a: &Account) -> solana_sdk::account::Account {
    solana_sdk::account::Account { lamports: a.lamports, data: a.data.clone(), owner: sdk_key(&a.owner), executable: a.executable, rent_epoch: a.rent_epoch }
}

/// A LiteSVM with the forged `reflect_main.so` loaded and every non-program
/// forged account set; the Clock is warped to the forge meta. `None` (skip)
/// when the committed program is absent.
fn load_svm(snap: &Snapshot) -> Option<LiteSVM> {
    let so = std::fs::read(manifest_rel("tests/fixtures/programs/reflect_main.so")).ok().or_else(|| {
        eprintln!("[skip] no reflect_main.so - regenerate via reflect-core bond_forge_dump_tests");
        None
    })?;
    let mut svm = LiteSVM::new();
    svm.add_program(sdk_key(&snap.program_id), &so);

    let clock_id = sdk_to_pk(&solana_sdk::sysvar::clock::ID);
    for (k, acc) in &snap.map {
        if acc.executable || *k == clock_id {
            continue;
        }
        let _ = svm.set_account(sdk_key(k), to_sdk_account(acc));
    }
    svm.set_sysvar::<SolanaClock>(&snap.clock);
    Some(svm)
}

/// Build the venue from the forged controller and `update_state` it off the
/// snapshot. Returns (venue, snapshot, receipt mint, basket asset mints).
async fn setup() -> (ReflectBondVenue, Snapshot, Pubkey, Vec<Pubkey>) {
    let snap = load_snapshot();
    let ctrl = snap.map.get(&snap.controller).expect("controller in fixture").clone();
    let mut venue = ReflectBondVenue::from_account(&snap.controller, &ctrl).expect("from_account");
    let cache = SnapshotCache { map: snap.map.clone() };
    venue.update_state(&cache).await.expect("update_state");

    // mints() = [receipt, asset_0..n] - the venue's documented order.
    let toks: Vec<Pubkey> = venue.get_token_info().iter().map(|t| t.pubkey).collect();
    assert!(toks.len() >= 3, "expected receipt + 2 basket assets, got {}", toks.len());
    (venue, snap, toks[0], toks[1..].to_vec())
}

fn user_ata(user: &Keypair, mint: &Pubkey) -> solana_sdk::pubkey::Pubkey {
    spl_associated_token_account::get_associated_token_address(&user.pubkey(), &sdk_key(mint))
}

fn token_balance(svm: &LiteSVM, ata: solana_sdk::pubkey::Pubkey) -> u64 {
    use solana_program_pack::Pack;
    let acc = svm.get_account(&ata).expect("ata exists");
    spl_token::state::Account::unpack(&acc.data).expect("token account").amount
}

/// Quote `amount` of `in_mint` → `out_mint`, execute the adapter-built ix in a
/// FRESH svm off the snapshot, and assert the user's dest delta equals the
/// quote atom-exact (plus the ExactIn source contract).
fn assert_direction(venue: &ReflectBondVenue, snap: &Snapshot, in_mint: Pubkey, out_mint: Pubkey, amount: u64) {
    let req = QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount, swap_type: SwapType::ExactIn };
    let quote = venue.quote(req.clone()).expect("quote");
    assert!(!quote.not_enough_liquidity, "expected liquidity for {amount} {in_mint}->{out_mint}");
    assert!(quote.expected_output > 0);

    let ix = venue.generate_swap_instruction(req, sdk_to_pk(&snap.user.pubkey())).expect("build");
    let Some(mut svm) = load_svm(snap) else { return };
    svm.airdrop(&snap.user.pubkey(), 1_000_000_000).expect("airdrop");

    let (src, dst) = (user_ata(&snap.user, &in_mint), user_ata(&snap.user, &out_mint));
    let (src_before, dst_before) = (token_balance(&svm, src), token_balance(&svm, dst));

    let sdk_ix = solana_sdk::instruction::Instruction {
        program_id: sdk_key(&ix.program_id),
        accounts: ix.accounts.iter().map(|m| solana_sdk::instruction::AccountMeta { pubkey: sdk_key(&m.pubkey), is_signer: m.is_signer, is_writable: m.is_writable }).collect(),
        data: ix.data.clone(),
    };
    // The forge harness ran Mollusk with a 256KiB heap and bond flows burn
    // ~200k CU; request both so LiteSVM matches.
    let heap = solana_sdk::compute_budget::ComputeBudgetInstruction::request_heap_frame(256 * 1024);
    let cu = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    let tx = Transaction::new_signed_with_payer(&[heap, cu, sdk_ix], Some(&snap.user.pubkey()), &[&snap.user], svm.latest_blockhash());
    if let Err(e) = svm.send_transaction(tx) {
        for l in &e.meta.logs {
            eprintln!("    {l}");
        }
        panic!("execution failed for {amount} {in_mint}->{out_mint}: {:?}", e.err);
    }

    let dst_delta = token_balance(&svm, dst) - dst_before;
    let src_delta = src_before - token_balance(&svm, src);
    assert_eq!(dst_delta, quote.expected_output, "dest delta != quote ({amount} {in_mint}->{out_mint})");
    // Mint and redeem legs may consume LESS than the request: the deposit
    // allocator clamps a single-sided deposit to the basket's imbalance caps,
    // and the constructed receipt-exact redeem re-derives the burn from the
    // achieved gross. Quote and execution clamp identically (the dest assert
    // above is the proof); only overspend is a bug. Swap is spend-exact.
    assert!(src_delta <= amount, "source overspent ({amount} {in_mint}->{out_mint})");
    let toks = venue.get_token_info();
    let receipt = toks[0].pubkey;
    if in_mint != receipt && out_mint != receipt {
        assert_eq!(src_delta, amount, "swap must consume the exact input ({amount} {in_mint}->{out_mint})");
    }
}

/// Every direction of the basket, swept: asset→receipt (mint), receipt→asset
/// (constructed receipt-exact redeem), asset→asset (swap).
#[tokio::test(flavor = "current_thread")]
async fn parity_all_directions_swept() {
    let (venue, snap, receipt, assets) = setup().await;
    if load_svm(&snap).is_none() {
        return; // program absent - loader already printed the skip reason
    }

    for amount in [1_000u64, 250_000, 5_000_000] {
        assert_direction(&venue, &snap, assets[0], receipt, amount); // mint
        assert_direction(&venue, &snap, receipt, assets[0], amount); // redeem
        assert_direction(&venue, &snap, assets[0], assets[1], amount); // swap
        assert_direction(&venue, &snap, assets[1], assets[0], amount); // swap back
    }
}

/// An absurd request maps to no-liquidity (never a lying quote), and the venue
/// stays quote-consistent on the zero edge (`bounds()` relies on both).
#[tokio::test(flavor = "current_thread")]
async fn edges_zero_and_overask() {
    let (venue, snap, receipt, assets) = setup().await;
    let _ = snap;

    let zero = venue.quote(QuoteRequest { input_mint: assets[0], output_mint: receipt, amount: 0, swap_type: SwapType::ExactIn }).expect("zero quote");
    assert_eq!(zero.expected_output, 0);
    assert!(!zero.not_enough_liquidity, "zero input is routable-with-zero-output");

    let over = venue.quote(QuoteRequest { input_mint: receipt, output_mint: assets[0], amount: u64::MAX, swap_type: SwapType::ExactIn }).expect("over quote");
    assert!(over.not_enough_liquidity, "over-ask must flag no-liquidity");
}
