//! Off-chain `quote(amount)` must equal the on-chain execution delta, atom-exact,
//! for deposit (asset -> LP) and swap (asset -> asset). Both adapters delegate
//! quote/build to the same `reflect-routing-core` venue, so executing the Titan
//! venue's `generate_swap_instruction` proves the property for Titan AND Jupiter.
//!
//! One program, `rlp_prod.so`, the DUMPED mainnet binary (see `SOURCES.md`), run two ways:
//! - over a mainnet snapshot of pool 4 (one migrated Pyth asset; Swap is
//!   crank-only there, so deposit is the only public route);
//! - bootstrapped from an empty ledger through its own admin instructions with
//!   two banded assets (one single-leg Pyth, one Doppler-over-inverse-Pyth) and
//!   Deposit + Swap opened to the public.
//!
//! MANDATORY: a missing fixture is a test FAILURE, not a skip.

use std::path::PathBuf;

use litesvm::LiteSVM;
use solana_sdk::clock::Clock;
use solana_sdk::instruction::AccountMeta;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use reflect_sdk::common::ids::TOKEN_PROGRAM;
use reflect_sdk::rlp::ids::{DOPPLER_ORACLE, PYTH_RECEIVER, RLP_PROGRAM};
use reflect_sdk::rlp::ix::{
    add_asset, freeze_functionality, initialize_lp, initialize_pool_reserve, initialize_rlp, update_action_role, update_deposit_cap, AddAssetAccounts, AdminMainAccounts,
    InitializeLpAccounts, InitializePoolReserveAccounts, PriceBand, Update,
};
use reflect_sdk::rlp::{pda, AccessLevel, Action, AssetView, LiquidityPoolView, OracleLeg, OracleProvider, Role};

use reflect_titan_common::harness::*;
use reflect_titan_common::trading_venue::{QuoteRequest, SwapType, TradingVenue};
use reflect_titan_rlp::ReflectRlpVenue;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn settings() -> Pubkey {
    pda::derive_settings(&RLP_PROGRAM).0
}

/// The venue over the live SVM state (the clock sysvar included).
async fn venue(svm: &LiteSVM, pool: Pubkey, mints: Vec<Pubkey>) -> ReflectRlpVenue {
    let mut v = ReflectRlpVenue::new(RLP_PROGRAM, pool, mints);
    v.update_state(&SvmCache(svm)).await.expect("update_state");
    v
}

fn quote_out(venue: &ReflectRlpVenue, in_mint: Pubkey, out_mint: Pubkey, amount: u64) -> Option<u64> {
    let q = venue.quote(QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount, swap_type: SwapType::ExactIn }).expect("quote");
    (!q.not_enough_liquidity).then_some(q.expected_output)
}

/// Build the venue's instruction, execute it as `user`, and return the out-mint
/// balance delta. `None` = the venue refused to build or the tx failed (logged).
fn sim_delta(svm: &mut LiteSVM, venue: &ReflectRlpVenue, user: &Keypair, in_mint: Pubkey, out_mint: Pubkey, amount: u64) -> Option<u64> {
    let req = QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount, swap_type: SwapType::ExactIn };
    let ix = match venue.generate_swap_instruction(req, user.pubkey()) {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("[sim_delta] generate_swap_instruction refused amt {amount}: {e:?}");
            return None;
        },
    };
    let dst = ata(&user.pubkey(), &out_mint);
    let before = token_balance(svm, &dst);
    match try_send(svm, ix, user) {
        Ok(()) => Some(token_balance(svm, &dst) - before),
        Err(logs) => {
            eprintln!("[sim_delta] tx failed amt {amount} {in_mint}->{out_mint}:");
            for l in &logs {
                eprintln!("    {l}");
            }
            None
        },
    }
}

// ---------------------------------------------------------------------------
// Deployed program over the mainnet snapshot of pool 4.
// ---------------------------------------------------------------------------

struct Prod {
    svm: LiteSVM,
    clock: Clock,
    pool: Pubkey,
    lp_mint: Pubkey,
    asset_mint: Pubkey,
}

fn boot_prod() -> Prod {
    let snap = snapshot(&fixtures(), "rlp_prod");
    let mut svm = LiteSVM::new();
    svm.add_program(RLP_PROGRAM, &program_bytes(&fixtures(), "rlp_prod"));
    load(&mut svm, &snap);
    let clock = snap.clock();
    set_clock(&mut svm, &clock);
    let (pool, pool_data) = snap.accounts.iter().find(|a| a.1 == RLP_PROGRAM && a.2.len() == LiquidityPoolView::LEN).map(|a| (a.0, &a.2)).unwrap();
    let pv = LiquidityPoolView::from_bytes(pool_data).unwrap();
    let lp_mint = Pubkey::new_from_array(pv.lp_token());
    let asset_mint = snap
        .accounts
        .iter()
        .filter(|a| a.1 == RLP_PROGRAM && (a.2.len() == AssetView::LEN || a.2.len() == AssetView::LEN_LEGACY))
        .map(|a| AssetView::from_bytes(&a.2).unwrap())
        .find(|v| pv.has_asset(v.index()))
        .map(|v| Pubkey::new_from_array(v.mint()))
        .unwrap();
    Prod { svm, clock, pool, lp_mint, asset_mint }
}

impl Prod {
    fn user(&mut self, asset: u64) -> Keypair {
        let user = funded_user(&mut self.svm);
        forge_ata(&mut self.svm, &user.pubkey(), &self.asset_mint, asset);
        forge_ata(&mut self.svm, &user.pubkey(), &self.lp_mint, 0);
        user
    }
}

/// Deposit into the live pool, atom-exact over a sweep; a fresh snapshot per
/// amount because execution grows the pool.
#[tokio::test(flavor = "current_thread")]
async fn prod_deposit_matches_the_program() {
    for amt in [1_000u64, 1_000_000, 123_456_789, 2_000_000_000] {
        let mut fx = boot_prod();
        let user = fx.user(5_000_000_000);
        let v = venue(&fx.svm, fx.pool, vec![fx.asset_mint]).await;
        assert_eq!(v.get_token_info().len(), 2, "[lp, asset]");
        let expected = quote_out(&v, fx.asset_mint, fx.lp_mint, amt).expect("deposit quotes on the live pool");
        assert!(expected > 0);
        let delta = sim_delta(&mut fx.svm, &v, &user, fx.asset_mint, fx.lp_mint, amt).expect("execution must succeed where the quote predicts output");
        assert_eq!(delta, expected, "deposit parity broke at {amt}");
    }
}

/// The LP-input direction is the cooldown withdrawal (no route, `bounds()`
/// finds nothing), zero input is a clean zero, and the deployed 120 s Pyth
/// window makes a stale clock refuse on both sides.
#[tokio::test(flavor = "current_thread")]
async fn prod_edges_agree_with_the_program() {
    let mut fx = boot_prod();
    let user = fx.user(5_000_000_000);
    let v = venue(&fx.svm, fx.pool, vec![fx.asset_mint]).await;
    assert!(quote_out(&v, fx.lp_mint, fx.asset_mint, 1_000_000).is_none());
    assert!(v.bounds(0, 1).is_err(), "LP -> asset has no quotable value");
    let z = v.quote(QuoteRequest { input_mint: fx.asset_mint, output_mint: fx.lp_mint, amount: 0, swap_type: SwapType::ExactIn }).unwrap();
    assert_eq!((z.expected_output, z.not_enough_liquidity), (0, false));

    let fresh_ix =
        v.generate_swap_instruction(QuoteRequest { input_mint: fx.asset_mint, output_mint: fx.lp_mint, amount: 1_000_000, swap_type: SwapType::ExactIn }, user.pubkey()).unwrap();
    let stale = Clock { unix_timestamp: fx.clock.unix_timestamp + 121, ..fx.clock.clone() };
    set_clock(&mut fx.svm, &stale);
    let v = venue(&fx.svm, fx.pool, vec![fx.asset_mint]).await; // quotes at the stale clock
    assert!(quote_out(&v, fx.asset_mint, fx.lp_mint, 1_000_000).is_none(), "stale deposit must quote no route");
    let logs = try_send(&mut fx.svm, fresh_ix, &user).expect_err("stale deposit must revert");
    // The deployed valuation walk maps every oracle failure to InvalidInput.
    assert!(logs.iter().any(|l| l.contains("InvalidInput")), "{logs:?}");
}

// ---------------------------------------------------------------------------
// Oracle-stacking build, bootstrapped through the program.
// ---------------------------------------------------------------------------

const NOW: Clock = Clock { slot: 500_000, epoch_start_timestamp: 0, epoch: 0, leader_schedule_epoch: 0, unix_timestamp: 1_800_000_000 };
const SWAP_FEE_BPS: u16 = 25;
const FEED_A: [u8; 32] = [0xa1; 32];
const FEED_B2: [u8; 32] = [0xb2; 32];
/// Asset A: $2.00 (Pyth, expo -8), 6 dp.
const PRICE_A: i64 = 200_000_000;
/// Asset B leg 0: Doppler 1.000000 (6 dp); leg 1: inverse Pyth $1.50, so B = 1 / 1.5 USD.
const DOPPLER_B: u64 = 1_000_000;
const PYTH_B: i64 = 150_000_000;
/// Genesis: 1,000 A ($2,000) and 3,000 B ($2,000).
const GENESIS_A: u64 = 1_000_000_000;
const GENESIS_B: u64 = 3_000_000_000_000;

struct Stacking {
    svm: LiteSVM,
    admin: Keypair,
    pool: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    lp_mint: Pubkey,
}

fn leg(oracle: Pubkey, feed_id: [u8; 32], provider: OracleProvider, inverse: bool) -> OracleLeg {
    OracleLeg { oracle: oracle.to_bytes(), feed_id, max_staleness: 0, inverse, provider, max_confidence_bps: 0, decimals: 0 }
}

/// Boot pool 0 THROUGH the program: initialize_rlp (admin becomes Supremo), add
/// both assets with banded chains, initialize the pool listing both, create both
/// reserves, open Deposit and Swap to the public, then seed both reserves with
/// genesis deposits built by the venue (each one a parity check of its own).
async fn boot_stacking() -> Stacking {
    let mut svm = LiteSVM::new();
    svm.add_program(RLP_PROGRAM, &program_bytes(&fixtures(), "rlp_prod"));
    set_clock(&mut svm, &NOW);
    let admin = funded_user(&mut svm);
    let pid = &RLP_PROGRAM;
    let pool = pda::derive_liquidity_pool(0, pid).0;

    let mint_a = Pubkey::new_unique();
    let mint_b = Pubkey::new_unique();
    let lp_mint = Pubkey::new_unique();
    forge_account(&mut svm, mint_a, TOKEN_PROGRAM, mint_bytes(&admin.pubkey(), 1_000_000_000_000, 6));
    forge_account(&mut svm, mint_b, TOKEN_PROGRAM, mint_bytes(&admin.pubkey(), 1_000_000_000_000_000, 9));
    forge_account(&mut svm, lp_mint, TOKEN_PROGRAM, mint_bytes(&pool, 0, 6));

    let oracle_a = Pubkey::new_unique();
    let oracle_b0 = Pubkey::new_unique();
    let oracle_b1 = Pubkey::new_unique();
    forge_account(&mut svm, oracle_a, PYTH_RECEIVER, pyth_bytes(&FEED_A, PRICE_A, 1_000_000, -8, NOW.unix_timestamp - 5, NOW.slot - 5));
    forge_account(&mut svm, oracle_b0, DOPPLER_ORACLE, doppler_bytes(NOW.slot - 5, DOPPLER_B, 6));
    forge_account(&mut svm, oracle_b1, PYTH_RECEIVER, pyth_bytes(&FEED_B2, PYTH_B, 1_000_000, -8, NOW.unix_timestamp - 5, NOW.slot - 5));

    let permissions = pda::derive_permissions(&admin.pubkey(), pid).0;
    send(&mut svm, "initialize_rlp", initialize_rlp(admin.pubkey(), permissions, settings(), SWAP_FEE_BPS, pid), &admin);

    let asset_a = pda::derive_asset(&mint_a, pid).0;
    let asset_b = pda::derive_asset(&mint_b, pid).0;
    let ix = add_asset(
        &AddAssetAccounts { signer: admin.pubkey(), admin: permissions, settings: settings(), asset: asset_a, asset_mint: mint_a, oracle: oracle_a },
        AccessLevel::Public,
        &[leg(oracle_a, FEED_A, OracleProvider::Pyth, false)],
        0,
        PriceBand { min_price_1e6: 1_000_000, max_price_1e6: 3_000_000 },
        vec![],
        pid,
    );
    send(&mut svm, "add_asset A", ix, &admin);
    let ix = add_asset(
        &AddAssetAccounts { signer: admin.pubkey(), admin: permissions, settings: settings(), asset: asset_b, asset_mint: mint_b, oracle: oracle_b0 },
        AccessLevel::Public,
        &[leg(oracle_b0, oracle_b0.to_bytes(), OracleProvider::Doppler, false), leg(oracle_b1, FEED_B2, OracleProvider::Pyth, true)],
        0,
        PriceBand { min_price_1e6: 500_000, max_price_1e6: 800_000 },
        vec![AccountMeta::new_readonly(oracle_b1, false)],
        pid,
    );
    send(&mut svm, "add_asset B", ix, &admin);

    let ix = initialize_lp(
        &InitializeLpAccounts {
            signer: admin.pubkey(),
            permissions,
            settings: settings(),
            liquidity_pool: pool,
            lp_token_mint: lp_mint,
            dead_shares_vault: pda::token_ata(&pool, &lp_mint),
        },
        3_600,
        None,
        &[0, 1],
        None,
        pid,
    );
    send(&mut svm, "initialize_lp", ix, &admin);
    for (asset, mint) in [(asset_a, mint_a), (asset_b, mint_b)] {
        let ix = initialize_pool_reserve(
            &InitializePoolReserveAccounts {
                signer: admin.pubkey(),
                permissions,
                settings: settings(),
                liquidity_pool: pool,
                asset,
                asset_mint: mint,
                pool_asset_account: pda::token_ata(&pool, &mint),
            },
            0,
            pid,
        );
        send(&mut svm, "initialize_pool_reserve", ix, &admin);
    }
    let main = AdminMainAccounts { admin: admin.pubkey(), settings: settings(), admin_permissions: permissions };
    send(&mut svm, "open deposit", update_action_role(&main, Action::Deposit, Role::Public, Update::Add, pid), &admin);
    send(&mut svm, "open swap", update_action_role(&main, Action::Swap, Role::Public, Update::Add, pid), &admin);

    let mut fx = Stacking { svm, admin, pool, mint_a, mint_b, lp_mint };
    let seed = fx.user(GENESIS_A, GENESIS_B);
    for (mint, amt, label) in [(mint_a, GENESIS_A, "A"), (mint_b, GENESIS_B, "B")] {
        let v = fx.venue().await; // rebuilt per deposit: the pool grows
        assert_eq!(v.get_token_info().len(), 3, "[lp, a, b]");
        let expected = quote_out(&v, mint, lp_mint, amt).expect("genesis deposit quotes");
        let delta = sim_delta(&mut fx.svm, &v, &seed, mint, lp_mint, amt).expect("genesis deposit executes");
        assert_eq!(delta, expected, "genesis deposit {label} parity");
    }
    fx
}

impl Stacking {
    fn user(&mut self, a: u64, b: u64) -> Keypair {
        let user = funded_user(&mut self.svm);
        forge_ata(&mut self.svm, &user.pubkey(), &self.mint_a, a);
        forge_ata(&mut self.svm, &user.pubkey(), &self.mint_b, b);
        forge_ata(&mut self.svm, &user.pubkey(), &self.lp_mint, 0);
        user
    }

    async fn venue(&self) -> ReflectRlpVenue {
        venue(&self.svm, self.pool, vec![self.mint_a, self.mint_b]).await
    }

    fn admin_main(&self) -> AdminMainAccounts {
        AdminMainAccounts { admin: self.admin.pubkey(), settings: settings(), admin_permissions: pda::derive_permissions(&self.admin.pubkey(), &RLP_PROGRAM).0 }
    }
}

/// Deposit of either asset and both swap directions, atom-exact over a sweep
/// (price impact + the 25 bps fee on swaps). A fresh boot per amount: execution
/// mutates the pool.
#[tokio::test(flavor = "current_thread")]
async fn stacking_deposit_and_swap_match_the_program() {
    for amt in [1_000u64, 1_000_000, 123_456_789, 2_000_000_000] {
        for dir in 0..4 {
            let mut fx = boot_stacking().await;
            let (in_mint, out_mint) = match dir {
                0 => (fx.mint_a, fx.lp_mint),
                1 => (fx.mint_b, fx.lp_mint),
                2 => (fx.mint_a, fx.mint_b),
                _ => (fx.mint_b, fx.mint_a),
            };
            let user = fx.user(10_000_000_000, 10_000_000_000_000);
            let v = fx.venue().await;
            let Some(expected) = quote_out(&v, in_mint, out_mint, amt) else {
                // The one legal None in a healthy pool: an output that rounds to zero
                // (1,000 atoms of 9 dp B is $6.7e-7). The program refuses the swap
                // (NotEnoughFunds) and ACCEPTS the deposit minting 0 LP; either way
                // nothing is paid out, so "no route" is the right answer for a router.
                assert_eq!(amt, 1_000, "unexpected no-route {in_mint}->{out_mint} at {amt}");
                let paid = sim_delta(&mut fx.svm, &v, &user, in_mint, out_mint, amt).unwrap_or(0);
                assert_eq!(paid, 0, "quote said no route but execution paid {paid}");
                continue;
            };
            assert!(expected > 0);
            let delta = sim_delta(&mut fx.svm, &v, &user, in_mint, out_mint, amt).expect("execution must succeed where the quote predicts output");
            assert_eq!(delta, expected, "parity broke: {in_mint}->{out_mint} amt {amt}: quote {expected} sim {delta}");
        }
    }
}

/// Predicted reverts the venue reports as no route, each confirmed by the
/// program: a stale Doppler leg, a deposit past the cap, and the swap killswitch.
#[tokio::test(flavor = "current_thread")]
async fn stacking_predicted_reverts_agree_with_the_program() {
    let mut fx = boot_stacking().await;
    let user = fx.user(10_000_000_000, 10_000_000_000_000);

    // Stale Doppler leg (200 slots): the fresh-built swap reverts once the clock moves.
    let v = fx.venue().await;
    let fresh_swap =
        v.generate_swap_instruction(QuoteRequest { input_mint: fx.mint_a, output_mint: fx.mint_b, amount: 1_000_000, swap_type: SwapType::ExactIn }, user.pubkey()).unwrap();
    let stale = Clock { slot: NOW.slot + 200, ..NOW };
    set_clock(&mut fx.svm, &stale);
    let v = fx.venue().await;
    assert!(quote_out(&v, fx.mint_a, fx.mint_b, 1_000_000).is_none(), "stale swap must quote no route");
    assert!(quote_out(&v, fx.mint_b, fx.lp_mint, 1_000_000_000).is_none(), "stale deposit must quote no route");
    let logs = try_send(&mut fx.svm, fresh_swap, &user).expect_err("stale swap must revert");
    assert!(logs.iter().any(|l| l.contains("OracleDataTooStale")), "{logs:?}");
    set_clock(&mut fx.svm, &NOW);

    // Deposit cap just above the supply: 1,000 units of A ($0.002) fit, 1,000,000 do not.
    let supply = mint_supply(&fx.svm, &fx.lp_mint);
    let cap = update_deposit_cap(fx.admin.pubkey(), fx.admin_main().admin_permissions, settings(), fx.pool, 0, Some(supply + 5_000), &RLP_PROGRAM);
    send(&mut fx.svm, "cap", cap, &fx.admin);
    let v = fx.venue().await;
    assert!(quote_out(&v, fx.mint_a, fx.lp_mint, 1_000_000).is_none(), "over-cap deposit must quote no route");
    assert!(sim_delta(&mut fx.svm, &v, &user, fx.mint_a, fx.lp_mint, 1_000_000).is_none(), "the venue refuses to build past the cap");
    let expected = quote_out(&v, fx.mint_a, fx.lp_mint, 1_000).expect("under-cap deposit quotes");
    assert_eq!(sim_delta(&mut fx.svm, &v, &user, fx.mint_a, fx.lp_mint, 1_000), Some(expected));
    let lift = update_deposit_cap(fx.admin.pubkey(), fx.admin_main().admin_permissions, settings(), fx.pool, 0, None, &RLP_PROGRAM);
    send(&mut fx.svm, "lift cap", lift, &fx.admin);

    // Killswitch on swap: the pre-freeze instruction reverts, deposits stay live.
    let v = fx.venue().await;
    let pre_freeze =
        v.generate_swap_instruction(QuoteRequest { input_mint: fx.mint_a, output_mint: fx.mint_b, amount: 1_000_000, swap_type: SwapType::ExactIn }, user.pubkey()).unwrap();
    let freeze = freeze_functionality(&fx.admin_main(), Action::FreezeSwap, true, &RLP_PROGRAM);
    send(&mut fx.svm, "freeze swap", freeze, &fx.admin);
    let v = fx.venue().await;
    assert!(quote_out(&v, fx.mint_a, fx.mint_b, 1_000_000).is_none(), "frozen swap must quote no route");
    let logs = try_send(&mut fx.svm, pre_freeze, &user).expect_err("frozen swap must revert");
    assert!(logs.iter().any(|l| l.contains("Frozen")), "{logs:?}");
    let expected = quote_out(&v, fx.mint_b, fx.lp_mint, 1_000_000_000).expect("deposit unaffected");
    assert_eq!(sim_delta(&mut fx.svm, &v, &user, fx.mint_b, fx.lp_mint, 1_000_000_000), Some(expected));
}
