#![allow(clippy::result_large_err)] // `TradingVenueError` is large; it is used everywhere.

//! `reflect-titan-whitelabel` — the Reflect whitelabel branded pair as a Titan
//! [`TradingVenue`](trading_venue::TradingVenue).
//!
//! The `trading_venue` and `account_caching` modules vendor the upstream
//! `Titan-Pathfinder/integration-template` contract (the trait the framework
//! consumes); the RPC-backed cache helper is omitted — the framework supplies
//! its own [`AccountsCache`](account_caching::AccountsCache). All Reflect logic
//! is in [`reflect`], delegated to `reflect-sdk` via `reflect-routing-core`.

pub mod account_caching;
pub mod reflect;
pub mod trading_venue;

pub use reflect::ReflectWhitelabelVenue;
