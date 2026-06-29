//! Token metadata for Titan venues (vendored from the Titan template).

use solana_account::Account;
use solana_pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_token::solana_program::clock::Epoch;
use spl_token_2022::{
    extension::{BaseStateWithExtensions, StateWithExtensions},
    state::Mint,
};

use crate::trading_venue::error::TradingVenueError;

/// Canonical program ID for standard SPL Token.
pub const TOKEN_PROGRAM_ID: Pubkey = spl_token::ID;

/// Canonical program ID for the SPL Token 2022 program.
pub const TOKEN_2022_PROGRAM_ID: Pubkey = spl_token_2022::ID;

/// Mint metadata exposed by a Titan venue.
#[derive(Default, Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenInfo {
    pub pubkey: Pubkey,
    pub decimals: i32,
    pub is_token_2022: bool,
    /// Transfer-fee basis points (Token-2022 TransferFee extension).
    pub transfer_fee: Option<u16>,
    /// Maximum fee in atom units per transfer.
    pub maximum_fee: Option<u64>,
}

impl TokenInfo {
    /// Construct `TokenInfo` by unpacking a mint account (SPL Token or Token-2022).
    pub fn new(pubkey: &Pubkey, account: &Account, epoch: Epoch) -> Result<Self, TradingVenueError> {
        if let Ok(mint) = StateWithExtensions::<Mint>::unpack(&account.data) {
            let transfer_fee: Option<u16>;
            let maximum_fee: Option<u64>;

            (transfer_fee, maximum_fee) = match mint.get_extension::<spl_token_2022::extension::transfer_fee::TransferFeeConfig>() {
                Ok(val) => {
                    let current_fees = val.get_epoch_fee(epoch);
                    let fee_bps: u16 = current_fees.transfer_fee_basis_points.into();
                    (Some(fee_bps), Some(current_fees.maximum_fee.into()))
                },
                Err(_) => (None, None),
            };

            Ok(TokenInfo { pubkey: *pubkey, decimals: mint.base.decimals as i32, is_token_2022: account.owner == spl_token_2022::ID, transfer_fee, maximum_fee })
        } else {
            Err(TradingVenueError::DeserializationFailed(pubkey.into()))
        }
    }

    /// The SPL token program ID appropriate for this mint.
    pub fn get_token_program(&self) -> Pubkey {
        if self.is_token_2022 {
            TOKEN_2022_PROGRAM_ID
        } else {
            TOKEN_PROGRAM_ID
        }
    }

    /// The associated token account address for `wallet_address`.
    pub fn get_associated_token_address(&self, wallet_address: &Pubkey) -> Pubkey {
        get_associated_token_address_with_program_id(wallet_address, &self.pubkey, &self.get_token_program())
    }
}
