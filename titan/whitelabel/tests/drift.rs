//! Drift probes against the live staging proxy build: every gate the audit fixes added is driven
//! from both sides (venue quote vs program execution) and the disagreements are printed, then
//! asserted empty.

use litesvm::LiteSVM;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::path::PathBuf;

use reflect_sdk::proxy::ids::PROXY_STAGING_PROGRAM;
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
    vault: Pubkey,
}

fn boot() -> Fx {
    let snap = snapshot(&fixtures(), "proxy_staging");
    let mut svm = LiteSVM::new();
    svm.add_program(PROXY_STAGING_PROGRAM, &program_bytes(&fixtures(), "proxy_staging"));
    load(&mut svm, &snap);
    let clock = snap.clock();
    set_clock(&mut svm, &clock);
    let (market, data) = snap.accounts.iter().find(|a| a.1 == PROXY_STAGING_PROGRAM && a.2.len() == ProxyStateView::LEN).map(|a| (a.0, &a.2)).unwrap();
    let view = ProxyStateView::from_bytes(data).unwrap();
    let branded = Pubkey::new_from_array(view.branded_mint());
    let stablecoin = Pubkey::new_from_array(view.stablecoin_mint());
    let vault = snap.accounts.iter().find(|a| a.2.len() == 165).unwrap().0;
    Fx { svm, clock, market, branded, stablecoin, vault }
}

impl Fx {
    fn user(&mut self) -> Keypair {
        let user = funded_user(&mut self.svm);
        forge_ata(&mut self.svm, &user.pubkey(), &self.stablecoin, 1_000_000_000);
        forge_ata(&mut self.svm, &user.pubkey(), &self.branded, 1_000_000);
        user
    }
    async fn venue(&self) -> ReflectWhitelabelVenue {
        let account = self.svm.get_account(&self.market).unwrap();
        let mut v = ReflectWhitelabelVenue::from_account(&self.market, &account).unwrap();
        v.update_state(&SvmCache(&self.svm)).await.expect("update_state");
        v
    }
    fn patch(&mut self, key: Pubkey, f: impl FnOnce(&mut Vec<u8>)) {
        let mut a = self.svm.get_account(&key).unwrap();
        f(&mut a.data);
        self.svm.set_account(key, a).unwrap();
    }
}

fn quote(v: &ReflectWhitelabelVenue, i: Pubkey, o: Pubkey, amt: u64) -> Option<u64> {
    let q = v.quote(QuoteRequest { input_mint: i, output_mint: o, amount: amt, swap_type: SwapType::ExactIn }).unwrap();
    (!q.not_enough_liquidity).then_some(q.expected_output)
}

/// Execute `ix` as `user`; `Some(delta)` on the out mint or `None` with the error printed.
fn exec(svm: &mut LiteSVM, ix: solana_sdk::instruction::Instruction, user: &Keypair, out: Pubkey) -> Option<u64> {
    let dst = ata(&user.pubkey(), &out);
    let before = token_balance(svm, &dst);
    match try_send(svm, ix, user) {
        Ok(()) => Some(token_balance(svm, &dst) - before),
        Err(logs) => {
            let err = logs.iter().rev().find(|l| l.contains("failed") || l.contains("Error")).cloned().unwrap_or_default();
            eprintln!("      program: {err}");
            None
        },
    }
}

/// Pyth legs with stored `max_staleness = 1500`: the venue's model vs the program's, by slot
/// age and by publish-time age.
#[tokio::test(flavor = "current_thread")]
#[ignore = "SDK v0.11.0 models the pre-audit staging build; re-enable after the protocol-core sync"]
async fn staleness_boundaries() {
    let mut disagreements = vec![];
    for (dslot, dsec) in [(1_499u64, 0i64), (1_500, 0), (1_501, 0), (2_999, 0), (3_000, 0), (3_001, 0), (0, 599), (0, 600), (0, 601), (0, 1_500), (0, 1_501)] {
        let mut fx = boot();
        let user = fx.user();
        let v = fx.venue().await;
        let ix = v
            .generate_swap_instruction(QuoteRequest { input_mint: fx.stablecoin, output_mint: fx.branded, amount: 1_000_000, swap_type: SwapType::ExactIn }, user.pubkey())
            .unwrap();
        let later = Clock { slot: fx.clock.slot + dslot, unix_timestamp: fx.clock.unix_timestamp + dsec, ..fx.clock.clone() };
        set_clock(&mut fx.svm, &later);
        let v = fx.venue().await;
        let q = quote(&v, fx.stablecoin, fx.branded, 1_000_000);
        let e = exec(&mut fx.svm, ix, &user, fx.branded);
        let agree = q.is_some() == e.is_some();
        eprintln!(
            "  +{dslot:>5} slots +{dsec:>5} s  venue {:<8} program {:<8} {}",
            if q.is_some() { "route" } else { "none" },
            if e.is_some() { "ok" } else { "revert" },
            if agree { "" } else { "<-- DRIFT" }
        );
        if !agree {
            disagreements.push((dslot, dsec, q, e));
        }
    }
    assert!(disagreements.is_empty(), "staleness drift: {disagreements:?}");
}

/// Vault worth less than principal: the program refuses the wrap; does the venue?
#[tokio::test(flavor = "current_thread")]
#[ignore = "SDK v0.11.0 models the pre-audit staging build; re-enable after the protocol-core sync"]
async fn undercollateralised_wrap() {
    let mut fx = boot();
    let user = fx.user();
    // Vault 1,000,000 units at ~1.1855 is worth ~1,185,518 against principal 1,173,743. Halve it.
    let vault = fx.vault;
    fx.patch(vault, |d| d[64..72].copy_from_slice(&500_000u64.to_le_bytes()));
    let v = fx.venue().await;
    let q = quote(&v, fx.stablecoin, fx.branded, 1_000_000);
    let e = v
        .generate_swap_instruction(QuoteRequest { input_mint: fx.stablecoin, output_mint: fx.branded, amount: 1_000_000, swap_type: SwapType::ExactIn }, user.pubkey())
        .ok()
        .and_then(|ix| exec(&mut fx.svm, ix, &user, fx.branded));
    eprintln!("  undercollateralised wrap: venue {q:?} program {e:?}");
    assert_eq!(q.is_some(), e.is_some(), "venue {q:?} vs program {e:?}");
    if let (Some(q), Some(e)) = (q, e) {
        assert_eq!(q, e);
    }
}

/// Commission larger than the vault's excess over principal: the program prices wrap and unwrap
/// against min(principal, vault value - commission).
#[tokio::test(flavor = "current_thread")]
#[ignore = "SDK v0.11.0 models the pre-audit staging build; re-enable after the protocol-core sync"]
async fn commission_adjusted_pricing() {
    let mut rows = vec![];
    for dir in ["wrap", "unwrap"] {
        let mut fx = boot();
        let user = fx.user();
        let market = fx.market;
        // Vault value ~1,185,518; principal 1,173,743; a 200,000 commission pushes the
        // redeemable principal to ~985,518.
        fx.patch(market, |d| d[74..82].copy_from_slice(&200_000u64.to_le_bytes()));
        let v = fx.venue().await;
        let (i, o) = if dir == "wrap" { (fx.stablecoin, fx.branded) } else { (fx.branded, fx.stablecoin) };
        let q = quote(&v, i, o, 100_000);
        let e = v
            .generate_swap_instruction(QuoteRequest { input_mint: i, output_mint: o, amount: 100_000, swap_type: SwapType::ExactIn }, user.pubkey())
            .ok()
            .and_then(|ix| exec(&mut fx.svm, ix, &user, o));
        eprintln!("  {dir} with commission 200,000: venue {q:?} program {e:?}");
        rows.push((dir, q, e));
    }
    assert!(rows.iter().all(|(_, q, e)| q == e), "{rows:?}");
}
