//! Off-chain `quote(amount)` must equal the on-chain execution delta, atom-exact,
//! over an amount sweep, for restake (asset -> LP) and swap (asset -> asset).
//! Both adapters delegate quote/build to the same `reflect-sdk` math, so
//! executing the Titan venue's `generate_swap_instruction` proves the property
//! for Titan AND Jupiter.
//!
//! Unlike lending/bonds there is nothing to dump: rlp is not on mainnet and the
//! devnet deployment's `Settings` predates the current layout. The committed
//! fixture `tests/fixtures/programs/rlp.so` is built FROM SOURCE — and the
//! toolchain is LOAD-BEARING: `cargo-build-sbf --tools-version v1.41` in
//! `reflect-program-library/rlp`. Newer platform-tools (v1.43 partially, v1.52
//! fully) inflate the Anchor-0.29 `try_accounts`/`PreciseNumber` stack frames
//! past the 4KB SBF limit and emit binaries whose restake/swap ACCESS-VIOLATE
//! at runtime (the build only warns). The pool is bootstrapped through the
//! program's OWN admin instructions in LiteSVM (`initialize_rlp` ->
//! `initialize_lp` -> `add_asset` -> vault init -> genesis restakes), so every
//! stored bump, ATA, and access-map byte is program-made.
//!
//! MANDATORY: a missing fixture is a test FAILURE, not a skip.
//!
//! Also proven here, empirically, against the real program:
//! - restake is role-gated by default (`Action::Restake` -> MANAGER/TESTEE):
//!   an anonymous venue-built restake REVERTS unless the deployment adds
//!   `Role::Public` to `Action::Restake` — the open question in `RlpVenue`'s
//!   module doc, now pinned by `restake_role_gate_stays_closed_by_default`.
//! - the venue's predicted-revert quotes (`None`: killswitch, stale oracle,
//!   swap inventory) agree with on-chain reverts.

use std::path::PathBuf;

use async_trait::async_trait;
use litesvm::LiteSVM;
use solana_account::Account;
use solana_program_pack::Pack as _;
use solana_pubkey::Pubkey;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

use reflect_sdk::common::ids::TOKEN_PROGRAM;
use reflect_sdk::rlp::ix::admin::{
    add_asset, initialize_lp, initialize_lp_token_account, initialize_rlp, update_action_role, AccessLevel, AddAssetAccounts, AdminMainAccounts, InitializeLpAccounts,
    InitializeLpTokenAccountAccounts, InitializeRlpAccounts, Role, Update,
};
use reflect_sdk::rlp::ix::{restake, restake_accounts};
use reflect_sdk::rlp::resolve::pool_value_remaining_accounts;
use reflect_sdk::rlp::{ids, pda, Action, OracleRef};

use reflect_titan_common::account_caching::{AccountCacheError, AccountsCache};
use reflect_titan_common::trading_venue::{QuoteRequest, SwapType, TradingVenue};
use reflect_titan_rlp::ReflectRlpVenue;

fn manifest_rel(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Committed fixture first, then the sibling source build. MANDATORY: parity
/// cannot silently skip, so an unresolvable program is a panic.
fn program_bytes() -> Vec<u8> {
    let committed = manifest_rel("tests/fixtures/programs/rlp.so");
    let sibling = manifest_rel("../../../reflect-program-library/rlp/target/deploy/rlp.so");
    for p in [&committed, &sibling] {
        if let Ok(b) = std::fs::read(p) {
            return b;
        }
    }
    panic!("rlp.so fixture missing: commit {} or `cargo build-sbf` in reflect-program-library/rlp — parity is mandatory, not skippable", committed.display());
}

// solana_pubkey/solana_account (adapter+SDK) and solana_sdk (litesvm) are the
// same wire layout under unified 2.2, but distinct types; bridge by bytes.
fn sdk_key(k: &Pubkey) -> solana_sdk::pubkey::Pubkey {
    solana_sdk::pubkey::Pubkey::new_from_array(k.to_bytes())
}
fn sdk_to_pk(k: &solana_sdk::pubkey::Pubkey) -> Pubkey {
    Pubkey::new_from_array(k.to_bytes())
}
fn from_sdk_account(a: solana_sdk::account::Account) -> Account {
    Account { lamports: a.lamports, data: a.data, owner: sdk_to_pk(&a.owner), executable: a.executable, rent_epoch: a.rent_epoch }
}
fn to_sdk_ix(ix: &solana_instruction::Instruction) -> solana_sdk::instruction::Instruction {
    solana_sdk::instruction::Instruction {
        program_id: sdk_key(&ix.program_id),
        accounts: ix.accounts.iter().map(|m| solana_sdk::instruction::AccountMeta { pubkey: sdk_key(&m.pubkey), is_signer: m.is_signer, is_writable: m.is_writable }).collect(),
        data: ix.data.clone(),
    }
}

/// `pyth_solana_receiver_sdk::ID` — `add_asset` tags an oracle Pyth iff the
/// account is OWNED by this program (the only owner check the program makes).
const PYTH_RECEIVER: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";
/// Anchor discriminator of `PriceUpdateV2` (`sha256("account:PriceUpdateV2")`).
const PRICE_UPDATE_V2_DISC: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];

fn pk(s: &str) -> Pubkey {
    s.parse().expect("pubkey literal")
}

/// A full borsh `PriceUpdateV2` (133 bytes): disc(8) ++ write_authority(32) ++
/// verification_level(1, Full = 1) ++ PriceFeedMessage{feed_id(32), price(i64),
/// conf(u64), exponent(i32), publish_time(i64), prev_publish_time(i64),
/// ema_price(i64), ema_conf(u64)} ++ posted_slot(u64). The program's
/// `get_price_no_older_than(clock, 120s, feed_id)` reads `feed_id` from this
/// same account, so zeros pass; only freshness bites.
fn pyth_oracle_account(price: i64, exponent: i32, publish_time: i64) -> Account {
    let mut d = vec![0u8; 133];
    d[0..8].copy_from_slice(&PRICE_UPDATE_V2_DISC);
    d[40] = 1; // VerificationLevel::Full
    d[73..81].copy_from_slice(&price.to_le_bytes());
    d[89..93].copy_from_slice(&exponent.to_le_bytes());
    d[93..101].copy_from_slice(&publish_time.to_le_bytes());
    d[101..109].copy_from_slice(&publish_time.to_le_bytes()); // prev_publish_time
    d[109..117].copy_from_slice(&price.to_le_bytes()); // ema_price
    Account { lamports: 10_000_000, data: d, owner: pk(PYTH_RECEIVER), executable: false, rent_epoch: 0 }
}

/// SPL Mint (82 bytes).
fn mint_account(authority: &Pubkey, supply: u64, decimals: u8) -> Account {
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    let state = spl_token::state::Mint {
        mint_authority: spl_token::solana_program::program_option::COption::Some(sdk_key(authority)),
        supply,
        decimals,
        is_initialized: true,
        freeze_authority: spl_token::solana_program::program_option::COption::None,
    };
    spl_token::state::Mint::pack(state, &mut data).expect("pack mint");
    Account { lamports: 1_461_600, data, owner: TOKEN_PROGRAM, executable: false, rent_epoch: 0 }
}

/// SPL TokenAccount (165 bytes).
fn token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Account {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    let state = spl_token::state::Account { mint: sdk_key(mint), owner: sdk_key(owner), amount, state: spl_token::state::AccountState::Initialized, ..Default::default() };
    spl_token::state::Account::pack(state, &mut data).expect("pack token account");
    Account { lamports: 2_039_280, data, owner: TOKEN_PROGRAM, executable: false, rent_epoch: 0 }
}

fn ata(owner: &solana_sdk::pubkey::Pubkey, mint: &Pubkey) -> solana_sdk::pubkey::Pubkey {
    spl_associated_token_account::get_associated_token_address_with_program_id(owner, &sdk_key(mint), &sdk_key(&TOKEN_PROGRAM))
}

const UNIX: i64 = 1_700_000_000;
const PRICE_A: i64 = 100_000_000; // $1.00 @ exp -8
const PRICE_B: i64 = 300_000_000; // $3.00 @ exp -8: swap math must truncate
const GENESIS_A: u64 = 2_000_000_000; // 2000 units of a (6dp) into the vault
const GENESIS_B: u64 = 1_000_000_000; // 1000 units of b: the swap inventory cap
const USER_FUNDS: u64 = 1_000_000_000_000;

struct Fx {
    admin: Keypair,
    user: Keypair,
    pool: Pubkey,
    lp_mint: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    oracle_a: Pubkey,
    oracle_b: Pubkey,
}

impl Fx {
    fn oracle_refs(&self) -> Vec<(Pubkey, OracleRef)> {
        vec![(self.mint_a, OracleRef::Pyth(self.oracle_a.to_bytes())), (self.mint_b, OracleRef::Pyth(self.oracle_b.to_bytes()))]
    }
}

/// Execute instructions as one tx; panic with logs on failure — fixture
/// bootstrap must never half-succeed silently.
fn send(svm: &mut LiteSVM, label: &str, ix: solana_program::instruction::Instruction, payer: &Keypair) {
    let tx = Transaction::new_signed_with_payer(&[to_sdk_ix(&ix)], Some(&payer.pubkey()), &[payer], svm.latest_blockhash());
    if let Err(e) = svm.send_transaction(tx) {
        for l in &e.meta.logs {
            eprintln!("    {l}");
        }
        panic!("fixture bootstrap tx `{label}` failed: {:?}", e.err);
    }
}

/// Boot a 2-asset pool THROUGH the program: init settings (the caller becomes
/// SUPREMO), init pool 0, register both assets Public, create both vault ATAs,
/// optionally open `Action::Restake` to `Role::Public`, then two admin genesis
/// restakes so the LP supply and both vaults are non-zero (the supply > 0
/// branch of the restake math is the one aggregator users hit).
fn build(open_restake: bool) -> (LiteSVM, Fx) {
    let program = program_bytes();
    let mut svm = LiteSVM::new();
    svm.add_program(sdk_key(&ids::RLP_PROGRAM), &program);
    svm.set_sysvar::<SolanaClock>(&SolanaClock { slot: 1_000, unix_timestamp: UNIX, ..Default::default() });

    let admin = Keypair::new();
    let user = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).expect("airdrop admin");
    svm.airdrop(&user.pubkey(), 100_000_000_000).expect("airdrop user");

    let pool = pda::derive_liquidity_pool(0).0;
    let settings = pda::derive_settings().0;
    let admin_perms = pda::derive_permissions(&sdk_to_pk(&admin.pubkey())).0;
    let mint_a = Pubkey::new_unique();
    let mint_b = Pubkey::new_unique();
    let lp_mint = Pubkey::new_unique();
    let oracle_a = Pubkey::new_unique();
    let oracle_b = Pubkey::new_unique();

    // Mints/oracles/user funds pre-exist the program bootstrap. The lp mint's
    // authority MUST be the pool PDA (initialize_lp enforces it; restake's
    // MintTo signs with the pool seeds); decimals MUST be 9, supply 0.
    svm.set_account(sdk_key(&mint_a), to_sdk(mint_account(&Pubkey::new_unique(), USER_FUNDS * 4, 6))).unwrap();
    svm.set_account(sdk_key(&mint_b), to_sdk(mint_account(&Pubkey::new_unique(), USER_FUNDS * 4, 6))).unwrap();
    svm.set_account(sdk_key(&lp_mint), to_sdk(mint_account(&pool, 0, 9))).unwrap();
    svm.set_account(sdk_key(&oracle_a), to_sdk(pyth_oracle_account(PRICE_A, -8, UNIX))).unwrap();
    svm.set_account(sdk_key(&oracle_b), to_sdk(pyth_oracle_account(PRICE_B, -8, UNIX))).unwrap();
    for (kp, amt) in [(&admin, GENESIS_A + GENESIS_B), (&user, USER_FUNDS)] {
        for m in [&mint_a, &mint_b] {
            svm.set_account(ata(&kp.pubkey(), m), to_sdk(token_account(m, &sdk_to_pk(&kp.pubkey()), amt))).unwrap();
        }
    }

    let signer = sdk_to_pk(&admin.pubkey());
    send(&mut svm, "initialize_rlp", initialize_rlp(&InitializeRlpAccounts { signer, permissions: admin_perms, settings }, 0), &admin);
    send(&mut svm, "initialize_lp", initialize_lp(&InitializeLpAccounts { signer, permissions: admin_perms, settings, liquidity_pool: pool, lp_token_mint: lp_mint }, 3_600, 0), &admin);
    for (m, o) in [(mint_a, oracle_a), (mint_b, oracle_b)] {
        send(
            &mut svm,
            "add_asset",
            add_asset(&AddAssetAccounts { signer, admin: admin_perms, settings, asset: pda::derive_asset(&m).0, asset_mint: m, oracle: o }, AccessLevel::Public),
            &admin,
        );
        send(
            &mut svm,
            "initialize_lp_token_account",
            initialize_lp_token_account(
                &InitializeLpTokenAccountAccounts {
                    signer,
                    admin: admin_perms,
                    settings,
                    liquidity_pool: pool,
                    asset: pda::derive_asset(&m).0,
                    mint: m,
                    lp_mint_token_account: sdk_to_pk(&ata(&sdk_key(&pool), &m)),
                },
                0,
            ),
            &admin,
        );
    }
    if open_restake {
        send(
            &mut svm,
            "update_action_role",
            update_action_role(&AdminMainAccounts { admin: signer, settings, admin_permissions: admin_perms }, Action::Restake, Role::Public, Update::Add),
            &admin,
        );
    }

    let fx = Fx { admin, user, pool, lp_mint, mint_a, mint_b, oracle_a, oracle_b };
    for (m, o, amt) in [(mint_a, oracle_a, GENESIS_A), (mint_b, oracle_b, GENESIS_B)] {
        let accs = restake_accounts(&signer, 0, &m, &lp_mint, &o, Some(admin_perms));
        send(&mut svm, "genesis restake", restake(&accs, 0, amt, 0, pool_value_remaining_accounts(&fx.oracle_refs(), &pool)), &fx.admin);
    }
    (svm, fx)
}

fn to_sdk(a: Account) -> solana_sdk::account::Account {
    solana_sdk::account::Account { lamports: a.lamports, data: a.data, owner: sdk_key(&a.owner), executable: a.executable, rent_epoch: a.rent_epoch }
}

/// [`AccountsCache`] over the LIVE LiteSVM state, so the venue quotes off the
/// exact bytes the program will execute against (the clock sysvar included).
struct SvmCache<'a>(&'a LiteSVM);

#[async_trait]
impl AccountsCache for SvmCache<'_> {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, AccountCacheError> {
        Ok(self.0.get_account(&sdk_key(pubkey)).map(from_sdk_account))
    }
    async fn get_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>, AccountCacheError> {
        Ok(pubkeys.iter().map(|k| self.0.get_account(&sdk_key(k)).map(from_sdk_account)).collect())
    }
}

async fn setup(svm: &LiteSVM, fx: &Fx) -> ReflectRlpVenue {
    let mut venue = ReflectRlpVenue::new(fx.pool, vec![fx.mint_a, fx.mint_b]);
    venue.update_state(&SvmCache(svm)).await.expect("update_state");
    assert_eq!(venue.get_token_info().len(), 3, "venue must resolve [lp, a, b]");
    venue
}

/// Build the venue's instruction, execute it, and return the out-mint balance
/// delta. `None` = the tx failed (logged) — the revert side of a `None` quote.
fn sim_delta(svm: &mut LiteSVM, venue: &ReflectRlpVenue, fx: &Fx, in_mint: Pubkey, out_mint: Pubkey, amount: u64) -> Option<u64> {
    let req = QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount, swap_type: SwapType::ExactIn };
    let ix = match venue.generate_swap_instruction(req, sdk_to_pk(&fx.user.pubkey())) {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("[sim_delta] generate_swap_instruction failed amt {amount}: {e:?}");
            return None;
        },
    };
    let dst = ata(&fx.user.pubkey(), &out_mint);
    let before = svm.get_account(&dst).and_then(|a| spl_token::state::Account::unpack(&a.data).ok()).map_or(0, |a| a.amount);

    let tx = Transaction::new_signed_with_payer(&[to_sdk_ix(&ix)], Some(&fx.user.pubkey()), &[&fx.user], svm.latest_blockhash());
    if let Err(e) = svm.send_transaction(tx) {
        eprintln!("[sim_delta] tx failed amt {amount} {in_mint}->{out_mint}:");
        for l in &e.meta.logs {
            eprintln!("    {l}");
        }
        eprintln!("    err: {:?}", e.err);
        return None;
    }
    let after = svm.get_account(&dst).and_then(|a| spl_token::state::Account::unpack(&a.data).ok()).map_or(0, |a| a.amount);
    Some(after - before)
}

fn quote_out(venue: &ReflectRlpVenue, in_mint: Pubkey, out_mint: Pubkey, amount: u64) -> Option<u64> {
    let q = venue.quote(QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount, swap_type: SwapType::ExactIn }).expect("quote");
    (!q.not_enough_liquidity).then_some(q.expected_output)
}

const SWEEP: [u64; 4] = [1_000, 1_000_000, 123_456_789, 2_000_000_000];

/// Restake (a -> LP) and both swap directions, atom-exact over the sweep. A
/// fresh program-bootstrapped SVM per amount: execution mutates the pool.
#[tokio::test(flavor = "current_thread")]
async fn parity_restake_and_swap_swept() {
    for amt in SWEEP {
        for (input, output) in [(None, None), (Some(0), Some(1)), (Some(1), Some(0))] {
            let (mut svm, fx) = build(true);
            let venue = setup(&svm, &fx).await;
            let (in_mint, out_mint) = match (input, output) {
                (None, None) => (fx.mint_a, fx.lp_mint), // restake
                (Some(0), _) => (fx.mint_a, fx.mint_b),
                _ => (fx.mint_b, fx.mint_a),
            };
            let Some(expected) = quote_out(&venue, in_mint, out_mint, amt) else {
                // The only legal None in this healthy fixture is swap inventory.
                assert!(out_mint != fx.lp_mint, "restake quote must not predict a revert at {amt}");
                let sim = sim_delta(&mut svm, &venue, &fx, in_mint, out_mint, amt);
                assert!(sim.is_none(), "quote said no-route at {amt} but execution succeeded: {sim:?}");
                continue;
            };
            assert!(expected > 0, "sweep amounts are sized to quote non-zero (amt {amt})");
            let delta = sim_delta(&mut svm, &venue, &fx, in_mint, out_mint, amt).expect("execution must succeed where quote predicts output");
            assert_eq!(delta, expected, "parity broke: {in_mint}->{out_mint} amt {amt}: quote {expected} sim {delta}");
        }
    }
}

/// DEFAULT access map: restake is MANAGER/TESTEE-gated, the venue cannot see
/// that (Settings' AccessMap is not decoded by the SDK), so it quotes Some —
/// and the on-chain execution with `permissions: None` MUST revert. This pins
/// the documented known-limitation in `RlpVenue`'s module doc to real program
/// behavior; if the SDK learns to model the gate, this test flips and says so.
#[tokio::test(flavor = "current_thread")]
async fn restake_role_gate_stays_closed_by_default() {
    let (mut svm, fx) = build(false);
    let venue = setup(&svm, &fx).await;

    let q = quote_out(&venue, fx.mint_a, fx.lp_mint, 1_000_000);
    assert!(q.is_some(), "the venue cannot model the role gate: quote stays Some");
    let sim = sim_delta(&mut svm, &venue, &fx, fx.mint_a, fx.lp_mint, 1_000_000);
    assert!(sim.is_none(), "anonymous restake must revert under the default access map");

    // PublicSwap really is public by default: same fixture, swap succeeds.
    let expected = quote_out(&venue, fx.mint_a, fx.mint_b, 1_000_000).expect("swap quote");
    let delta = sim_delta(&mut svm, &venue, &fx, fx.mint_a, fx.mint_b, 1_000_000).expect("public swap");
    assert_eq!(delta, expected);
}

/// Killswitch: freeze restake on-chain, reload the venue, quote goes None AND
/// execution reverts; swap stays live on both sides.
#[tokio::test(flavor = "current_thread")]
async fn killswitch_gates_quote_and_execution() {
    let (mut svm, fx) = build(true);
    let settings = pda::derive_settings().0;
    let admin_perms = pda::derive_permissions(&sdk_to_pk(&fx.admin.pubkey())).0;
    // `freeze_functionality` takes the Freeze* alias; the handler maps it onto
    // the core action's killswitch bit (FreezeRestake -> Restake).
    let accs = AdminMainAccounts { admin: sdk_to_pk(&fx.admin.pubkey()), settings, admin_permissions: admin_perms };
    send(&mut svm, "freeze restake", reflect_sdk::rlp::ix::admin::freeze_functionality(&accs, Action::FreezeRestake, true), &fx.admin);

    let venue = setup(&svm, &fx).await;
    assert!(quote_out(&venue, fx.mint_a, fx.lp_mint, 1_000_000).is_none(), "frozen restake must quote None");
    assert!(sim_delta(&mut svm, &venue, &fx, fx.mint_a, fx.lp_mint, 1_000_000).is_none(), "frozen restake must revert");

    let expected = quote_out(&venue, fx.mint_a, fx.mint_b, 1_000_000).expect("swap unaffected");
    assert_eq!(sim_delta(&mut svm, &venue, &fx, fx.mint_a, fx.mint_b, 1_000_000), Some(expected));
}

/// Swap inventory: an output the destination vault cannot cover quotes None
/// and reverts on-chain (the SDK's raw quote reads only the two oracles; the
/// venue's vault guard is what keeps quote == execution here).
#[tokio::test(flavor = "current_thread")]
async fn swap_inventory_guard_agrees() {
    let (mut svm, fx) = build(true);
    let venue = setup(&svm, &fx).await;
    // a -> b at $1 -> $3 pays out amt/3; 6e9 in asks ~2e9 b, vault holds 1e9.
    let amt = 6_000_000_000u64;
    assert!(quote_out(&venue, fx.mint_a, fx.mint_b, amt).is_none(), "over-inventory swap must quote None");
    assert!(sim_delta(&mut svm, &venue, &fx, fx.mint_a, fx.mint_b, amt).is_none(), "over-inventory swap must revert");
}

/// Stale oracle (> 120s unix): quote None, execution reverts — same window in
/// the SDK and the program (`ORACLE_MAXIMUM_AGE = 2 * 60`).
#[tokio::test(flavor = "current_thread")]
async fn stale_oracle_agrees() {
    let (mut svm, fx) = build(true);
    svm.set_sysvar::<SolanaClock>(&SolanaClock { slot: 1_001, unix_timestamp: UNIX + 121, ..Default::default() });
    let venue = setup(&svm, &fx).await; // built AFTER the warp: quotes at the stale clock

    assert!(quote_out(&venue, fx.mint_a, fx.lp_mint, 1_000_000).is_none(), "stale restake must quote None");
    assert!(quote_out(&venue, fx.mint_a, fx.mint_b, 1_000_000).is_none(), "stale swap must quote None");
    assert!(sim_delta(&mut svm, &venue, &fx, fx.mint_a, fx.lp_mint, 1_000_000).is_none(), "stale restake must revert");
    assert!(sim_delta(&mut svm, &venue, &fx, fx.mint_a, fx.mint_b, 1_000_000).is_none(), "stale swap must revert");
}
