//! Reflect venue legs, invoked as TitanPDA for the route-sized amount.
//!
//! Lending and index legs forward the leg's accounts as they are, with data from the SDK's
//! encoders. Lending mint and redeem take only the amount: no state is loaded and no quote
//! runs. An index leg loads its accounts for the one thing its data needs from state: the
//! asset ids of a swap, or the USD value of a mint or redeem at the handler's clock; nothing
//! else of the quote runs. Both keep the leg inside the 32 KiB heap. An rlp or whitelabel leg
//! loads its accounts in place through
//! `reflect-sdk`, quotes, and builds. No instruction bytes are encoded here; the SDK is the
//! only encoder.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use reflect_sdk::rlp::LiquidityPoolView;
use reflect_sdk::strategy::ix::{mint_index_data, mint_lending_data, redeem_index_data, redeem_lending_data, swap_index_data};
use reflect_sdk::strategy::{IndexRoute, TokenInput};
use reflect_sdk::{BuiltInstruction, InsurancePool, Now, Strategy, Whitelabel};

use crate::error::TemplateError;
use crate::instructions::swap_route_v3::{leg_metas, Leg};
use crate::state::Venue;

/// Deployments a leg may CPI into, per variant. The leg's trailing account must be one of them.
pub const REFLECT_PROGRAMS: [Pubkey; 1] = [reflect_sdk::strategy::ids::REFLECT_PROGRAM];
pub const RLP_STAGING_PROGRAM: Pubkey = pubkey!("GSEYK2FtDLywAoxn2mioXCft9FWH6Zn4VmKUArGyTVj8");
pub const PROXY_STAGING_PROGRAM: Pubkey = pubkey!("HvX27r2MoKn6oakkmcAgMo5ZFhheGKQ5Ht2KL3sk5ANM");
pub const RLP_PROGRAMS: [Pubkey; 2] = [reflect_sdk::rlp::ids::RLP_PROGRAM, RLP_STAGING_PROGRAM];
pub const PROXY_PROGRAMS: [Pubkey; 2] = [reflect_sdk::proxy::ids::PROXY_PROGRAM, PROXY_STAGING_PROGRAM];

/// Named-meta position of the account each SDK loader is keyed on, per instruction family
/// (`reflect_sdk::{strategy, rlp, proxy}::ix` meta order). `tests/venue_parity.rs` proves them.
pub const LENDING_CONTROLLER: usize = 5;
pub const LENDING_RECEIPT_MINT: usize = 6;
pub const INDEX_CONTROLLER: usize = 4;
pub const RLP_POOL: usize = 3;
pub const PROXY_BRANDED_MINT: usize = 5;

/// The SDK builds its `min_out` from its own quote at this clock, which the dispatcher's
/// realized-delta check supersedes; a fully open slippage keeps that floor out of the way (the
/// proxy SDK floors it at 1, which its program requires).
const CPI_SLIPPAGE_BPS: u16 = 10_000;

/// The floor in a forwarded leg's instruction data: open, for the same reason.
const CPI_MIN_OUT: u64 = 0;

/// A leg that forwards its accounts as they are.
fn forwarded(program_id: Pubkey, leg: &Leg, infos: &[AccountInfo], data: Vec<u8>) -> BuiltInstruction {
    BuiltInstruction { instruction: Instruction { program_id, accounts: leg_metas(infos, &leg.titan_pda), data }, lookup_tables: Vec::new(), extra_signers: Vec::new() }
}

/// Build the leg's Reflect instruction for `leg.amount`.
///
/// # Errors
/// `VenueProgram` when the trailing account is not an allowed deployment; the SDK's load, quote
/// or build error otherwise (an unknown mint pair, a stale oracle, a predicted revert). A
/// forwarded leg has no predicted revert: the venue program refuses it.
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
            let data = if leg.in_mint == key(LENDING_RECEIPT_MINT)? { redeem_lending_data(leg.amount, CPI_MIN_OUT) } else { mint_lending_data(leg.amount, CPI_MIN_OUT) };
            forwarded(program_id, leg, infos, data)
        },
        Venue::ReflectBonds { .. } => {
            // Swap takes the two asset ids, and mint and redeem take a USD value: all come from
            // live state. The loader and its account borrows end with this block, before the
            // dispatcher invokes.
            let data = {
                let s = Strategy::from_infos(program_id, key(INDEX_CONTROLLER)?, infos)?;
                match s.index_route(leg.in_mint, leg.out_mint)? {
                    IndexRoute::Swap { in_asset_id, out_asset_id } => swap_index_data(in_asset_id, leg.amount, out_asset_id, CPI_MIN_OUT),
                    IndexRoute::Mint { in_asset_id } => {
                        let transfers = [TokenInput { asset_id: in_asset_id, amount: leg.amount }];
                        let value = s.index(user)?.prepare_quote(now)?.mint_usd_value(&transfers)?;
                        // `mint_index` refuses a zero value.
                        require!(value > 0, TemplateError::InvalidSwapInput);
                        mint_index_data(value, CPI_MIN_OUT, &transfers)
                    },
                    IndexRoute::RedeemByReceipt { out_asset_id } => {
                        let value = s.index(user)?.prepare_quote(now)?.redeem_usd_target(leg.amount)?;
                        // `redeem_index` reads a zero value as "withdraw max".
                        require!(value > 0, TemplateError::InvalidSwapInput);
                        // Ceiling, not a target: u64::MAX never over-allocates (the allocator
                        // clamps to pool value in checked u128 math).
                        redeem_index_data(value, CPI_MIN_OUT, &[TokenInput { asset_id: out_asset_id, amount: u64::MAX }])
                    },
                }
            };
            forwarded(program_id, leg, infos, data)
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
