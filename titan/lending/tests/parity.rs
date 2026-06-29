//! Off-chain `quote(amount)` must equal the on-chain execution delta, atom-exact,
//! both directions, over an amount sweep. Both adapters delegate quote/build to
//! the same `reflect-sdk` math, so executing the Titan venue's
//! [`ReflectLendingVenue::generate_swap_instruction`] (a complete Reflect
//! mint/redeem ix) proves the property for Titan AND Jupiter.
//!
//! The controller is not on mainnet: it is FORGED by `reflect_main`'s ix sequence
//! over Mollusk in reflect-core — which pins solana 2.1 and CANNOT link with
//! these 2.2 adapters — then dumped to `tests/fixtures/`. This harness loads that
//! snapshot (a real, program-created Kamino obligation with a non-trivial
//! position) plus the cloned Kamino `.so`s into one LiteSVM, drives the adapter's
//! quote, re-executes the adapter-built ix in the SAME instance, and asserts the
//! dest-balance delta equals the quote.
//!
//! PORTABLE/OFFLINE: quote-only tests need just the committed JSON fixture.
//! Execution needs three programs via [`resolve_programs`] — `reflect_main.so` is
//! committed (its forge id is not on mainnet, so unfetchable); the Kamino `.so`s
//! (10MB) are RPC-dumped + cached on first run with `SOLANA_RPC_URL`, else taken
//! from a sibling reflect-core checkout, else the execution tests skip.
//!
//! Regenerate the fixture (reflect-core stays clean): apply
//! `tests/fixtures/forge-dump.patch` to a reflect-core checkout, run
//! `min_provided_routes_gate_in_lending_mint` with `DUMP_FORGE_SNAPSHOT=<path>`,
//! then revert the patch.

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

use reflect_sdk::common::ids::{TOKEN_2022_PROGRAM, TOKEN_PROGRAM};

use reflect_titan_lending::account_caching::{AccountCacheError, AccountsCache};
use reflect_titan_lending::trading_venue::{FromAccount, QuoteRequest, SwapType, TradingVenue};
use reflect_titan_lending::ReflectLendingVenue;

// Owners of the cloned Kamino state in the snapshot; their `.so`s live under
// reflect-core's gitignored `tests/fixtures/` (not committed — built/dumped).
const KAMINO_LEND: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
const KAMINO_FARMS: &str = "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr";

fn manifest_rel(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}
fn fixture_path() -> PathBuf {
    manifest_rel("tests/fixtures/forged_reserve_kamino.json")
}
/// Where resolved program `.so`s live in THIS repo. `reflect_main.so` is
/// committed here (the forged program — paired with the fixture); the Kamino
/// blobs are RPC-fetched + cached here and gitignored.
fn programs_dir() -> PathBuf {
    manifest_rel("tests/fixtures/programs")
}
// reflect-core is a sibling checkout (a fallback for the dev who regenerates).
const CORE: &str = "../../../reflect-core";

/// The program bytes needed to EXECUTE, resolved once and reused across the
/// per-amount LiteSVM rebuilds.
struct Programs {
    reflect: Vec<u8>,
    klend: Vec<u8>,
    kfarms: Vec<u8>,
}

/// Resolve all three programs, or `None` (with a skip reason) when a fresh clone
/// can't get them — keeping `cargo test` green offline. Kamino blobs RPC-dump on
/// first run when `SOLANA_RPC_URL` is set; `reflect_main.so` cannot (its forged
/// program id is not deployed), so it must be committed or built in the sibling.
fn resolve_programs(test: &str) -> Option<Programs> {
    let reflect = read_first(&[programs_dir().join("reflect_main.so"), manifest_rel(CORE).join("target/deploy/reflect_main.so")]).or_else(|| {
        eprintln!("[skip {test}] no reflect_main.so (commit tests/fixtures/programs/reflect_main.so or build reflect-core)");
        None
    })?;
    let klend = resolve_cloned(test, "kamino_lend_program_2.so", KAMINO_LEND, manifest_rel(CORE).join("programs/reflect-main/tests/fixtures/kamino_lend/kamino_lend_program_2.so"))?;
    let kfarms = resolve_cloned(test, "kamino_farm_program_2.so", KAMINO_FARMS, manifest_rel(CORE).join("programs/reflect-main/tests/fixtures/kamino_farms/kamino_farm_program_2.so"))?;
    Some(Programs { reflect, klend, kfarms })
}

fn read_first(paths: &[PathBuf]) -> Option<Vec<u8>> {
    paths.iter().find(|p| p.exists()).and_then(|p| std::fs::read(p).ok())
}

/// A cloned mainnet program: local cache → sibling reflect-core fixture → RPC
/// dump into the local cache (needs `SOLANA_RPC_URL`).
fn resolve_cloned(test: &str, file: &str, program_id: &str, sibling: PathBuf) -> Option<Vec<u8>> {
    let cache = programs_dir().join(file);
    if let Some(b) = read_first(&[cache.clone(), sibling]) {
        return Some(b);
    }
    let Ok(url) = std::env::var("SOLANA_RPC_URL") else {
        eprintln!("[skip {test}] {file} absent and SOLANA_RPC_URL unset — set it to fetch {program_id} once");
        return None;
    };
    eprintln!("[parity] dumping {program_id} from RPC → {}", cache.display());
    let so = dump_program(&url, &pk(program_id))?;
    let _ = std::fs::create_dir_all(programs_dir());
    let _ = std::fs::write(&cache, &so);
    Some(so)
}

/// Fetch a BPF-upgradeable program's ELF bytes over JSON-RPC (`solana program
/// dump` equivalent): the Program account holds the ProgramData pubkey; the ELF
/// follows the ProgramData metadata header. The blocking `RpcClient` spins its
/// own runtime, so it runs on a plain thread — calling it from the `#[tokio::test]`
/// context directly panics ("runtime within a runtime").
fn dump_program(url: &str, program_id: &Pubkey) -> Option<Vec<u8>> {
    let (url, id) = (url.to_string(), *program_id);
    std::thread::spawn(move || {
        use solana_client::rpc_client::RpcClient;
        let rpc = RpcClient::new(url);
        let prog = rpc.get_account(&sdk_key(&id)).ok()?;
        // Upgradeable Program account: enum tag (4) + ProgramData pubkey (32).
        let programdata = solana_sdk::pubkey::Pubkey::try_from(prog.data.get(4..36)?).ok()?;
        let pd = rpc.get_account(&programdata).ok()?;
        // ProgramData metadata: enum(4) + slot(8) + Option<Pubkey>(1+32) = 45, then ELF.
        pd.data.get(45..).map(<[u8]>::to_vec)
    })
    .join()
    .ok()?
}

/// Parsed fixture: the program id the controller is owned by, the controller
/// PDA, the forge Clock, and every forged account keyed by pubkey.
struct Snapshot {
    program_id: Pubkey,
    controller: Pubkey,
    /// The privileged forge admin — the ONLY signer `user_permission` grants the
    /// Mint/Redeem action. Its creds + ATAs are in `map`; LiteSVM verifies the
    /// signature, so the secret is carried in the fixture.
    admin: Keypair,
    clock: SolanaClock,
    map: HashMap<Pubkey, Account>,
}

fn pk(s: &str) -> Pubkey {
    s.parse().expect("pubkey")
}

/// Read `forged_reserve_kamino.json` (the cached-fixture JSON shape) into a
/// [`Snapshot`]. Synthesizes the Clock sysvar account from the dumped slot/unix
/// so the adapter's `update_state` (which fetches CLOCK_SYSVAR) resolves it.
fn load_snapshot() -> Snapshot {
    let raw = std::fs::read(fixture_path()).expect("read fixture");
    let v: serde_json::Value = serde_json::from_slice(&raw).expect("parse fixture");

    let program_id = pk(v["program_id"].as_str().unwrap());
    let controller = pk(v["controller"].as_str().unwrap());
    let admin = Keypair::from_bytes(&B64.decode(v["admin_secret"].as_str().unwrap()).unwrap()).expect("admin keypair");
    // Kamino's RefreshReserve rejects a price older than 180s. The forge's clock
    // passed under Mollusk, but LiteSVM lands ~2s later against the same cloned
    // `scope_prices` (age=182 observed). Roll the clock back a margin so the
    // cloned price stays fresh. ponytail: 120s margin; re-tune if a fixture
    // refresh moves the price timestamp.
    const STALENESS_MARGIN_S: i64 = 120;
    let clock = SolanaClock { slot: v["slot"].as_u64().unwrap(), unix_timestamp: v["unix"].as_i64().unwrap() - STALENESS_MARGIN_S, ..Default::default() };

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
    // adapter needs it for the quote timestamp. Synthesize it from the meta.
    let clock_id = sdk_to_pk(&solana_sdk::sysvar::clock::ID);
    map.insert(
        clock_id,
        Account { lamports: 1, data: bincode::serialize(&clock).expect("ser clock"), owner: sdk_to_pk(&solana_sdk::sysvar::id()), executable: false, rent_epoch: 0 },
    );

    Snapshot { program_id, controller, admin, clock, map }
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

/// A LiteSVM with the forged program + the cloned Kamino programs loaded and
/// every non-program forged account set. The Clock is warped to the forge slot
/// so Kamino's 180s oracle-staleness check passes against the cloned
/// `scope_prices`.
fn load_svm(snap: &Snapshot, p: &Programs) -> LiteSVM {
    let mut svm = LiteSVM::new();
    svm.add_program(sdk_key(&snap.program_id), &p.reflect);
    svm.add_program(sdk_key(&pk(KAMINO_LEND)), &p.klend);
    svm.add_program(sdk_key(&pk(KAMINO_FARMS)), &p.kfarms);

    // Programs are loaded above; everything else is plain forged state. Skip
    // executable stubs (the loaders' program accounts) and the synthetic Clock
    // (set via set_sysvar below for the runtime to honor it).
    let clock_id = sdk_to_pk(&solana_sdk::sysvar::clock::ID);
    for (k, acc) in &snap.map {
        if acc.executable || *k == clock_id {
            continue;
        }
        let _ = svm.set_account(sdk_key(k), to_sdk_account(acc));
    }
    svm.set_sysvar::<SolanaClock>(&snap.clock);
    svm
}

/// Build the adapter swap ix, fund synthetic source/dest ATAs (source `u64::MAX`,
/// dest `0`), execute in the SVM, and return the dest balance delta. The
/// atom-exact comparison happens at the call site.
fn sim_delta(svm: &mut LiteSVM, venue: &ReflectLendingVenue, user: &Keypair, in_mint: Pubkey, out_mint: Pubkey, amount: u64, in_tp: Pubkey, out_tp: Pubkey) -> Option<u64> {
    let req = QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount, swap_type: SwapType::ExactIn };
    let ix = match venue.generate_swap_instruction(req, sdk_to_pk(&user.pubkey())) {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("[sim_delta] generate_swap_instruction failed amt {amount}: {e:?}");
            return None;
        },
    };

    let src_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user.pubkey(), &sdk_key(&in_mint), &sdk_key(&in_tp));
    let dst_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user.pubkey(), &sdk_key(&out_mint), &sdk_key(&out_tp));
    set_token_account(svm, src_ata, sdk_key(&in_mint), user.pubkey(), u64::MAX, in_tp);
    set_token_account(svm, dst_ata, sdk_key(&out_mint), user.pubkey(), 0, out_tp);
    svm.airdrop(&user.pubkey(), 1_000_000_000).ok()?;

    let before = token_balance(svm, dst_ata)?;
    let sdk_ix = solana_sdk::instruction::Instruction {
        program_id: sdk_key(&ix.program_id),
        accounts: ix.accounts.iter().map(|m| solana_sdk::instruction::AccountMeta { pubkey: sdk_key(&m.pubkey), is_signer: m.is_signer, is_writable: m.is_writable }).collect(),
        data: ix.data.clone(),
    };
    // The Kamino CPIs (RefreshReserve/Obligation + supply) blow past the 200k
    // default; the forge ran at 1.4M. Match it.
    let cu = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    let tx = Transaction::new_signed_with_payer(&[cu, sdk_ix], Some(&user.pubkey()), &[user], svm.latest_blockhash());
    if let Err(e) = svm.send_transaction(tx) {
        eprintln!("[sim_delta] tx failed amt {amount} {in_mint}->{out_mint}:");
        for l in &e.meta.logs {
            eprintln!("    {l}");
        }
        eprintln!("    err: {:?}", e.err);
        return None;
    }

    let after = token_balance(svm, dst_ata)?;
    Some(after.saturating_sub(before))
}

/// Write a minimal SPL token account (amount/mint/owner). Token-2022's base
/// layout is byte-identical for these fields.
fn set_token_account(svm: &mut LiteSVM, ata: solana_sdk::pubkey::Pubkey, mint: solana_sdk::pubkey::Pubkey, owner: solana_sdk::pubkey::Pubkey, amount: u64, token_program: Pubkey) {
    use solana_program_pack::Pack;
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    let state = spl_token::state::Account {
        mint: spl_token::solana_program::pubkey::Pubkey::new_from_array(mint.to_bytes()),
        owner: spl_token::solana_program::pubkey::Pubkey::new_from_array(owner.to_bytes()),
        amount,
        state: spl_token::state::AccountState::Initialized,
        ..Default::default()
    };
    spl_token::state::Account::pack(state, &mut data).expect("pack token account");
    let acc = solana_sdk::account::Account { lamports: 2_039_280, data, owner: sdk_key(&token_program), executable: false, rent_epoch: 0 };
    let _ = svm.set_account(ata, acc);
}

fn token_balance(svm: &LiteSVM, ata: solana_sdk::pubkey::Pubkey) -> Option<u64> {
    use solana_program_pack::Pack;
    let acc = svm.get_account(&ata)?;
    spl_token::state::Account::unpack(&acc.data).ok().map(|a| a.amount)
}

/// Build the venue from the forged controller account and `update_state` it off
/// the snapshot. Returns the venue + snapshot + the (mint, token-program) pairs.
async fn setup() -> Option<(ReflectLendingVenue, Snapshot, (Pubkey, Pubkey), (Pubkey, Pubkey))> {
    let snap = load_snapshot();
    let ctrl = snap.map.get(&snap.controller)?.clone();
    let mut venue = ReflectLendingVenue::from_account(&snap.controller, &ctrl).ok()?;
    let cache = SnapshotCache { map: snap.map.clone() };
    venue.update_state(&cache).await.ok()?;

    let toks = venue.get_token_info().to_vec();
    if toks.len() != 2 {
        eprintln!("[skip] venue resolved {} tokens, expected 2", toks.len());
        return None;
    }

    let (deposit, receipt) = (&toks[0], &toks[1]);
    let tp = |t22: bool| if t22 { TOKEN_2022_PROGRAM } else { TOKEN_PROGRAM };
    Some((venue, snap, (deposit.pubkey, tp(deposit.is_token_2022)), (receipt.pubkey, tp(receipt.is_token_2022))))
}

/// Log-uniform amount sweep over `[lo, hi]`, `n` samples, deterministic seed.
fn log_uniform_samples(lo: u64, hi: u64, n: usize) -> Vec<u64> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(0x5EED_u64.wrapping_add(lo).wrapping_add(hi));
    let (l, h) = ((lo.max(1) as f64).ln(), (hi.max(2) as f64).ln());
    (0..n).map(|_| (rng.gen_range(l..=h)).exp() as u64).map(|x| x.clamp(lo, hi)).collect()
}

// Empirical safe bands for the forged cell (deposit_amount=1e9, controller cap
// 1e15). MINT is atom-exact up to the cap, but `quote()` does NOT model the cap
// (see `mint_quote_ignores_cap_above_mintable`), so the mint sweep stays a
// safe margin below it. REDEEM is bounded by the SupplyFloor would_revert, which
// `bounds(1,0)` finds correctly; tiny redeems revert on Kamino withdraw dust, so
// the redeem sweep starts above a dust floor.
const MINT_LO: u64 = 1_000;
const MINT_HI: u64 = 1_000_000_000_000; // 1e12 — well under the 1e15 cap
const REDEEM_LO: u64 = 1_000_000; // above the withdraw-dust revert threshold

/// `delta.abs_diff(quote.expected_output) == 0` — exact, both directions, across
/// the band endpoints + a log-uniform sweep (≥50 each).
#[tokio::test(flavor = "current_thread")]
async fn parity_swept_both_directions() {
    let Some(programs) = resolve_programs("parity_swept_both_directions") else {
        return;
    };
    let Some((venue, snap, deposit, receipt)) = setup().await else {
        eprintln!("[skip] snapshot did not yield a tradable venue");
        return;
    };

    // (in, out, lo, hi) per direction. Mint uses a fixed sub-cap band; redeem
    // uses the venue's own (correct) would_revert ceiling from bounds(1,0).
    let (rlo, rhi) = venue.bounds(1, 0).expect("redeem bounds");
    let dirs = [(deposit.0, deposit.1, receipt.0, receipt.1, MINT_LO, MINT_HI), (receipt.0, receipt.1, deposit.0, deposit.1, REDEEM_LO.max(rlo), rhi.saturating_sub(rhi / 100))];

    for (in_mint, in_tp, out_mint, out_tp, lo, hi) in dirs {
        let mut amounts = vec![lo, hi];
        amounts.extend(log_uniform_samples(lo, hi, 50));
        for amt in amounts {
            let q = venue.quote(QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount: amt, swap_type: SwapType::ExactIn }).expect("quote");
            assert!(!q.not_enough_liquidity, "amount {amt} unexpectedly outside the routable band {lo}..={hi}");
            let mut svm = load_svm(&snap, &programs);
            let delta = sim_delta(&mut svm, &venue, &snap.admin, in_mint, out_mint, amt, in_tp, out_tp).expect("simulate");
            assert_eq!(delta.abs_diff(q.expected_output), 0, "atom-exact parity broke: dir {in_mint}->{out_mint} amount {amt}: quote {} sim {delta}", q.expected_output);
        }
    }
}

/// mint → redeem ≈ identity within rounding tolerance (~1%). Quote-only — runs
/// from the committed fixture, no `.so`s needed.
#[tokio::test(flavor = "current_thread")]
async fn parity_roundtrip_identity() {
    if !fixture_path().exists() {
        eprintln!("[skip parity_roundtrip_identity] missing fixture");
        return;
    }
    let Some((venue, _snap, deposit, receipt)) = setup().await else {
        return;
    };
    let amount = 100_000_000u64; // 100 USDC — mid of both directions' valid bands

    let q1 = venue.quote(QuoteRequest { input_mint: deposit.0, output_mint: receipt.0, amount, swap_type: SwapType::ExactIn }).expect("mint quote");
    assert!(q1.expected_output > 0);
    let q2 = venue.quote(QuoteRequest { input_mint: receipt.0, output_mint: deposit.0, amount: q1.expected_output, swap_type: SwapType::ExactIn }).expect("redeem quote");

    let diff = (amount as i64 - q2.expected_output as i64).unsigned_abs();
    assert!(diff <= amount / 100, "roundtrip drifted >1%: {amount} -> {} -> {} (diff {diff})", q1.expected_output, q2.expected_output);
}

/// Edges: amount 0 → clean 0 (never panics); and the REDEEM supply-floor
/// correctly surfaces `not_enough_liquidity` (the adapter's would_revert path).
/// Quote-only.
#[tokio::test(flavor = "current_thread")]
async fn edges_zero_and_redeem_floor() {
    if !fixture_path().exists() {
        eprintln!("[skip edges_zero_and_redeem_floor] missing fixture");
        return;
    }
    let Some((venue, _snap, deposit, receipt)) = setup().await else {
        return;
    };
    for (a, b) in [(deposit.0, receipt.0), (receipt.0, deposit.0)] {
        let z = venue.quote(QuoteRequest { input_mint: a, output_mint: b, amount: 0, swap_type: SwapType::ExactIn }).expect("zero quote");
        assert_eq!(z.expected_output, 0);
        assert!(!z.not_enough_liquidity, "amount 0 is not a liquidity failure");
    }

    // Redeeming far past the position breaches the supply floor → would_revert →
    // not_enough_liquidity. The ceiling lives at bounds(1,0).hi.
    let (_, rhi) = venue.bounds(1, 0).expect("redeem bounds");
    let over = venue.quote(QuoteRequest { input_mint: receipt.0, output_mint: deposit.0, amount: rhi.saturating_mul(4), swap_type: SwapType::ExactIn }).expect("over quote");
    assert!(over.not_enough_liquidity, "redeem past the supply floor must report no liquidity");
}

/// MINT cap parity: above the strategy mint cap the adapter quote now reports
/// `not_enough_liquidity` (the SDK mirrors `validate_cap_limits`:
/// `receipt_supply + receipt_out <= base.cap`), AND on-chain `mint_lending`
/// reverts `CapExceeded`. Quote and execution AGREE at the cap — no
/// over-promise. (Previously the quote over-promised; SDK fix in
/// `LendingQuoteCtx::quote_mint`.)
#[tokio::test(flavor = "current_thread")]
async fn mint_quote_caps_above_mintable() {
    let Some(programs) = resolve_programs("mint_quote_caps_above_mintable") else {
        return;
    };
    let Some((venue, snap, deposit, receipt)) = setup().await else {
        return;
    };
    let over_cap = 2_000_000_000_000_000u64; // 2× the 1e15 controller cap

    let q = venue.quote(QuoteRequest { input_mint: deposit.0, output_mint: receipt.0, amount: over_cap, swap_type: SwapType::ExactIn }).expect("quote");
    assert!(q.not_enough_liquidity, "over-cap mint quote must report no liquidity (cap modeled)");
    assert_eq!(q.expected_output, 0, "no-liquidity quote outputs 0");

    // …and execution reverts CapExceeded — sim_delta returns None. Quote agrees.
    let mut svm = load_svm(&snap, &programs);
    let sim = sim_delta(&mut svm, &venue, &snap.admin, deposit.0, receipt.0, over_cap, deposit.1, receipt.1);
    assert!(sim.is_none(), "over-cap mint must revert on-chain");
}

/// Offline: the SnapshotCache honors the order/missing contract with no .so/fixture.
#[tokio::test(flavor = "current_thread")]
async fn offline_snapshot_cache_contract() {
    let a = Pubkey::new_unique();
    let b = Pubkey::new_unique();
    let mut map = HashMap::new();
    map.insert(a, Account { lamports: 1, data: vec![], owner: Pubkey::default(), executable: false, rent_epoch: 0 });
    let cache = SnapshotCache { map };

    let got = cache.get_accounts(&[a, b]).await.unwrap();
    assert_eq!(got.len(), 2);
    assert!(got[0].is_some());
    assert!(got[1].is_none(), "missing account must be Ok(None), not an error");
    assert!(cache.get_account(&b).await.unwrap().is_none());
}
