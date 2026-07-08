//! Off-chain `quote(amount)` must equal the on-chain execution delta, atom-exact,
//! both directions, over an amount sweep. Both adapters delegate quote/build to
//! the same `reflect-sdk` math, so executing the Titan venue's
//! [`ReflectWhitelabelVenue::generate_swap_instruction`] (a complete wrap/unwrap
//! ix) proves the property for Titan AND Jupiter.
//!
//! Unlike lending/bonds, whitelabel needs NO forge: `ProxyState`/`Oracle` are
//! tiny, discriminator-free structs (verified byte-for-byte against
//! `reflect-proxy-program/program/src/{state,helpers}.rs`), so the state is
//! hand-synthesized here and executed against the LIVE mainnet proxy program
//! `.so` (`ProxycrBkRh1S1241nBRkqrJTgTX1ginHDR8xTLjYSE`), resolved once and
//! cached like the Kamino programs in `titan/lending/tests/parity.rs`.
//!
//! PORTABLE/OFFLINE: `cargo test` stays green with no network and no cached
//! `.so` — execution tests skip with an eprintln. Set `SOLANA_RPC_URL` once to
//! populate `tests/fixtures/programs/proxy_program.so` (gitignored, ~2MB).
//!
//! On-chain checks this harness satisfies (read from source, not assumed):
//! - `Oracle::load` requires the oracle account's KEY to equal the pinned
//!   `ORACLE_ID` and checks staleness via `Clock::get()?.slot` (the live SVM
//!   sysvar) — MAX_STALENESS = 200 slots.
//! - `wrap`'s `MintTo` names `proxy_state` as `mint_authority`, so the branded
//!   mint's on-chain `mint_authority` MUST be the `proxy_state` PDA.
//! - `unwrap`'s `Transfer` names `proxy_state` as the vault's transfer
//!   authority, so the vault's SPL `owner` field MUST be `proxy_state`.
//! - the vault address is `find_program_address([proxy_state, token_program,
//!   stablecoin_mint], ATA_PROGRAM)` — the same derivation as
//!   `reflect_sdk::proxy::pda::proxy_state_vault`.

use std::collections::HashMap;
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
use reflect_sdk::proxy::{ids, pda};

use reflect_titan_common::account_caching::{AccountCacheError, AccountsCache};
use reflect_titan_common::trading_venue::{FromAccount, QuoteRequest, SwapType, TradingVenue};
use reflect_titan_whitelabel::ReflectWhitelabelVenue;

fn manifest_rel(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}
fn program_path() -> PathBuf {
    manifest_rel("tests/fixtures/programs/proxy_program.so")
}

/// Local cache -> RPC dump (needs `SOLANA_RPC_URL`) -> `None` with a skip
/// reason, keeping `cargo test` green offline. Mirrors
/// `titan/lending/tests/parity.rs`'s Kamino resolver, but for the whitelabel
/// program itself: nothing here is forged, so there is no first-party binary
/// to commit.
fn resolve_program(test: &str) -> Option<Vec<u8>> {
    let cache = program_path();
    if cache.exists() {
        return std::fs::read(&cache).ok();
    }
    let Ok(url) = std::env::var("SOLANA_RPC_URL") else {
        eprintln!("[skip {test}] proxy_program.so absent and SOLANA_RPC_URL unset - set it to fetch {} once", ids::PROGRAM);
        return None;
    };
    eprintln!("[parity] dumping {} from RPC -> {}", ids::PROGRAM, cache.display());
    let so = dump_program(&url, &ids::PROGRAM)?;
    let _ = std::fs::create_dir_all(program_path().parent().unwrap());
    let _ = std::fs::write(&cache, &so);
    Some(so)
}

/// Fetch a BPF-upgradeable program's ELF bytes over JSON-RPC (`solana program
/// dump` equivalent). The blocking `RpcClient` spins its own runtime, so it
/// runs on a plain thread — calling it from `#[tokio::test]` directly panics.
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

// solana_account/solana_pubkey (adapter) and solana_sdk (litesvm) are the same
// wire layout under unified 2.2, but distinct types; bridge by bytes regardless.
fn sdk_key(k: &Pubkey) -> solana_sdk::pubkey::Pubkey {
    solana_sdk::pubkey::Pubkey::new_from_array(k.to_bytes())
}
fn sdk_to_pk(k: &solana_sdk::pubkey::Pubkey) -> Pubkey {
    Pubkey::new_from_array(k.to_bytes())
}
fn clock_sysvar_key() -> Pubkey {
    sdk_to_pk(&solana_sdk::sysvar::clock::ID)
}
fn to_sdk_account(a: &Account) -> solana_sdk::account::Account {
    solana_sdk::account::Account { lamports: a.lamports, data: a.data.clone(), owner: sdk_key(&a.owner), executable: a.executable, rent_epoch: a.rent_epoch }
}
/// The bincode-serialized Clock sysvar account the adapter's `update_state`
/// fetches (`Clock::from_sysvar_bytes`).
fn clock_sysvar_account(clock: &SolanaClock) -> Account {
    Account { lamports: 1, data: bincode::serialize(clock).expect("ser clock"), owner: sdk_to_pk(&solana_sdk::sysvar::id()), executable: false, rent_epoch: 0 }
}

/// A hand-synthesized branded market: one `ProxyState` mid-life (non-zero
/// principal/commission, vault worth MORE than recorded principal +
/// commission — every quote in this fixture exercises the crank), backing a
/// non-trivial branded supply. All byte layouts verified against
/// `reflect-protocol-core::proxy` / `reflect-proxy-program::state`.
struct Fixture {
    proxy_state: Pubkey,
    branded_mint: Pubkey,
    stablecoin_mint: Pubkey,
    user: Keypair,
    clock: SolanaClock,
    map: HashMap<Pubkey, Account>,
}

const FEE_BPS: u16 = 1_000; // 10% of realized interest to the integrator
const PRINCIPAL: u64 = 900_000_000; // recorded principal, USDC value (6dp)
const BRANDED_SUPPLY: u64 = 900_000_000; // par with principal pre-crank
const VAULT_BALANCE: u64 = 1_000_000_000; // > principal: crank fires on every quote
const ORACLE_SLOT: u64 = 500;
const ORACLE_PRICE: u64 = 1_000_000_000_000; // $1.00, precision 12
const ORACLE_PRECISION: u8 = 12;

fn build_fixture() -> Fixture {
    let branded_mint = Pubkey::new_unique();
    let stablecoin_mint = Pubkey::new_unique();
    let (proxy_state, bump) = pda::derive_proxy_state(&branded_mint);
    let vault = pda::proxy_state_vault(&proxy_state, &stablecoin_mint);
    let user = Keypair::new();
    let user_pk = sdk_to_pk(&user.pubkey());

    let mut map = HashMap::new();

    // ProxyState: 115 bytes, no discriminator. Layout verified against
    // reflect-protocol-core/src/proxy/mod.rs.
    let mut ps = vec![0u8; 115];
    ps[0..32].copy_from_slice(branded_mint.as_ref());
    ps[32..64].copy_from_slice(stablecoin_mint.as_ref());
    ps[64..66].copy_from_slice(&FEE_BPS.to_le_bytes());
    ps[66..74].copy_from_slice(&PRINCIPAL.to_le_bytes());
    ps[74..82].copy_from_slice(&0u64.to_le_bytes()); // integrators_commission
    ps[114] = bump;
    map.insert(proxy_state, Account { lamports: 1_000_000, data: ps, owner: ids::PROGRAM, executable: false, rent_epoch: 0 });

    // Oracle: 17 bytes, no discriminator, at the PROGRAM-PINNED key (Oracle::load
    // checks the account's KEY, not its owner).
    let mut oracle = vec![0u8; 17];
    oracle[0..8].copy_from_slice(&ORACLE_SLOT.to_le_bytes());
    oracle[8..16].copy_from_slice(&ORACLE_PRICE.to_le_bytes());
    oracle[16] = ORACLE_PRECISION;
    map.insert(ids::ORACLE, Account { lamports: 1_000_000, data: oracle, owner: Pubkey::new_unique(), executable: false, rent_epoch: 0 });

    // Branded mint: mint_authority MUST be proxy_state (wrap's MintTo signs
    // with the proxy_state PDA seeds).
    map.insert(branded_mint, mint_account(&proxy_state, BRANDED_SUPPLY, 6));
    // Stablecoin mint: only fetched for decimals; authority is unused on-chain.
    map.insert(stablecoin_mint, mint_account(&Pubkey::new_unique(), 0, 6));

    // Vault: SPL owner field MUST be proxy_state (unwrap's Transfer signs with
    // the proxy_state PDA seeds); mint = stablecoin_mint.
    map.insert(vault, token_account(&stablecoin_mint, &proxy_state, VAULT_BALANCE));

    // User ATAs: stablecoin funded generously for wrap; branded funded for
    // unwrap. Both present every run (fresh SVM per amount below).
    let stablecoin_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user.pubkey(), &sdk_key(&stablecoin_mint), &sdk_key(&TOKEN_PROGRAM));
    let branded_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user.pubkey(), &sdk_key(&branded_mint), &sdk_key(&TOKEN_PROGRAM));
    map.insert(sdk_to_pk(&stablecoin_ata), token_account(&stablecoin_mint, &user_pk, u64::MAX / 2));
    map.insert(sdk_to_pk(&branded_ata), token_account(&branded_mint, &user_pk, 500_000_000));

    let clock = SolanaClock { slot: ORACLE_SLOT + 5, unix_timestamp: 1_700_000_000, ..Default::default() };
    map.insert(clock_sysvar_key(), clock_sysvar_account(&clock));

    Fixture { proxy_state, branded_mint, stablecoin_mint, user, clock, map }
}

/// SPL Mint (82 bytes): `mint_authority = Some(authority)`, given `supply`/`decimals`.
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

/// SPL TokenAccount (165 bytes): `owner = owner`, `mint = mint`, `amount`.
fn token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Account {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    let state = spl_token::state::Account { mint: sdk_key(mint), owner: sdk_key(owner), amount, state: spl_token::state::AccountState::Initialized, ..Default::default() };
    spl_token::state::Account::pack(state, &mut data).expect("pack token account");
    Account { lamports: 2_039_280, data, owner: TOKEN_PROGRAM, executable: false, rent_epoch: 0 }
}

/// [`AccountsCache`] over the fixture map. Contract: order-preserving, miss -> None.
struct FixtureCache {
    map: HashMap<Pubkey, Account>,
}

#[async_trait]
impl AccountsCache for FixtureCache {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, AccountCacheError> {
        Ok(self.map.get(pubkey).cloned())
    }
    async fn get_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>, AccountCacheError> {
        Ok(pubkeys.iter().map(|k| self.map.get(k).cloned()).collect())
    }
}

/// A LiteSVM with the real proxy program loaded and every fixture account set.
fn load_svm(fx: &Fixture, program: &[u8]) -> LiteSVM {
    let mut svm = LiteSVM::new();
    svm.add_program(sdk_key(&ids::PROGRAM), program);
    for (k, acc) in &fx.map {
        if acc.executable || *k == clock_sysvar_key() {
            continue;
        }
        let _ = svm.set_account(sdk_key(k), to_sdk_account(acc));
    }
    svm.set_sysvar::<SolanaClock>(&fx.clock);
    svm.airdrop(&fx.user.pubkey(), 1_000_000_000).ok();
    svm
}

fn token_balance(svm: &LiteSVM, ata: solana_sdk::pubkey::Pubkey) -> Option<u64> {
    let acc = svm.get_account(&ata)?;
    spl_token::state::Account::unpack(&acc.data).ok().map(|a| a.amount)
}

/// Build the venue's swap ix, execute it in the SVM, and return the dest
/// balance delta. `None` means the tx failed (logged to stderr) or the venue
/// wouldn't build the ix at all.
fn sim_delta(svm: &mut LiteSVM, venue: &ReflectWhitelabelVenue, fx: &Fixture, in_mint: Pubkey, out_mint: Pubkey, amount: u64) -> Option<u64> {
    let req = QuoteRequest { input_mint: in_mint, output_mint: out_mint, amount, swap_type: SwapType::ExactIn };
    let ix = match venue.generate_swap_instruction(req, sdk_to_pk(&fx.user.pubkey())) {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("[sim_delta] generate_swap_instruction failed amt {amount}: {e:?}");
            return None;
        },
    };
    let dst_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&fx.user.pubkey(), &sdk_key(&out_mint), &sdk_key(&TOKEN_PROGRAM));
    let before = token_balance(svm, dst_ata)?;

    let sdk_ix = solana_sdk::instruction::Instruction {
        program_id: sdk_key(&ix.program_id),
        accounts: ix.accounts.iter().map(|m| solana_sdk::instruction::AccountMeta { pubkey: sdk_key(&m.pubkey), is_signer: m.is_signer, is_writable: m.is_writable }).collect(),
        data: ix.data.clone(),
    };
    let tx = Transaction::new_signed_with_payer(&[sdk_ix], Some(&fx.user.pubkey()), &[&fx.user], svm.latest_blockhash());
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

/// Build the venue from the fixture's `ProxyState` account and `update_state`
/// it off the fixture map.
async fn setup(fx: &Fixture) -> ReflectWhitelabelVenue {
    let ps_account = fx.map.get(&fx.proxy_state).cloned().expect("proxy_state in map");
    let mut venue = ReflectWhitelabelVenue::from_account(&fx.proxy_state, &ps_account).expect("from_account");
    let cache = FixtureCache { map: fx.map.clone() };
    venue.update_state(&cache).await.expect("update_state");
    assert_eq!(venue.get_token_info().len(), 2, "venue must resolve exactly [stablecoin, branded]");
    venue
}

const SWEEP_AMOUNTS: [u64; 4] = [1_000, 1_000_000, 50_000_000, 200_000_000];

/// `delta.abs_diff(quote.expected_output) == 0` - exact, both directions, over
/// a sweep. Every amount here exercises the crank (vault > principal +
/// commission at fixture genesis).
#[tokio::test(flavor = "current_thread")]
async fn parity_wrap_and_unwrap_swept() {
    let Some(program) = resolve_program("parity_wrap_and_unwrap_swept") else {
        return;
    };
    let fx = build_fixture();
    let venue = setup(&fx).await;

    for amt in SWEEP_AMOUNTS {
        let q = venue.quote(QuoteRequest { input_mint: fx.stablecoin_mint, output_mint: fx.branded_mint, amount: amt, swap_type: SwapType::ExactIn }).expect("wrap quote");
        assert!(!q.not_enough_liquidity, "wrap amount {amt} unexpectedly no-liquidity");
        let mut svm = load_svm(&fx, &program);
        let delta = sim_delta(&mut svm, &venue, &fx, fx.stablecoin_mint, fx.branded_mint, amt).expect("simulate wrap");
        assert_eq!(delta, q.expected_output, "wrap parity broke at amount {amt}: quote {} sim {delta}", q.expected_output);
    }

    for amt in SWEEP_AMOUNTS {
        let q = venue.quote(QuoteRequest { input_mint: fx.branded_mint, output_mint: fx.stablecoin_mint, amount: amt, swap_type: SwapType::ExactIn }).expect("unwrap quote");
        assert!(!q.not_enough_liquidity, "unwrap amount {amt} unexpectedly no-liquidity");
        let mut svm = load_svm(&fx, &program);
        let delta = sim_delta(&mut svm, &venue, &fx, fx.branded_mint, fx.stablecoin_mint, amt).expect("simulate unwrap");
        assert_eq!(delta, q.expected_output, "unwrap parity broke at amount {amt}: quote {} sim {delta}", q.expected_output);
    }
}

/// Edges: amount 0 -> clean 0, never a liquidity failure; a stale oracle (>200
/// slots past `ORACLE_SLOT`) makes the quote report no-liquidity AND the
/// on-chain wrap revert - they must agree.
#[tokio::test(flavor = "current_thread")]
async fn edges_zero_and_stale_oracle() {
    let Some(program) = resolve_program("edges_zero_and_stale_oracle") else {
        return;
    };
    let fx = build_fixture();
    let venue = setup(&fx).await;

    for (a, b) in [(fx.stablecoin_mint, fx.branded_mint), (fx.branded_mint, fx.stablecoin_mint)] {
        let z = venue.quote(QuoteRequest { input_mint: a, output_mint: b, amount: 0, swap_type: SwapType::ExactIn }).expect("zero quote");
        assert_eq!(z.expected_output, 0);
        assert!(!z.not_enough_liquidity, "amount 0 is not a liquidity failure");
    }

    // Roll the clock 201 slots past the oracle - the venue's OWN clock is
    // fixed at construction time (fetched once in update_state), so build a
    // second venue against a fixture whose clock is stale from the start.
    let mut stale = build_fixture();
    let stale_clock = SolanaClock { slot: ORACLE_SLOT + 201, unix_timestamp: stale.clock.unix_timestamp, ..Default::default() };
    stale.map.insert(clock_sysvar_key(), clock_sysvar_account(&stale_clock));
    stale.clock = stale_clock;
    let stale_venue = setup(&stale).await;

    let q = stale_venue
        .quote(QuoteRequest { input_mint: stale.stablecoin_mint, output_mint: stale.branded_mint, amount: 1_000_000, swap_type: SwapType::ExactIn })
        .expect("stale quote");
    assert!(q.not_enough_liquidity, "stale oracle must report no liquidity off-chain");

    let mut svm = load_svm(&stale, &program);
    let sim = sim_delta(&mut svm, &stale_venue, &stale, stale.stablecoin_mint, stale.branded_mint, 1_000_000);
    assert!(sim.is_none(), "stale oracle must revert on-chain - quote and execution agree");
}

/// Offline: the FixtureCache honors the order/missing contract with no .so.
#[tokio::test(flavor = "current_thread")]
async fn offline_fixture_cache_contract() {
    let a = Pubkey::new_unique();
    let b = Pubkey::new_unique();
    let mut map = HashMap::new();
    map.insert(a, Account { lamports: 1, data: vec![], owner: Pubkey::default(), executable: false, rent_epoch: 0 });
    let cache = FixtureCache { map };

    let got = cache.get_accounts(&[a, b]).await.unwrap();
    assert_eq!(got.len(), 2);
    assert!(got[0].is_some());
    assert!(got[1].is_none(), "missing account must be Ok(None), not an error");
    assert!(cache.get_account(&b).await.unwrap().is_none());
}
