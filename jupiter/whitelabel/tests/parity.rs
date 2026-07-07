//! Asserts the Jupiter whitelabel adapter's quote equals the shared
//! [`WhitelabelVenue`] and pins the `get_swap_and_account_metas` account-meta
//! ORDER per route as the golden contract.
//!
//! Jupiter's end-to-end swap cannot run here - the `ReflectProxy*` tags are
//! placeholders for Jupiter's closed program - so the test validates the owned
//! parts:
//!   - `ReflectWhitelabelAmm::quote` == `reflect-routing-core`
//!     `WhitelabelVenue::quote`. Both adapters delegate to that one call, and
//!     the Titan whitelabel suite (`titan/whitelabel/tests/parity.rs`) proves
//!     it equals on-chain execution atom-for-atom against the LIVE mainnet
//!     proxy program, so quote==execution transfers here WITHOUT an SVM.
//!   - the `account_metas` ORDER per route - wrap and unwrap share ONE order
//!     (the proxy program's `ix.rs` names identical accounts for both), so
//!     the TAG is the only direction carrier; this suite pins that fact.
//!
//! OFFLINE: unlike bonds (which reuses Titan's committed forge dump),
//! `ProxyState`/`Oracle` are tiny discriminator-free structs - state is
//! hand-synthesized in-process here, no fixture file needed.

use std::sync::atomic::Ordering;

use solana_sdk::account::Account;
use solana_sdk::clock::Clock as SolanaClock;
use solana_sdk::pubkey::Pubkey;

use reflect_routing_core::{WhitelabelRoute, WhitelabelVenue};
use reflect_sdk::common::Clock as ReflectClock;
use reflect_sdk::proxy::{ids, pda};

use reflect_jupiter_whitelabel::{AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, ReflectWhitelabelAmm, Swap, SwapMode, SwapParams};

const ORACLE_SLOT: u64 = 500;
const ORACLE_PRICE: u64 = 1_000_000_000_000; // $1.00, precision 12
const ORACLE_PRECISION: u8 = 12;

/// A hand-synthesized branded market: same genesis shape as
/// `core/src/whitelabel.rs`'s unit tests, at the CANONICAL PDA for `branded`.
struct Fixture {
    program_id: Pubkey,
    proxy_state: Pubkey,
    branded_mint: Pubkey,
    stablecoin_mint: Pubkey,
    clock: SolanaClock,
    map: AccountMap,
}

fn proxy_state_buf(branded: &Pubkey, stablecoin: &Pubkey, fee: u16, principal: u64, commission: u64, bump: u8) -> Vec<u8> {
    let mut b = vec![0u8; 115];
    b[0..32].copy_from_slice(branded.as_ref());
    b[32..64].copy_from_slice(stablecoin.as_ref());
    b[64..66].copy_from_slice(&fee.to_le_bytes());
    b[66..74].copy_from_slice(&principal.to_le_bytes());
    b[74..82].copy_from_slice(&commission.to_le_bytes());
    b[114] = bump;
    b
}
fn oracle_buf(slot: u64, price: u64, precision: u8) -> Vec<u8> {
    let mut b = vec![0u8; 17];
    b[0..8].copy_from_slice(&slot.to_le_bytes());
    b[8..16].copy_from_slice(&price.to_le_bytes());
    b[16] = precision;
    b
}
fn mint_buf(supply: u64, decimals: u8) -> Vec<u8> {
    let mut b = vec![0u8; 82];
    b[36..44].copy_from_slice(&supply.to_le_bytes()); // Mint.supply @36
    b[44] = decimals; // Mint.decimals @44
    b
}
fn token_buf(amount: u64) -> Vec<u8> {
    let mut b = vec![0u8; 165];
    b[64..72].copy_from_slice(&amount.to_le_bytes()); // TokenAccount.amount @64
    b
}
fn account(owner: Pubkey, data: Vec<u8>) -> Account {
    Account { lamports: 1_000_000, data, owner, executable: false, rent_epoch: 0 }
}

/// A market mid-life: vault worth more than recorded principal (every quote
/// exercises the crank), branded supply par with principal pre-crank.
fn build_fixture() -> Fixture {
    let branded_mint = Pubkey::new_unique();
    let stablecoin_mint = Pubkey::new_unique();
    let (proxy_state, bump) = pda::derive_proxy_state(&branded_mint);
    let vault = pda::proxy_state_vault(&proxy_state, &stablecoin_mint);

    let mut map = AccountMap::default();
    map.insert(proxy_state, account(ids::PROGRAM, proxy_state_buf(&branded_mint, &stablecoin_mint, 1_000, 900_000_000, 0, bump)));
    map.insert(ids::ORACLE, account(Pubkey::new_unique(), oracle_buf(ORACLE_SLOT, ORACLE_PRICE, ORACLE_PRECISION)));
    map.insert(vault, account(Pubkey::new_unique(), token_buf(1_000_000_000)));
    map.insert(branded_mint, account(Pubkey::new_unique(), mint_buf(900_000_000, 6)));
    map.insert(stablecoin_mint, account(Pubkey::new_unique(), mint_buf(0, 6)));

    let clock = SolanaClock { slot: ORACLE_SLOT + 5, unix_timestamp: 1_700_000_000, ..Default::default() };
    Fixture { program_id: ids::PROGRAM, proxy_state, branded_mint, stablecoin_mint, clock, map }
}

/// Wire the Jupiter Amm from the fixture (cold `from_keyed_account` → `update`
/// ingests the seed then the discovery set from the same map - up to two
/// rounds to warm, mirroring the bonds/lending suites).
fn setup(fx: &Fixture) -> (ReflectWhitelabelAmm, ClockRef) {
    let clock_ref: ClockRef = fx.clock.clone().into();
    let ctx = AmmContext { clock_ref: clock_ref.clone() };
    let ka = KeyedAccount { key: fx.proxy_state, account: fx.map.get(&fx.proxy_state).unwrap().clone() };

    let mut amm = ReflectWhitelabelAmm::from_keyed_account(&ka, &ctx).expect("from_keyed_account");
    amm.update(&fx.map).expect("update round 1");
    if amm.get_reserve_mints().len() < 2 {
        amm.update(&fx.map).expect("update round 2"); // re-run after dynamic accounts resolved
    }
    assert_eq!(amm.get_reserve_mints().len(), 2, "amm must resolve [stablecoin, branded]");
    (amm, clock_ref)
}

/// The shared delegate both adapters call, built independently from the same
/// bytes - the quote oracle. The Titan whitelabel LiteSVM suite proves ITS
/// quotes equal on-chain execution, so equality here transfers that proof.
fn core_venue(fx: &Fixture) -> WhitelabelVenue {
    let mut v = WhitelabelVenue::from_market(fx.proxy_state);
    let state = fx.map.get(&fx.proxy_state).unwrap().data.clone();
    let keys = v.discovery_keys(&state).expect("discovery");
    v.load(keys.iter().filter_map(|k| fx.map.get(k).map(|a| (*k, a.owner, a.data.clone())))).expect("load");
    v
}

fn reflect_clock(clock: &ClockRef) -> ReflectClock {
    ReflectClock { slot: clock.slot.load(Ordering::Relaxed), unix: clock.unix_timestamp.load(Ordering::Relaxed) }
}

/// The Jupiter adapter's quote must equal the shared `WhitelabelVenue` quote
/// (= Titan's, = on-chain execution via the Titan whitelabel suite). Both
/// directions, swept.
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_equals_core_venue() {
    let fx = build_fixture();
    let (amm, clock) = setup(&fx);
    let venue = core_venue(&fx);
    let rc = reflect_clock(&clock);

    for (in_mint, out_mint) in [(fx.stablecoin_mint, fx.branded_mint), (fx.branded_mint, fx.stablecoin_mint)] {
        for amount in [1_000u64, 1_000_000, 50_000_000] {
            let q = amm.quote(&QuoteParams { amount, input_mint: in_mint, output_mint: out_mint, swap_mode: SwapMode::ExactIn }).expect("jupiter quote");
            let core = venue.quote(in_mint, amount, rc).expect("core quote").expect("core route clears");
            assert_eq!(q.out_amount, core.amount_out, "jupiter quote != core venue for {in_mint}->{out_mint} amt {amount}");
            assert_eq!(q.in_amount, amount);
            assert_eq!(q.fee_amount, 0, "the crank fee is folded into pricing, never a scalar");
        }
    }
}

/// would_revert edge: a stale oracle (>200 slots past the recorded slot) maps
/// to no-route, so the Jupiter adapter returns `Err` (no route, not a bad route).
#[tokio::test(flavor = "current_thread")]
async fn jupiter_quote_errs_on_would_revert() {
    let mut fx = build_fixture();
    fx.clock.slot = ORACLE_SLOT + 201;
    let (amm, _clock) = setup(&fx);
    let r = amm.quote(&QuoteParams { amount: 1_000_000, input_mint: fx.stablecoin_mint, output_mint: fx.branded_mint, swap_mode: SwapMode::ExactIn });
    assert!(r.is_err(), "a stale oracle must surface as Err, not a bad route");
}

/// GOLDEN: the `account_metas` ORDER from `get_swap_and_account_metas` is the
/// eventual on-chain contract for each `ReflectProxy*` tag - indexed
/// positionally, so any reorder is breaking. Asserts the route→tag mapping,
/// that the user is the sole signer, and that wrap/unwrap share ONE order
/// (proving the tag - not the account order - carries direction here, unlike
/// lending's `ReflectS1`). Records each full order to stdout for the handoff.
#[tokio::test(flavor = "current_thread")]
async fn golden_account_metas_order_per_route() {
    let fx = build_fixture();
    let (amm, _clock) = setup(&fx);
    let venue = core_venue(&fx);
    let user = Pubkey::new_unique();

    let swap_params = |in_mint: Pubkey, out_mint: Pubkey| SwapParams {
        swap_mode: SwapMode::ExactIn,
        in_amount: 1_000_000,
        out_amount: 0,
        source_mint: in_mint,
        destination_mint: out_mint,
        source_token_account: Pubkey::new_unique(),
        destination_token_account: Pubkey::new_unique(),
        token_transfer_authority: user,
        quote_mint_to_referrer: None,
        jupiter_program_id: &fx.program_id,
        missing_dynamic_accounts_as_default: false,
    };

    let mut orders = vec![];
    for (in_mint, out_mint) in [(fx.stablecoin_mint, fx.branded_mint), (fx.branded_mint, fx.stablecoin_mint)] {
        let sp = swap_params(in_mint, out_mint);
        let sam = amm.get_swap_and_account_metas(&sp).expect("metas");

        let route = venue.route(in_mint).expect("route");
        let expected_tag = match route {
            WhitelabelRoute::Wrap => Swap::ReflectProxyWrap,
            WhitelabelRoute::Unwrap => Swap::ReflectProxyUnwrap,
        };
        assert_eq!(sam.swap, expected_tag, "route→tag mapping for {in_mint}->{out_mint}");
        assert!(!sam.account_metas.is_empty(), "metas must not be empty");

        // The user (token_transfer_authority) is the sole signer an aggregator
        // route carries.
        let signers: Vec<_> = sam.account_metas.iter().filter(|m| m.is_signer).map(|m| m.pubkey).collect();
        assert_eq!(signers, vec![user], "only the user signs an aggregator route ({in_mint}->{out_mint})");

        eprintln!("[golden] {:?} account_metas order ({} accounts):", sam.swap, sam.account_metas.len());
        for (i, m) in sam.account_metas.iter().enumerate() {
            eprintln!("  [{i:>2}] {} signer={} writable={}", m.pubkey, m.is_signer, m.is_writable);
        }
        orders.push(sam.account_metas.iter().map(|m| (m.pubkey, m.is_signer, m.is_writable)).collect::<Vec<_>>());
    }

    // Wrap and unwrap derive the SAME accounts (same user/branded/stablecoin
    // mints), so the full meta list is byte-identical - the tag above is the
    // ONLY direction carrier. Pin that fact explicitly.
    assert_eq!(orders[0], orders[1], "wrap and unwrap must share the exact same account-meta order");
}
