#![allow(clippy::result_large_err)] // `TradingVenueError` is large; it is used everywhere.

//! `reflect-amm-titan` — the Reflect lending strategy as a Titan
//! [`TradingVenue`](reflect_titan_common::trading_venue::TradingVenue).
//!
//! The vendored `Titan-Pathfinder/integration-template` contract (the trait +
//! `AccountsCache` the framework consumes) lives in `reflect-titan-common`,
//! shared by every `titan/<product>` crate. All Reflect logic is in
//! [`reflect`], delegated to `reflect-sdk`.

pub mod reflect;

pub use reflect::ReflectLendingVenue;
