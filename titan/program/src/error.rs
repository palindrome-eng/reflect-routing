use anchor_lang::prelude::*;

#[error_code]
pub enum TemplateError {
    #[msg("Invalid swap input")]
    InvalidSwapInput,
    #[msg("Missing remaining account")]
    MissingRemainingAccount,
    #[msg("Invalid account data")]
    InvalidAccountData,
    #[msg("CPI swap violated reserved TitanPDA balance")]
    ReservedBalanceViolation,
    #[msg("Leg output below its minimum")]
    MinimumOutput,
    #[msg("Venue program is not an allowed deployment")]
    VenueProgram,
}
