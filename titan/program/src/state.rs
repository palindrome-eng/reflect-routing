use anchor_lang::prelude::*;

pub const MAX_SWAPS: usize = 12;
pub const MAX_MINTS: usize = 12;

/// Route venue selector. MUST stay byte-for-byte identical to `reflect_titan_common::swap_route::Venue`
/// (same variants, same order); `tests/venue_parity.rs` proves it. `min_out` is the leg's output
/// floor in out-token atoms, checked by the dispatcher on the realized TitanPDA balance delta.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Copy, Eq, Debug)]
pub enum Venue {
    RaydiumAmm,
    ReflectLending { min_out: u64 },
    ReflectBonds { min_out: u64 },
    ReflectRlp { min_out: u64 },
    ReflectWhitelabel { min_out: u64 },
}

impl Venue {
    /// The leg's output floor; none for venues that carry their own minimum in the CPI data.
    pub fn min_out(&self) -> Option<u64> {
        match *self {
            Venue::RaydiumAmm => None,
            Venue::ReflectLending { min_out } | Venue::ReflectBonds { min_out } | Venue::ReflectRlp { min_out } | Venue::ReflectWhitelabel { min_out } => Some(min_out),
        }
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Copy, Eq, Debug)]
pub struct SwapSpecInputV2 {
    pub venue: Venue,
    pub from: u8,
    pub to: u8,
    pub weight_nanos: u32,
    pub n_accounts: u8,
}

#[account]
pub struct TitanPda {
    pub bump: u8,
}

impl TitanPda {
    pub const SIZE: usize = 1;
    pub const SEED: &'static [u8] = b"titan_pda";
}
