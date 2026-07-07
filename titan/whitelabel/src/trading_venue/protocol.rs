//! Supported pool/AMM protocol types (vendored from the Titan template, with the
//! `ReflectWhitelabel` variant added).

use std::fmt::Display;

/// Identifies the protocol family of a trading venue.
#[derive(Debug, Copy, Clone)]
pub enum PoolProtocol {
    /// Example/custom protocol placeholder.
    YourPoolProtocol,
    /// Raydium's AMM (x*y=k) pools.
    RaydiumAMM,
    /// Reflect whitelabel branded pair: wrap/unwrap at the cranked price.
    ReflectWhitelabel,
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
            PoolProtocol::ReflectWhitelabel => "ReflectWhitelabel".to_string(),
        }
    }
}
