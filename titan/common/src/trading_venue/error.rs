//! Error types for Titan trading-venue integration (vendored from the template).

use std::fmt::Display;

use solana_pubkey::Pubkey;
use thiserror::Error;

use crate::{account_caching::AccountCacheError, trading_venue::protocol::PoolProtocol};

/// Attaches context (a pubkey or message) to an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorInfo {
    Pubkey(Pubkey),
    String(String),
    StaticStr(&'static str),
}

impl From<Pubkey> for ErrorInfo {
    fn from(pubkey: Pubkey) -> Self {
        ErrorInfo::Pubkey(pubkey)
    }
}

impl From<&Pubkey> for ErrorInfo {
    fn from(pubkey: &Pubkey) -> Self {
        ErrorInfo::Pubkey(*pubkey)
    }
}

impl From<String> for ErrorInfo {
    fn from(string: String) -> Self {
        ErrorInfo::String(string)
    }
}

impl From<&'static str> for ErrorInfo {
    fn from(string: &'static str) -> Self {
        ErrorInfo::StaticStr(string)
    }
}

impl Display for ErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorInfo::Pubkey(pubkey) => write!(f, "{}", pubkey),
            ErrorInfo::String(string) => write!(f, "{}", string),
            ErrorInfo::StaticStr(string) => write!(f, "{}", string),
        }
    }
}

/// Errors surfaced from venue init, state update, quoting, or instruction build.
#[allow(clippy::large_enum_variant)]
#[derive(Error, Debug)]
pub enum TradingVenueError {
    #[error("No account found for pubkey: {0}")]
    NoAccountFound(ErrorInfo),

    #[error("Unable to build venue from account for pubkey: {0}")]
    FromAccountError(ErrorInfo),

    #[error("Failed to fetch multiple account data")]
    FailedToFetchMultipleAccountData,

    #[error("Failed to fetch account data: {0}")]
    FailedToFetchAccountData(ErrorInfo),

    #[error("Failed to deserialize account data: {0}")]
    DeserializationFailed(ErrorInfo),

    #[error("Failed to unlock cache")]
    CacheUnlockFailed,

    #[error("The venue has not had its accounts loaded")]
    NotInitialized(ErrorInfo),

    #[error("The state object is not loaded: {0}")]
    MissingState(ErrorInfo),

    #[error("Invalid mint: {0}")]
    InvalidMint(ErrorInfo),

    #[error("Checked math error: {0}")]
    CheckedMathError(ErrorInfo),

    #[error("Error from method: {0}")]
    AmmMethodError(ErrorInfo),

    #[error("Exact Out swap type is not supported")]
    ExactOutNotSupported,

    #[error("Data conversion error: {0}")]
    DataConversionError(ErrorInfo),

    #[error("Boundary search failed: {0}")]
    BoundarySearchFailed(ErrorInfo),

    #[error("Boundary search failed: {0}")]
    NoQuotableValue(ErrorInfo),

    #[error("Something went wrong: {0}")]
    SomethingWentWrong(Box<dyn std::error::Error>),

    #[error("Unsupported venue: {0}")]
    UnsupportedVenue(ErrorInfo),

    #[error("Token info does not extend to index {0}")]
    TokenInfoIndexError(usize),

    #[error("Math Error: {0}")]
    MathError(ErrorInfo),

    #[error("Deserialization Error: {0}")]
    DeserializationError(ErrorInfo),

    #[error("Pool {0} from protocol {1} is inactive")]
    InactivePoolError(Pubkey, PoolProtocol),

    #[error("Account cache error: {0}")]
    AccountCacheError(#[from] AccountCacheError),
}
