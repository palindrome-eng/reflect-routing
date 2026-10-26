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
