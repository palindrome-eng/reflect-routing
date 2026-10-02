//! Supported pool/AMM protocol types (vendored from the Titan template).
//!
//! One `Reflect<Product>` variant per `titan/<product>` crate - there is
//! exactly one Titan protocol-family type, so landing a new product means
//! adding one variant here, not forking this file (mirrors how
//! `jupiter-common::Swap` unifies every product's Jupiter tags into one enum).

use std::fmt::Display;

/// Identifies the protocol family of a trading venue.
#[derive(Debug, Copy, Clone)]
pub enum PoolProtocol {
    /// Example/custom protocol placeholder.
    YourPoolProtocol,
    /// Raydium's AMM (x*y=k) pools.
    RaydiumAMM,
    /// Reflect lending strategy: deposit ↔ receipt at the settled price-per-share.
    ReflectLending,
    /// Reflect bond basket: N-asset mint/swap/redeem against the receipt.
    ReflectBonds,
    /// Reflect whitelabel branded pair: wrap/unwrap at the cranked price.
    ReflectWhitelabel,
    /// Reflect rlp insurance pool: N-asset restake/swap against the LP token.
    ReflectRlp,
}

impl Display for PoolProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from(*self))
    }
}

impl From<PoolProtocol> for String {
    fn from(protocol: PoolProtocol) -> Self {
        match protocol {
            PoolProtocol::YourPoolProtocol => "YourPoolProtocol".to_string(),
            PoolProtocol::RaydiumAMM => "RaydiumAMM".to_string(),
            PoolProtocol::ReflectLending => "ReflectLending".to_string(),
            PoolProtocol::ReflectBonds => "ReflectBonds".to_string(),
            PoolProtocol::ReflectWhitelabel => "ReflectWhitelabel".to_string(),
            PoolProtocol::ReflectRlp => "ReflectRlp".to_string(),
        }
    }
}
