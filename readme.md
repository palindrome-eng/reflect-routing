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