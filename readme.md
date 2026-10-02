# reflect-routing

Reflect Protocol AMM interface for integration with the Jupiter Router.

## Overview

This crate provides an AMM implementation for Reflect Protocol that enables routing between stablecoins (USDC/USDT) and their yield-bearing receipt tokens (USDC+/USDT+) through Jupiter.

**Supported Routes:**
- USDC ↔ USDC+
- USDT ↔ USDT+

The implementation uses [`reflect-exchange-rates`](https://crates.io/crates/reflect-exchange-rates) to calculate exchange rates and dynamically fetch required accounts from multiple protocols (Drift, Kamino, Jupiter).

## What is Reflect?

Reflect is credibly-neutral stablecoin software, where the software consists of decentralised financial strategies executed and managed permissionlessly by programs on SVM, rather than managed by humans. Reflect then tokenises these strategies in the form of interest-bearing US Dollars (stablecoins).

![Reflect Flow](image.png)

## Usage

The `ReflectAmm` struct implements the `Amm` trait and can be used with Jupiter's routing system:

```rust
use amm_reflect::ReflectAmm;

let mut amm = ReflectAmm::new();
amm.set_route_from_mints(&usdc_mint, &usdc_plus_mint)?;

// Get accounts needed for quote
let accounts = amm.get_accounts_to_update();

// Update with account data
amm.update(&account_map)?;

// Get quote
let quote = amm.quote(&QuoteParams {
    amount: 1_000_000_000,
    input_mint: usdc_mint,
    output_mint: usdc_plus_mint,
    swap_mode: SwapMode::ExactIn,
})?;
```

## Development

```bash
# Run tests
cargo test -- --nocapture
```

Tests will skip gracefully if controller account data is unavailable or invalid.
## Listing contract (rlp and whitelabel markets)

The Jupiter and Titan shims in `jupiter/*` and `titan/*` list one market per account.
The account's owner selects the deployment (mainnet or staging program id); the venue re-checks it against the fetched bytes.

**rlp** (`jupiter/rlp`, `titan/rlp`): the market is a `LiquidityPool` PDA.
The lister injects the pool's asset mints (`params.asset_mints` on Jupiter, the constructor on Titan): the pool lists registry indices and the `Asset` PDA is keyed by mint, so they cannot be read off the pool alone.
The injected set must be exactly the pool's assets, or `load` fails and the market must be re-listed.
Every asset must be `Public` and banded (`update_oracle` has set `FLAG_CHAIN_RESOLVED`).
Routes are asset -> LP (deposit) and asset -> asset (swap); LP as input is the cooldown withdrawal and never routes.
`Action::Swap` must be assigned `Role::Public` for asset -> asset routes to clear; on mainnet prod it is crank-only, so only deposits route there.
The swap fee (`swap_fee_bps`, after price impact) is reported in the output mint; deposits charge none.

**whitelabel** (`jupiter/whitelabel`, `titan/whitelabel`): the market is a `ProxyState` PDA.
Discovery takes three rounds (state -> its `Asset` -> vault, mints and every oracle leg).
The asset must be active and its oracle chain resolved, or `load` fails.
Routes are stablecoin -> branded (wrap) and branded -> stablecoin (unwrap); the fee is always 0 because the crank folds it into pricing.
