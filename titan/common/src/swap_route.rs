//! Off-chain side of the route program: a quoter [`TradingVenue`] becomes one `swap_route_v3`
//! leg. The [`Venue`] enum and [`SwapSpecInputV2`] MUST stay byte-for-byte identical to the
//! route program's `state.rs`; its `venue_parity` test proves it.
//!
//! Remaining-account layout of `swap_route_v3`:
//! `[0..mints]` TitanPDA token accounts, `[mints..2*mints]` the aligned mints, then every
//! leg's venue accounts concatenated, each leg ending with its venue program id.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use crate::trading_venue::{QuoteRequest, SwapType, TradingVenue, error::TradingVenueError, protocol::PoolProtocol};

/// The route program (Titan's `titan-v3-venue-template` id, never deployed by Reflect).
pub const ROUTER_PROGRAM_ID: Pubkey = Pubkey::from_str_const("T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT");
/// Seed of the custody PDA that signs every leg.
pub const TITAN_PDA_SEED: &[u8] = b"titan_pda";
/// Anchor discriminator of the `TitanPda` account (`sha256("account:TitanPda")[..8]`).
pub const TITAN_PDA_DISC: [u8; 8] = [251, 8, 48, 12, 67, 185, 133, 113];
/// Single-byte instruction discriminator of `swap_route_v3`.
pub const SWAP_ROUTE_V3_DISCRIMINATOR: u8 = 42;
/// Weight meaning "spend the whole available balance on this leg" (nanos).
pub const ROUTE_WEIGHT_ALL: u32 = 1_000_000_000;

/// The custody PDA `["titan_pda"]` of [`ROUTER_PROGRAM_ID`].
pub fn titan_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[TITAN_PDA_SEED], &ROUTER_PROGRAM_ID)
}

/// Route venue selector. `min_out` is the leg's output floor in out-token atoms, enforced
/// by the route program on the realized TitanPDA balance delta.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Venue {
    RaydiumAmm,
    ReflectLending { min_out: u64 },
    ReflectBonds { min_out: u64 },
    ReflectRlp { min_out: u64 },
    ReflectWhitelabel { min_out: u64 },
}

impl Venue {
    pub fn to_borsh_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("Venue serializes infallibly")
    }
}

/// One route leg, Borsh-identical to the program's `SwapSpecInputV2`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct SwapSpecInputV2 {
    pub venue: Venue,
    /// Index of the input mint in the route's mint list.
    pub from: u8,
    /// Index of the output mint.
    pub to: u8,
    /// Fraction of the available input balance to spend, in nanos.
    pub weight_nanos: u32,
    /// Accounts this leg consumes, INCLUDING the trailing venue program id.
    pub n_accounts: u8,
}

/// Map a quoter venue to its route [`Venue`] with `min_out` as the leg floor.
///
/// # Errors
/// `ExactOutNotSupported` for an ExactOut request; `UnsupportedVenue` for a protocol the
/// route program has no adapter for.
pub fn protocol_to_venue(venue: &dyn TradingVenue, request: &QuoteRequest, min_out: u64) -> Result<Venue, TradingVenueError> {
    if request.swap_type != SwapType::ExactIn {
        return Err(TradingVenueError::ExactOutNotSupported);
    }
    Ok(match venue.protocol() {
        PoolProtocol::RaydiumAMM => Venue::RaydiumAmm,
        PoolProtocol::ReflectLending => Venue::ReflectLending { min_out },
        PoolProtocol::ReflectBonds => Venue::ReflectBonds { min_out },
        PoolProtocol::ReflectRlp => Venue::ReflectRlp { min_out },
        PoolProtocol::ReflectWhitelabel => Venue::ReflectWhitelabel { min_out },
        other => return Err(TradingVenueError::UnsupportedVenue(other.to_string().into())),
    })
}

/// Build one `swap_route_v3` leg from the venue's own instruction: TitanPDA loses its signer
/// flag (it signs by seeds), the venue program id is appended read-only, and `n_accounts`
/// counts both.
pub fn build_swap_leg(
    venue: &dyn TradingVenue,
    request: &QuoteRequest,
    titan_pda: Pubkey,
    from: u8,
    to: u8,
    weight_nanos: u32,
    min_out: u64,
) -> Result<(SwapSpecInputV2, Vec<AccountMeta>), TradingVenueError> {
    let swap_ix = venue.generate_swap_instruction(request.clone(), titan_pda)?;
    let accounts = assemble_leg_accounts(&swap_ix, titan_pda, venue.program_id());
    let spec = SwapSpecInputV2 { venue: protocol_to_venue(venue, request, min_out)?, from, to, weight_nanos, n_accounts: accounts.len() as u8 };
    Ok((spec, accounts))
}

fn assemble_leg_accounts(swap_ix: &Instruction, titan_pda: Pubkey, venue_program_id: Pubkey) -> Vec<AccountMeta> {
    let mut accounts: Vec<AccountMeta> = swap_ix.accounts.iter().cloned().map(|acc| if acc.pubkey == titan_pda { AccountMeta { is_signer: false, ..acc } } else { acc }).collect();
    accounts.push(AccountMeta::new_readonly(venue_program_id, false));
    accounts
}

/// `swap_route_v3` instruction data: the discriminator byte then Borsh `(amount, mints, swaps)`.
pub fn encode_swap_route_v3_data(amount: u64, mints: u8, swaps: &[SwapSpecInputV2]) -> Vec<u8> {
    let mut data = vec![SWAP_ROUTE_V3_DISCRIMINATOR];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(mints);
    data.extend_from_slice(&(swaps.len() as u32).to_le_bytes());
    for swap in swaps {
        swap.serialize(&mut data).expect("borsh into Vec is infallible");
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leg_accounts_clear_titan_signer_and_append_program() {
        let titan = Pubkey::new_unique();
        let venue_program = Pubkey::new_unique();
        let ix = Instruction { program_id: venue_program, accounts: vec![AccountMeta::new(titan, true), AccountMeta::new(Pubkey::new_unique(), false)], data: vec![] };
        let accounts = assemble_leg_accounts(&ix, titan, venue_program);
        assert_eq!(accounts.len(), 3);
        assert!(!accounts[0].is_signer && accounts[0].is_writable);
        assert_eq!(accounts[2], AccountMeta::new_readonly(venue_program, false));
    }
}
