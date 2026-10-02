//! `reflect-titan-common` — the vendored `Titan-Pathfinder/integration-template`
//! contract (the `TradingVenue` trait + supporting types), shared by every
//! `titan/<product>` crate so it is copied exactly once, not once per product.
//!
//! The RPC-backed cache helper from the upstream template is deliberately
//! omitted — the framework supplies its own [`account_caching::AccountsCache`].
//! Each `titan/<product>` crate depends on this crate and adds only its own
//! concrete `Reflect<Product>Venue` (in its own `reflect.rs`); `PoolProtocol`
//! (in [`trading_venue::protocol`]) carries one variant per product. The
//! `harness` feature adds the LiteSVM parity harness the product suites share.

pub mod account_caching;
#[cfg(feature = "harness")]
pub mod harness;
pub mod trading_venue;
