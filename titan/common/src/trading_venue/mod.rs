//! Core traits and data structures used by Titan-compatible trading venues
//! (vendored from `Titan-Pathfinder/integration-template`).

pub mod bounds;
pub mod error;
pub mod protocol;
pub mod token_info;

use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use crate::{
    account_caching::AccountsCache,
    trading_venue::{bounds::find_boundaries, error::TradingVenueError, protocol::PoolProtocol, token_info::TokenInfo},
};

/// Whether the user specifies the input amount (`ExactIn`) or the output amount
/// (`ExactOut`). Titan currently only calls venues with `ExactIn`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SwapType {
    ExactIn,
    ExactOut,
}

/// Request passed to a venue's `quote()` and `generate_swap_instruction()`. All
/// amounts are integer atom units.
#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount: u64,
    pub swap_type: SwapType,
}

/// The result of a venue's `quote()`.
#[derive(Debug, Clone)]
pub struct QuoteResult {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount: u64,
    pub expected_output: u64,
    pub not_enough_liquidity: bool,
}

/// Convert an on-chain account into a venue's internal state.
pub trait FromAccount {
    fn from_account(pubkey: &Pubkey, account: &Account) -> Result<Self, TradingVenueError>
    where
        Self: Sized;
}

/// Declare the address-lookup-table keys a venue needs for tx construction.
#[async_trait]
pub trait AddressLookupTableTrait {
    async fn get_lookup_table_keys(&self, accounts_cache: Option<&dyn AccountsCache>) -> Result<Vec<Pubkey>, TradingVenueError>;
}

/// The primary trait an AMM/venue implements to integrate with Titan.
#[async_trait]
pub trait TradingVenue {
    /// Whether the venue is fully initialized (state loaded).
    fn initialized(&self) -> bool;

    /// The main program ID for the venue.
    fn program_id(&self) -> Pubkey;

    /// Additional program IDs this venue depends on (e.g. SPL Token).
    fn program_dependencies(&self) -> Vec<Pubkey>;

    /// Unique identifier for the market/pool instance.
    fn market_id(&self) -> Pubkey;

    /// The mints traded in this venue (default: from `TokenInfo`).
    fn tradable_mints(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(self.get_token_info().iter().map(|x| x.pubkey).collect())
    }

    /// The decimals for each tradable token (default: from `TokenInfo`).
    fn decimals(&self) -> Result<Vec<i32>, TradingVenueError> {
        Ok(self.get_token_info().iter().map(|x| x.decimals).collect())
    }

    /// Fixed token metadata for this venue.
    fn get_token_info(&self) -> &[TokenInfo];

    /// Fetch a single token by index.
    fn get_token(&self, i: usize) -> Result<&TokenInfo, TradingVenueError> {
        self.get_token_info().get(i).ok_or(TradingVenueError::TokenInfoIndexError(i))
    }

    /// Which protocol family this venue implements.
    fn protocol(&self) -> PoolProtocol;

    /// A human-readable label (default: the protocol name).
    fn label(&self) -> String {
        self.protocol().into()
    }

    /// The minimal set of pubkeys required to update venue state.
    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError>;

    /// Update internal state from the provided account cache.
    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError>;

    /// Compute a quote. **Must** handle zero input without panicking or erroring.
    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError>;

    /// Build the instruction that executes a swap (use the `QuoteRequest`
    /// amounts, not the `QuoteResult`).
    fn generate_swap_instruction(&self, request: QuoteRequest, user: Pubkey) -> Result<Instruction, TradingVenueError>;

    /// Lower/upper admissible input bounds for a pair, via binary search over
    /// `quote()`.
    fn bounds(&self, tkn_in_ind: u8, tkn_out_ind: u8) -> Result<(u64, u64), TradingVenueError> {
        let input_mint = self.get_token(tkn_in_ind as usize)?.pubkey;
        let output_mint = self.get_token(tkn_out_ind as usize)?.pubkey;

        let f = |x: u64| self.quote(QuoteRequest { amount: x, swap_type: SwapType::ExactIn, input_mint, output_mint });

        find_boundaries(&f)
    }
}
