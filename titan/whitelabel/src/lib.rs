#![allow(clippy::result_large_err)] // `TradingVenueError` is large; it is used everywhere.

//! `reflect-titan-whitelabel` — the Reflect whitelabel branded pair as a Titan
//! [`TradingVenue`](reflect_titan_common::trading_venue::TradingVenue).
//!
//! The vendored `Titan-Pathfinder/integration-template` contract (the trait +
//! `AccountsCache` the framework consumes) lives in `reflect-titan-common`,
//! shared by every `titan/<product>` crate. All Reflect logic is in
//! [`reflect`], delegated to `reflect-sdk` via `reflect-routing-core`.

pub mod reflect;

pub use reflect::ReflectWhitelabelVenue;
