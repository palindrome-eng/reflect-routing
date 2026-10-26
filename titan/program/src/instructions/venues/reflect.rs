//! Reflect venue legs: the leg's accounts are loaded in place through `reflect-sdk`, the
//! quote runs at the handler's clock, the instruction is built for the route-sized amount and
//! invoked as TitanPDA. No instruction bytes are encoded here; the SDK is the only encoder.

use anchor_lang::prelude::*;
use reflect_sdk::rlp::LiquidityPoolView;
use reflect_sdk::strategy::{IndexRoute, TokenInput};
use reflect_sdk::{BuiltInstruction, InsurancePool, Now, Strategy, Whitelabel};

use crate::error::TemplateError;
use crate::instructions::swap_route_v3::Leg;
use crate::state::Venue;

/// The devnet mock deployment of the main program; the forge fixtures run under it.
pub const REFLECT_DEVNET_MOCK: Pubkey = pubkey!("moCKesf95X6JJQ4ZwyDTnWAQD5tbx5ZpnBnwEY7bFmy");

/// Deployments a leg may CPI into, per variant. The leg's trailing account must be one of them.
pub const REFLECT_PROGRAMS: [Pubkey; 2] = [reflect_sdk::strategy::ids::REFLECT_PROGRAM, REFLECT_DEVNET_MOCK];
pub const RLP_PROGRAMS: [Pubkey; 2] = [reflect_sdk::rlp::ids::RLP_PROGRAM, reflect_sdk::rlp::ids::RLP_STAGING_PROGRAM];
pub const PROXY_PROGRAMS: [Pubkey; 2] = [reflect_sdk::proxy::ids::PROXY_PROGRAM, reflect_sdk::proxy::ids::PROXY_STAGING_PROGRAM];

/// Named-meta position of the account each SDK loader is keyed on, per instruction family
/// (`reflect_sdk::{strategy, rlp, proxy}::ix` meta order). `tests/venue_parity.rs` proves them.
pub const LENDING_CONTROLLER: usize = 5;
pub const INDEX_CONTROLLER: usize = 4;
pub const RLP_POOL: usize = 3;
pub const PROXY_BRANDED_MINT: usize = 5;

/// The SDK builds its `min_out` from its own quote at this clock, which the dispatcher's
/// realized-delta check supersedes; a fully open slippage keeps that floor out of the way (the
/// proxy SDK floors it at 1, which its program requires).
const CPI_SLIPPAGE_BPS: u16 = 10_000;

/// Build the leg's Reflect instruction for `leg.amount`.
///
/// # Errors
/// `VenueProgram` when the trailing account is not an allowed deployment; the SDK's load, quote
/// or build error otherwise (an unknown mint pair, a stale oracle, a predicted revert).
pub fn build(venue: &Venue, leg: &Leg, infos: &[AccountInfo]) -> Result<BuiltInstruction> {
    let program_id = *infos.last().ok_or(TemplateError::MissingRemainingAccount)?.key;
    let key = |i: usize| infos.get(i).map(|a| *a.key).ok_or(TemplateError::MissingRemainingAccount);
    let clock = Clock::get()?;
    let now = Now { slot: clock.slot, unix_timestamp: clock.unix_timestamp };
    let user = leg.titan_pda;

    let allowed: &[Pubkey] = match venue {
        Venue::ReflectLending { .. } | Venue::ReflectBonds { .. } => &REFLECT_PROGRAMS,
        Venue::ReflectRlp { .. } => &RLP_PROGRAMS,
        Venue::ReflectWhitelabel { .. } => &PROXY_PROGRAMS,
        Venue::RaydiumAmm => return err!(TemplateError::InvalidSwapInput),
    };
    // Pinned before any leg byte is read: an unknown program never gets its accounts decoded.
    require!(allowed.contains(&program_id), TemplateError::VenueProgram);

    // Each arm loads, quotes and builds inside its own block so every account borrow the loader
    // holds is released before the dispatcher invokes.
    let built = match venue {
        Venue::ReflectLending { .. } => {
            let s = Strategy::from_infos(program_id, key(LENDING_CONTROLLER)?, infos)?;
            let h = s.lending(user)?;
            if leg.in_mint == s.receipt_mint() {
                h.redeem(leg.amount, CPI_SLIPPAGE_BPS, now)?.1
            } else {
                h.mint(leg.amount, CPI_SLIPPAGE_BPS, now)?.1
            }
        },
        Venue::ReflectBonds { .. } => {
            let s = Strategy::from_infos(program_id, key(INDEX_CONTROLLER)?, infos)?;
            let h = s.index(user)?;
            match s.index_route(leg.in_mint, leg.out_mint)? {
                IndexRoute::Mint { in_asset_id } => h.mint_exact_in(&[TokenInput { asset_id: in_asset_id, amount: leg.amount }], CPI_SLIPPAGE_BPS, now)?.1,
                IndexRoute::Swap { in_asset_id, out_asset_id } => h.swap(in_asset_id, leg.amount, out_asset_id, CPI_SLIPPAGE_BPS, now)?.1,
                // Ceiling, not a target: u64::MAX never over-allocates (the allocator clamps to
                // pool value in checked u128 math).
                IndexRoute::RedeemByReceipt { out_asset_id } => {
                    h.redeem_by_receipt(leg.amount, &[TokenInput { asset_id: out_asset_id, amount: u64::MAX }], CPI_SLIPPAGE_BPS, now)?.1
                },
            }
        },
        Venue::ReflectRlp { .. } => {
            let pool_index = {
                let pool = infos.get(RLP_POOL).ok_or(TemplateError::MissingRemainingAccount)?;
                let data = pool.try_borrow_data()?;
                LiquidityPoolView::from_bytes(&data).map_err(|_| TemplateError::InvalidAccountData)?.index()
            };
            let p = InsurancePool::from_infos(program_id, pool_index, infos)?;
            if leg.out_mint == p.lp_token() {
                p.deposit(user, leg.in_mint, leg.amount, CPI_SLIPPAGE_BPS, now)?.1
            } else {
                p.swap(user, leg.in_mint, leg.out_mint, leg.amount, CPI_SLIPPAGE_BPS, now)?.1
            }
        },
        Venue::ReflectWhitelabel { .. } => {
            let w = Whitelabel::from_infos(program_id, key(PROXY_BRANDED_MINT)?, infos)?;
            if leg.in_mint == w.stablecoin_mint() {
                w.wrap(user, leg.amount, CPI_SLIPPAGE_BPS, now)?.1
            } else {
                w.unwrap(user, leg.amount, CPI_SLIPPAGE_BPS, now)?.1
            }
        },
        Venue::RaydiumAmm => return err!(TemplateError::InvalidSwapInput),
    };
    Ok(built)
}
