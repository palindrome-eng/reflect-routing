//! Off-chain `quote(amount)` must equal the on-chain execution delta, atom-exact,
//! for deposit (asset -> LP) and swap (asset -> asset). Both adapters delegate
//! quote/build to the same `reflect-routing-core` venue, so executing the Titan
//! venue's `generate_swap_instruction` proves the property for Titan AND Jupiter.
//!
//! One program, `rlp_prod.so`, the DUMPED mainnet binary (see `SOURCES.md`), over a
//! mainnet snapshot of pool 4 (one migrated Pyth asset; Swap is crank-only there, so
//! deposit is the only public route).
//!
//! MANDATORY: a missing fixture is a test FAILURE, not a skip.

use std::path::PathBuf;

use litesvm::LiteSVM;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use reflect_sdk::rlp::ids::RLP_PROGRAM;
use reflect_sdk::rlp::{pda, AssetView, LiquidityPoolView};

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
