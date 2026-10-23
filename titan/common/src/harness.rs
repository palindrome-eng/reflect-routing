//! LiteSVM test harness for the parity suites (`harness` feature): load a mainnet snapshot
//! and a dumped program, pin the clock inside every oracle's staleness window, forge SPL
//! accounts, send transactions with logs on failure, and serve the live SVM as an
//! [`AccountsCache`] so a venue quotes off the exact bytes the program executes against.

use std::path::Path;

use async_trait::async_trait;
use base64::Engine;
use litesvm::LiteSVM;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

use crate::account_caching::{AccountCacheError, AccountsCache};

/// A dumped program under `<fixtures>/programs/<name>.so`; a missing fixture panics.
pub fn program_bytes(fixtures: &Path, name: &str) -> Vec<u8> {
    let path = fixtures.join(format!("programs/{name}.so"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("missing program fixture {}: {e}", path.display()))
}

/// One fetched account: `(key, owner, data)`.
pub type Fetched = (Pubkey, Pubkey, Vec<u8>);

/// A mainnet snapshot under `<fixtures>/snapshots/<name>.json`.
pub struct Snapshot {
    pub slot: u64,
    pub unix_timestamp: i64,
    pub accounts: Vec<Fetched>,
}

pub fn snapshot(fixtures: &Path, name: &str) -> Snapshot {
    let path = fixtures.join(format!("snapshots/{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing snapshot {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let accounts = v["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["key"].as_str().unwrap().parse().unwrap(),
                a["owner"].as_str().unwrap().parse().unwrap(),
                base64::engine::general_purpose::STANDARD.decode(a["data"].as_str().unwrap()).unwrap(),
            )
        })
        .collect();
    Snapshot { slot: v["slot"].as_u64().unwrap(), unix_timestamp: v["unix_timestamp"].as_i64().unwrap(), accounts }
}

impl Snapshot {
    pub fn get(&self, key: &Pubkey) -> &Fetched {
        self.accounts.iter().find(|a| a.0 == *key).unwrap_or_else(|| panic!("snapshot lacks {key}"))
    }

    pub fn keys(&self) -> Vec<Pubkey> {
        self.accounts.iter().map(|a| a.0).collect()
    }

    /// A clock just inside every oracle's window: 10 slots and 10 seconds past the newest
    /// posted slot and publish time of the Pyth and Doppler accounts in the snapshot.
    pub fn clock(&self) -> Clock {
        let mut slot = 0u64;
        let mut ts = 0i64;
        for (_, _, d) in &self.accounts {
            if d.len() >= 133 && d[..8] == PYTH_PRICE_UPDATE_V2_DISC {
                ts = ts.max(i64::from_le_bytes(d[93..101].try_into().unwrap()));
                slot = slot.max(u64::from_le_bytes(d[125..133].try_into().unwrap()));
            } else if d.len() == 17 {
                slot = slot.max(u64::from_le_bytes(d[..8].try_into().unwrap()));
            }
        }
        Clock { slot: slot + 10, unix_timestamp: ts + 10, ..Default::default() }
    }
}

/// Anchor discriminator of the Pyth receiver's `PriceUpdateV2`.
pub const PYTH_PRICE_UPDATE_V2_DISC: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];

pub fn set_clock(svm: &mut LiteSVM, clock: &Clock) {
    svm.set_sysvar::<Clock>(clock);
}

/// Load the snapshot's accounts as they were on chain.
pub fn load(svm: &mut LiteSVM, snap: &Snapshot) {
    for (key, owner, data) in &snap.accounts {
        forge_account(svm, *key, *owner, data.clone());
    }
}

pub fn funded_user(svm: &mut LiteSVM) -> Keypair {
    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 100_000_000_000).unwrap();
    user
}

/// Write an arbitrary account.
pub fn forge_account(svm: &mut LiteSVM, key: Pubkey, owner: Pubkey, data: Vec<u8>) {
    svm.set_account(key, Account { lamports: 10_000_000_000, data, owner, executable: false, rent_epoch: 0 }).unwrap();
}

/// SPL Token `Mint` bytes (82 B): `mint_authority`, supply, decimals, initialized, no freeze authority.
pub fn mint_bytes(mint_authority: &Pubkey, supply: u64, decimals: u8) -> Vec<u8> {
    let mut d = vec![0u8; 82];
    d[..4].copy_from_slice(&1u32.to_le_bytes());
    d[4..36].copy_from_slice(mint_authority.as_ref());
    d[36..44].copy_from_slice(&supply.to_le_bytes());
    d[44] = decimals;
    d[45] = 1;
    d
}

/// SPL Token `Account` bytes (165 B, initialized).
pub fn token_account_bytes(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    d
}

/// `owner`'s SPL Token ATA for `mint`.
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account::get_associated_token_address_with_program_id(owner, mint, &spl_token::ID)
}

/// Forge `owner`'s ATA for `mint` holding `amount`; returns its address.
pub fn forge_ata(svm: &mut LiteSVM, owner: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
    let key = ata(owner, mint);
    svm.set_account(key, Account { lamports: 10_000_000, data: token_account_bytes(mint, owner, amount), owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();
    key
}

pub fn token_balance(svm: &LiteSVM, ata: &Pubkey) -> u64 {
    svm.get_account(ata).map_or(0, |a| u64::from_le_bytes(a.data[64..72].try_into().unwrap()))
}

pub fn mint_supply(svm: &LiteSVM, mint: &Pubkey) -> u64 {
    svm.get_account(mint).map_or(0, |a| u64::from_le_bytes(a.data[36..44].try_into().unwrap()))
}

/// A `Full` Pyth `PriceUpdateV2` (134 B as the receiver allocates it).
pub fn pyth_bytes(feed_id: &[u8; 32], price: i64, conf: u64, exponent: i32, publish_time: i64, posted_slot: u64) -> Vec<u8> {
    let mut d = vec![0u8; 134];
    d[..8].copy_from_slice(&PYTH_PRICE_UPDATE_V2_DISC);
    d[40] = 1;
    d[41..73].copy_from_slice(feed_id);
    d[73..81].copy_from_slice(&price.to_le_bytes());
    d[81..89].copy_from_slice(&conf.to_le_bytes());
    d[89..93].copy_from_slice(&exponent.to_le_bytes());
    d[93..101].copy_from_slice(&publish_time.to_le_bytes());
    d[125..133].copy_from_slice(&posted_slot.to_le_bytes());
    d
}

/// A Doppler push account (17 B).
pub fn doppler_bytes(slot: u64, price: u64, precision: u8) -> Vec<u8> {
    let mut d = vec![0u8; 17];
    d[..8].copy_from_slice(&slot.to_le_bytes());
    d[8..16].copy_from_slice(&price.to_le_bytes());
    d[16] = precision;
    d
}

/// Send one instruction signed by `payer`; panic with the program logs on failure.
pub fn send(svm: &mut LiteSVM, label: &str, ix: Instruction, payer: &Keypair) {
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[payer], svm.latest_blockhash());
    let result = svm.send_transaction(tx);
    // Identical instructions at the same blockhash would otherwise dedupe as AlreadyProcessed.
    svm.expire_blockhash();
    if let Err(e) = result {
        for l in &e.meta.logs {
            eprintln!("    {l}");
        }
        panic!("tx `{label}` failed: {:?}", e.err);
    }
}

/// Send one instruction; `Err(logs)` when the program refuses it.
pub fn try_send(svm: &mut LiteSVM, ix: Instruction, payer: &Keypair) -> Result<(), Vec<String>> {
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[payer], svm.latest_blockhash());
    let result = svm.send_transaction(tx);
    svm.expire_blockhash();
    result.map(|_| ()).map_err(|e| e.meta.logs)
}

/// The live LiteSVM state as an [`AccountsCache`].
pub struct SvmCache<'a>(pub &'a LiteSVM);

#[async_trait]
impl AccountsCache for SvmCache<'_> {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, AccountCacheError> {
        Ok(self.0.get_account(pubkey))
    }
    async fn get_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>, AccountCacheError> {
        Ok(pubkeys.iter().map(|k| self.0.get_account(k)).collect())
    }
}

// ---------------------------------------------------------------------------
// Titan's shared venue contract (upstream tests/common/mod.rs at c0bb5c9) over a
// live LiteSVM instead of RPC. The seven correctness assertions; the no-alloc
// and quoting-speed gates are the performance contract and live apart.
// ---------------------------------------------------------------------------

use crate::trading_venue::{QuoteRequest, QuoteResult, SwapType, TradingVenue};
use solana_sdk::account::ReadableAccount;
use solana_sdk::transaction::Transaction as SdkTransaction;

/// Deterministic xorshift64* so every sampled failure is reproducible from `seed`.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Log-uniform in `[lo, hi]`, as upstream samples.
    pub fn log_uniform(&mut self, lo: u64, hi: u64) -> u64 {
        assert!(lo >= 1 && lo <= hi);
        let r = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        ((lo as f64).ln() + r * ((hi as f64).ln() - (lo as f64).ln())).exp().clamp(lo as f64, hi as f64) as u64
    }
}

/// Geometrically spaced probe points across `[lb, ub]`, deduplicated.
pub fn geometric_grid(lb: u64, ub: u64, n: usize) -> Vec<u64> {
    let (lo, hi) = ((lb as f64).ln(), (ub as f64).ln());
    let mut pts: Vec<u64> = (0..n).map(|i| (lo + (hi - lo) * i as f64 / (n - 1) as f64).exp().clamp(lb as f64, ub as f64) as u64).collect();
    pts.sort_unstable();
    pts.dedup();
    pts
}

fn exact_in(input_mint: Pubkey, output_mint: Pubkey, amount: u64) -> QuoteRequest {
    QuoteRequest { input_mint, output_mint, amount, swap_type: SwapType::ExactIn }
}

/// Simulate the venue's instruction for `req` as `user` WITHOUT committing state and return the
/// out-mint balance delta, or `None` with the logs printed when the program refuses it.
pub fn simulate_delta(svm: &LiteSVM, venue: &dyn TradingVenue, user: &Keypair, req: QuoteRequest) -> Option<u64> {
    let out_token = venue.get_token_info().iter().find(|t| t.pubkey == req.output_mint).expect("output mint in venue");
    let out_program = if out_token.is_token_2022 { spl_token_2022::ID } else { spl_token::ID };
    let dst = spl_associated_token_account::get_associated_token_address_with_program_id(&user.pubkey(), &req.output_mint, &out_program);
    let before = token_balance(svm, &dst);
    let ix = venue.generate_swap_instruction(req, user.pubkey()).ok()?;
    // Upstream simulates under a 1.4M CU budget; a lending mint with its venue CPI exceeds
    // the runtime's 200k default.
    let budget = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    let tx = SdkTransaction::new_signed_with_payer(&[budget, ix], Some(&user.pubkey()), &[user], svm.latest_blockhash());
    match svm.simulate_transaction(tx) {
        Ok(info) => {
            let post = info.post_accounts.iter().find(|(k, _)| *k == dst).map(|(_, a)| a.data()[64..72].try_into().map(u64::from_le_bytes).unwrap())?;
            Some(post - before)
        },
        Err(e) => {
            for l in &e.meta.logs {
                eprintln!("    {l}");
            }
            None
        },
    }
}

/// Forge `user`'s ATAs for one direction as upstream does: the source holds `u64::MAX`, the
/// destination is empty, so only the venue's own ceilings bound the sweep.
pub fn fund_direction(svm: &mut LiteSVM, user: &Keypair, venue: &dyn TradingVenue, in_mint: Pubkey, out_mint: Pubkey) {
    for (mint, amount) in [(in_mint, u64::MAX), (out_mint, 0)] {
        let t = venue.get_token_info().iter().find(|t| t.pubkey == mint).expect("mint in venue");
        let program = if t.is_token_2022 { spl_token_2022::ID } else { spl_token::ID };
        let key = spl_associated_token_account::get_associated_token_address_with_program_id(&user.pubkey(), &mint, &program);
        svm.set_account(key, Account { lamports: 10_000_000, data: token_account_bytes(&mint, &user.pubkey(), amount), owner: program, executable: false, rent_epoch: 0 }).unwrap();
    }
}

fn valid(q: &QuoteResult) -> bool {
    !q.not_enough_liquidity && q.expected_output > 0
}

/// Run Titan's seven correctness assertions over every declared direction of `venue`,
/// quoting and simulating against the SAME SVM state:
/// construction (bounds valid, both edges quote with output and a positive price),
/// zero-input spot, bound simulation (atom-exact at both edges), 50 seeded random samples
/// (atom-exact), output monotone over 50 samples, price non-increasing over 200 samples
/// (1e-3 slack), and the mean-value bracket over a 64-point geometric grid (1e-5 slack plus
/// two output atoms of floor).
pub fn assert_titan_contract<V: TradingVenue>(svm: &mut LiteSVM, venue: &V, seed: u64) {
    const PRICE_TOL: f64 = 1e-3;
    const MVT_TOL: f64 = 1e-5;
    const OUT_QUANTUM: f64 = 2.0;
    eprintln!("titan contract: seed={seed}");
    let user = funded_user(svm);
    let mut rng = Rng::new(seed);
    assert!(venue.get_token_info().len() >= 2, "venue must expose at least two tokens");

    for (i, j) in venue.directions_num() {
        let (in_mint, out_mint) = (venue.get_token(i as usize).unwrap().pubkey, venue.get_token(j as usize).unwrap().pubkey);
        let dir = format!("{in_mint}->{out_mint}");
        fund_direction(svm, &user, venue, in_mint, out_mint);
        let quote = |amount: u64| venue.quote(exact_in(in_mint, out_mint, amount)).unwrap_or_else(|e| panic!("{dir}: quote at {amount} errored: {e:?}"));

        // construction
        let (lb, ub) = venue.bounds(i, j).unwrap_or_else(|e| panic!("{dir}: bounds failed: {e:?}"));
        assert!(lb < ub, "{dir}: lower bound {lb} must be < upper bound {ub}");
        for (edge, amount) in [("lower", lb), ("upper", ub)] {
            let q = quote(amount);
            assert!(valid(&q), "{dir}: {edge} bound {amount} does not quote");
            assert!(q.price > 0.0, "{dir}: {edge} bound price {} must be positive", q.price);
        }

        // zero-input spot
        let z = quote(0);
        assert_eq!(z.expected_output, 0, "{dir}: zero input must produce zero output");
        assert!(z.price > 0.0, "{dir}: zero input must still report a positive spot, got {}", z.price);

        // bound simulation + random samples: atom-exact against execution
        let samples: Vec<u64> = [lb, ub].into_iter().chain((0..50).map(|_| rng.log_uniform(lb, ub))).collect();
        for amount in &samples {
            let q = quote(*amount);
            let sim = simulate_delta(svm, venue, &user, exact_in(in_mint, out_mint, *amount)).unwrap_or_else(|| panic!("{dir}: execution refused {amount} inside bounds"));
            assert_eq!(q.expected_output, sim, "{dir}: quote != execution at {amount}");
        }

        // output monotone
        let mut sorted: Vec<u64> = (0..50).map(|_| rng.log_uniform(lb, ub)).collect();
        sorted.sort_unstable();
        let mut prev = 0;
        for amount in sorted {
            let out = quote(amount).expected_output;
            assert!(prev <= out, "{dir}: output not monotone: {prev} > {out} at {amount}");
            prev = out;
        }

        // price non-increasing
        let mut sorted: Vec<u64> = (0..200).map(|_| rng.log_uniform(lb, ub)).collect();
        sorted.sort_unstable();
        let mut prev_price = f64::INFINITY;
        for amount in sorted {
            let p = quote(amount).price;
            assert!(p > 0.0, "{dir}: price must be positive, got {p} at {amount}");
            assert!(p <= prev_price * (1.0 + PRICE_TOL), "{dir}: price not non-increasing: {prev_price} -> {p} at {amount}");
            prev_price = p;
        }

        // mean value theorem
        for w in geometric_grid(lb, ub, 64).windows(2) {
            let (a, b) = (w[0], w[1]);
            let (qa, qb) = (quote(a), quote(b));
            if b <= a || qb.expected_output <= qa.expected_output {
                continue;
            }
            let chord = (qb.expected_output - qa.expected_output) as f64 / (b - a) as f64;
            let atol = OUT_QUANTUM / (b - a) as f64;
            assert!(qb.price <= qa.price * (1.0 + MVT_TOL), "{dir}: price increased with size: f'({a})={} < f'({b})={}", qa.price, qb.price);
            assert!(chord <= qa.price * (1.0 + MVT_TOL) + atol, "{dir}: chord {chord} exceeds left price {} on [{a}, {b}]", qa.price);
            assert!(chord >= qb.price * (1.0 - MVT_TOL) - atol, "{dir}: chord {chord} below right price {} on [{a}, {b}]", qb.price);
        }
        eprintln!("titan contract: {dir} ok over [{lb}, {ub}]");
    }
}

// ---------------------------------------------------------------------------
// Titan's performance contract: no heap allocation inside `quote()` and under
// 1 µs per quote. The allocation guard needs `assert_no_alloc::AllocDisabler`
// installed as the test binary's global allocator under `debug_assertions`;
// the timing runs under `--release`.
// ---------------------------------------------------------------------------

pub use assert_no_alloc::{AllocDisabler, assert_no_alloc};

/// Upstream `construction`: `bounds()` and both edge quotes run with the heap disabled.
pub fn assert_quotes_do_not_allocate<V: TradingVenue>(venue: &V) {
    for (i, j) in venue.directions_num() {
        let (in_mint, out_mint) = (venue.get_token(i as usize).unwrap().pubkey, venue.get_token(j as usize).unwrap().pubkey);
        let (lb, ub) = assert_no_alloc(|| venue.bounds(i, j)).unwrap_or_else(|e| panic!("{in_mint}->{out_mint}: bounds failed: {e:?}"));
        for amount in [0, lb, ub] {
            let q = assert_no_alloc(|| venue.quote(exact_in(in_mint, out_mint, amount))).unwrap_or_else(|e| panic!("{in_mint}->{out_mint}: quote at {amount} errored: {e:?}"));
            assert!(q.price > 0.0, "{in_mint}->{out_mint}: price at {amount} must be positive");
        }
    }
}

/// Upstream `quoting_speed`: 10,000 seeded log-uniform quotes per direction must average
/// under `max_avg_ns` (Titan: 1,000 ns). Meaningful under `--release` only.
pub fn assert_quote_speed<V: TradingVenue>(venue: &V, seed: u64, max_avg_ns: f64) {
    const ITERATIONS: usize = 10_000;
    if cfg!(debug_assertions) {
        eprintln!("quote speed: SKIP in a debug build; run under --release");
        return;
    }
    let mut rng = Rng::new(seed);
    for (i, j) in venue.directions_num() {
        let (in_mint, out_mint) = (venue.get_token(i as usize).unwrap().pubkey, venue.get_token(j as usize).unwrap().pubkey);
        let (lb, ub) = venue.bounds(i, j).expect("bounds");
        let amounts: Vec<u64> = (0..ITERATIONS).map(|_| rng.log_uniform(lb, ub)).collect();
        let start = std::time::Instant::now();
        for amount in amounts {
            let _ = venue.quote(exact_in(in_mint, out_mint, amount)).expect("quote");
        }
        let avg_ns = start.elapsed().as_nanos() as f64 / ITERATIONS as f64;
        eprintln!("quote speed: {in_mint}->{out_mint} avg {avg_ns:.0} ns");
        assert!(avg_ns < max_avg_ns, "{in_mint}->{out_mint}: {avg_ns:.0} ns per quote exceeds {max_avg_ns:.0} ns");
    }
}

// ---------------------------------------------------------------------------
// Routed execution: the venue's leg inside the route program under TitanPDA custody.
// ---------------------------------------------------------------------------

use crate::swap_route::{build_swap_leg, encode_swap_route_v3_data, titan_pda, ROUTER_PROGRAM_ID, ROUTE_WEIGHT_ALL, TITAN_PDA_DISC};

/// The route program from `TITAN_ROUTER_SO`, else the workspace's
/// `cargo build-sbf --manifest-path titan/program/Cargo.toml` output; `None` until built.
pub fn router_program() -> Option<Vec<u8>> {
    let path = std::env::var_os("TITAN_ROUTER_SO").map(std::path::PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy/reflect_titan_router.so"));
    std::fs::read(path).ok()
}

/// TitanPDA balances that must survive every route untouched.
const RESERVED_IN: u64 = 17;
const RESERVED_OUT: u64 = 29;

fn token_program_of(venue: &dyn TradingVenue, mint: Pubkey) -> Pubkey {
    if venue.get_token_info().iter().find(|t| t.pubkey == mint).expect("mint in venue").is_token_2022 {
        spl_token_2022::ID
    } else {
        spl_token::ID
    }
}

fn forge_token(svm: &mut LiteSVM, owner: &Pubkey, mint: &Pubkey, program: &Pubkey, amount: u64) -> Pubkey {
    let key = spl_associated_token_account::get_associated_token_address_with_program_id(owner, mint, program);
    svm.set_account(key, Account { lamports: 10_000_000, data: token_account_bytes(mint, owner, amount), owner: *program, executable: false, rent_epoch: 0 }).unwrap();
    key
}

fn post_balance(info: &litesvm::types::SimulatedTransactionInfo, key: &Pubkey) -> u64 {
    info.post_accounts.iter().find(|(k, _)| k == key).map(|(_, a)| u64::from_le_bytes(a.data()[64..72].try_into().unwrap())).expect("account in simulation")
}

/// Route every declared direction of `venue` through `swap_route_v3` as one leg, simulated
/// (never committed) at the lower bound, the midpoint and the upper bound: a leg floor one atom
/// above the quote is refused with `MinimumOutput`; at the quote the user receives exactly the
/// quoted output, the input is fully spent and TitanPDA's reserved balances are intact.
pub fn assert_titan_route<V: TradingVenue>(svm: &mut LiteSVM, venue: &V, router: &[u8]) {
    svm.add_program(ROUTER_PROGRAM_ID, router);
    let (titan, bump) = titan_pda();
    let mut data = TITAN_PDA_DISC.to_vec();
    data.push(bump);
    svm.set_account(titan, Account { lamports: 10_000_000, data, owner: ROUTER_PROGRAM_ID, executable: false, rent_epoch: 0 }).unwrap();
    let user = funded_user(svm);

    for (i, j) in venue.directions_num() {
        let (in_mint, out_mint) = (venue.get_token(i as usize).unwrap().pubkey, venue.get_token(j as usize).unwrap().pubkey);
        let (in_program, out_program) = (token_program_of(venue, in_mint), token_program_of(venue, out_mint));
        let dir = format!("{in_mint}->{out_mint}");
        let (lb, ub) = venue.bounds(i, j).unwrap_or_else(|e| panic!("{dir}: bounds failed: {e:?}"));
        for amount in [lb, lb + (ub - lb) / 2, ub] {
            let req = exact_in(in_mint, out_mint, amount);
            let q = venue.quote(req.clone()).unwrap_or_else(|e| panic!("{dir}: quote at {amount} errored: {e:?}"));
            assert!(valid(&q), "{dir}: {amount} inside bounds must quote");
            let user_in = forge_token(svm, &user.pubkey(), &in_mint, &in_program, amount);
            let user_out = forge_token(svm, &user.pubkey(), &out_mint, &out_program, 0);
            let titan_in = forge_token(svm, &titan, &in_mint, &in_program, RESERVED_IN);
            let titan_out = forge_token(svm, &titan, &out_mint, &out_program, RESERVED_OUT);

            let route = |min_out: u64| {
                let (leg, leg_accounts) = build_swap_leg(venue, &req, titan, 0, 1, ROUTE_WEIGHT_ALL, min_out).unwrap_or_else(|e| panic!("{dir}: leg build failed: {e:?}"));
                let ro = |k: Pubkey| solana_sdk::instruction::AccountMeta::new_readonly(k, false);
                let rw = |k: Pubkey| solana_sdk::instruction::AccountMeta::new(k, false);
                let mut accounts = vec![
                    solana_sdk::instruction::AccountMeta::new(user.pubkey(), true),
                    solana_sdk::instruction::AccountMeta::new(user.pubkey(), true),
                    ro(titan),
                    rw(user_in),
                    rw(user_out),
                    ro(spl_token::ID),
                    ro(spl_token_2022::ID),
                    ro(solana_sdk::system_program::ID),
                    ro(spl_associated_token_account::ID),
                    // Anchor optional accounts: the program id is the none placeholder.
                    ro(ROUTER_PROGRAM_ID),
                    ro(ROUTER_PROGRAM_ID),
                    ro(ROUTER_PROGRAM_ID),
                    rw(titan_in),
                    rw(titan_out),
                    ro(in_mint),
                    ro(out_mint),
                ];
                accounts.extend(leg_accounts);
                let ix = Instruction { program_id: ROUTER_PROGRAM_ID, accounts, data: encode_swap_route_v3_data(amount, 2, &[leg]) };
                let budget = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
                SdkTransaction::new_signed_with_payer(&[budget, ix], Some(&user.pubkey()), &[&user], svm.latest_blockhash())
            };

            let refused = svm.simulate_transaction(route(q.expected_output + 1)).expect_err(&format!("{dir}: a floor above the quote must be refused at {amount}"));
            assert!(refused.meta.logs.iter().any(|l| l.contains("MinimumOutput")), "{dir}: refused for another reason at {amount}:\n{}", refused.meta.logs.join("\n"));

            let info = svm.simulate_transaction(route(q.expected_output)).unwrap_or_else(|e| panic!("{dir}: route failed at {amount}:\n{}\n{:?}", e.meta.logs.join("\n"), e.err));
            assert_eq!(post_balance(&info, &user_out), q.expected_output, "{dir}: user output != quote at {amount}");
            assert_eq!(post_balance(&info, &user_in), 0, "{dir}: input not fully spent at {amount}");
            assert!(post_balance(&info, &titan_in) >= RESERVED_IN, "{dir}: TitanPDA input custody drained at {amount}");
            assert_eq!(post_balance(&info, &titan_out), RESERVED_OUT, "{dir}: TitanPDA output custody changed at {amount}");
            eprintln!("titan route: {dir} ok at {amount} ({} CU)", info.meta.compute_units_consumed);
        }
    }
}
