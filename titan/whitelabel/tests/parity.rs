//! Off-chain `quote(amount)` must equal the on-chain execution delta, atom-exact,
//! both directions, over an amount sweep. Both adapters delegate quote/build to
//! the same `reflect-routing-core` venue, so executing the Titan venue's
//! [`ReflectWhitelabelVenue::generate_swap_instruction`] (a complete wrap/unwrap
//! ix) proves the property for Titan AND Jupiter.
//!
//! Two DUMPED mainnet programs over mainnet snapshots, both committed under
//! `tests/fixtures` (see `SOURCES.md`):
//! - `proxy_prod.so` with a prod market over a 98 B Pyth asset (10-account
//!   wrap/unwrap, staleness by posted slot);
//! - `proxy_staging.so` with a staging market over a 259 B two-leg asset (an
//!   extra-oracle tail, staleness by publish time on the second leg).
//!
//! MANDATORY: a missing fixture is a test FAILURE, not a skip.

use std::path::PathBuf;

use litesvm::LiteSVM;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use reflect_sdk::proxy::ids::{PROXY_PROGRAM, PROXY_STAGING_PROGRAM};
use reflect_sdk::proxy::ProxyStateView;

use reflect_titan_common::harness::*;
use reflect_titan_common::trading_venue::{FromAccount, QuoteRequest, SwapType, TradingVenue};
use reflect_titan_whitelabel::ReflectWhitelabelVenue;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

struct Fx {
    svm: LiteSVM,
    clock: Clock,
    market: Pubkey,
    branded: Pubkey,
    stablecoin: Pubkey,
}

fn boot(program: &str, program_id: Pubkey) -> Fx {
    let snap = snapshot(&fixtures(), program);
    let mut svm = LiteSVM::new();
    svm.add_program(program_id, &program_bytes(&fixtures(), program));
    load(&mut svm, &snap);
    let clock = snap.clock();
    set_clock(&mut svm, &clock);
    let (market, data) = snap.accounts.iter().find(|a| a.1 == program_id && a.2.len() == ProxyStateView::LEN).map(|a| (a.0, &a.2)).unwrap();
    let view = ProxyStateView::from_bytes(data).unwrap();
    Fx { svm, clock, market, branded: Pubkey::new_from_array(view.branded_mint()), stablecoin: Pubkey::new_from_array(view.stablecoin_mint()) }
}

impl Fx {
    fn user(&mut self, stable: u64, branded: u64) -> Keypair {
        let user = funded_user(&mut self.svm);
        forge_ata(&mut self.svm, &user.pubkey(), &self.stablecoin, stable);
        forge_ata(&mut self.svm, &user.pubkey(), &self.branded, branded);
        user
    }

    /// The venue over the live SVM state (the clock sysvar included).
    async fn venue(&self) -> ReflectWhitelabelVenue {
        let account = self.svm.get_account(&self.market).unwrap();
        let mut v = ReflectWhitelabelVenue::from_account(&self.market, &account).expect("from_account");
        v.update_state(&SvmCache(&self.svm)).await.expect("update_state");
        assert_eq!(v.get_token_info().len(), 2, "[stablecoin, branded]");
        v
    }
}

fn quote_out(venue: &ReflectWhitelabelVenue, in_mint: Pubkey, out_mint: Pubkey, amount: u64) -> Option<u64> {
    let q = venue.quote(QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount, swap_type: SwapType::ExactIn }).expect("quote");
    (!q.not_enough_liquidity).then_some(q.expected_output)
}

/// Build the venue's instruction, execute it as `user`, and return the out-mint
/// balance delta. `None` = the venue refused to build or the tx failed (logged).
fn sim_delta(svm: &mut LiteSVM, venue: &ReflectWhitelabelVenue, user: &Keypair, in_mint: Pubkey, out_mint: Pubkey, amount: u64) -> Option<u64> {
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

const SWEEP: [u64; 4] = [1_000, 1_000_000, 50_000_000, 250_000_000];

/// Wrap then unwrap on the live market, atom-exact over the sweep; the unwrap
/// runs against the post-wrap state and burns half of what was minted.
async fn wrap_then_unwrap(fx: &mut Fx, expected_accounts: usize) {
    for amt in SWEEP {
        let user = fx.user(1_000_000_000, 0);
        let v = fx.venue().await;
        let ix =
            v.generate_swap_instruction(QuoteRequest { input_mint: fx.stablecoin, output_mint: fx.branded, amount: amt, swap_type: SwapType::ExactIn }, user.pubkey()).unwrap();
        assert_eq!(ix.accounts.len(), expected_accounts);
        let expected = quote_out(&v, fx.stablecoin, fx.branded, amt).expect("wrap quotes on the live market");
        let minted = sim_delta(&mut fx.svm, &v, &user, fx.stablecoin, fx.branded, amt).expect("wrap executes");
        assert_eq!(minted, expected, "wrap parity broke at {amt}");

        let v = fx.venue().await; // the vault grew
        let burn = minted / 2;
        let expected = quote_out(&v, fx.branded, fx.stablecoin, burn).expect("unwrap quotes");
        let paid = sim_delta(&mut fx.svm, &v, &user, fx.branded, fx.stablecoin, burn).expect("unwrap executes");
        assert_eq!(paid, expected, "unwrap parity broke at {burn}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn prod_wrap_and_unwrap_match_the_program() {
    let mut fx = boot("proxy_prod", PROXY_PROGRAM);
    wrap_then_unwrap(&mut fx, 10).await;
}

#[tokio::test(flavor = "current_thread")]
async fn staging_two_leg_wrap_and_unwrap_match_the_program() {
    let mut fx = boot("proxy_staging", PROXY_STAGING_PROGRAM);
    wrap_then_unwrap(&mut fx, 11).await;
}

/// Zero input is a clean zero on both sides; a stale oracle makes the quote
/// report no liquidity AND the pre-built wrap revert.
async fn edges(fx: &mut Fx, stale: Clock, expected_log: &str) {
    let user = fx.user(1_000_000_000, 0);
    let v = fx.venue().await;
    for (a, b) in [(fx.stablecoin, fx.branded), (fx.branded, fx.stablecoin)] {
        let z = v.quote(QuoteRequest { input_mint: a, output_mint: b, amount: 0, swap_type: SwapType::ExactIn }).unwrap();
        assert_eq!((z.expected_output, z.not_enough_liquidity), (0, false), "amount 0 is not a liquidity failure");
    }
    let fresh =
        v.generate_swap_instruction(QuoteRequest { input_mint: fx.stablecoin, output_mint: fx.branded, amount: 1_000_000, swap_type: SwapType::ExactIn }, user.pubkey()).unwrap();
    set_clock(&mut fx.svm, &stale);
    let v = fx.venue().await; // quotes at the stale clock
    assert!(quote_out(&v, fx.stablecoin, fx.branded, 1_000_000).is_none(), "stale oracle must quote no route");
    let logs = try_send(&mut fx.svm, fresh, &user).expect_err("stale oracle must revert");
    assert!(logs.iter().any(|l| l.contains(expected_log)), "{logs:?}");
}

/// Prod: staleness is 200 slots from the POSTED slot; `ProxyError::StaleOracle` is variant 13.
#[tokio::test(flavor = "current_thread")]
async fn prod_edges_agree_with_the_program() {
    let mut fx = boot("proxy_prod", PROXY_PROGRAM);
    let stale = Clock { slot: fx.clock.slot + 200, ..fx.clock.clone() };
    edges(&mut fx, stale, "custom program error: 0xd").await;
}

/// Staging: the second leg's publish time gates the chain (600 s at 1500 slots).
#[tokio::test(flavor = "current_thread")]
async fn staging_edges_agree_with_the_program() {
    let mut fx = boot("proxy_staging", PROXY_STAGING_PROGRAM);
    let stale = Clock { unix_timestamp: fx.clock.unix_timestamp + 600, ..fx.clock.clone() };
    edges(&mut fx, stale, "custom program error: 0xd").await;
}
