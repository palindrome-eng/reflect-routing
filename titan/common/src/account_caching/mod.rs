//! Account retrieval abstraction (vendored from the Titan integration-template).
//!
//! A venue receives an `&dyn AccountsCache` during `update_state()` and uses it
//! to fetch on-chain accounts. The framework provides the concrete cache; this
//! crate vendors only the trait (the upstream RPC-backed `rpc_cache` helper is
//! omitted — it is not needed to implement a venue, and it pulls a network
//! client dependency the adapter does not).

use async_trait::async_trait;
use solana_account::Account;
use solana_pubkey::Pubkey;
use thiserror::Error;

/// Abstracts account retrieval for Titan. Implementations must be `Send + Sync`,
/// return accounts in the same order as the requested pubkeys, and surface
/// missing accounts as `Ok(None)` rather than an error.
#[async_trait]
pub trait AccountsCache: Send + Sync {
    /// Retrieve a single account by pubkey (`Ok(None)` if known-missing).
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, AccountCacheError>;

    /// Retrieve multiple accounts; the result has the same length and order as
    /// `pubkeys`.
    async fn get_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>, AccountCacheError>;
}

/// Errors from the account-fetching layer (not venue-level errors).
#[derive(Debug, Error)]
pub enum AccountCacheError {
    /// The backing store failed to fetch account data.
    #[error("Failed to fetch account: {0}")]
    FailedToFetchAccount(String),

    /// Failure acquiring a write lock, if the cache uses lock-based concurrency.
    #[error("Failed to acquire write lock")]
    FailedToAcquireWriteLock,

    /// Failure acquiring a read lock.
    #[error("Failed to acquire read lock")]
    FailedToAcquireReadLock,
}

/// `AccountCacheError` is `Send + Sync` at compile time.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AccountCacheError>();
};
