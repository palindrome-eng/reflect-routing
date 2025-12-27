use anyhow::{anyhow, Context, Error, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_account_decoder::{encode_ui_account, UiAccount, UiAccountEncoding};
use solana_sdk::clock::Clock;
use std::collections::HashSet;

use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::Arc;
use std::{collections::HashMap, convert::TryFrom, str::FromStr};
mod custom_serde;
mod swap;
mod constants;
pub use constants::*;
use custom_serde::field_as_string;
pub use swap::{AccountsType, RemainingAccountsInfo, RemainingAccountsSlice, Side, Swap};

/// An abstraction in order to share reserve mints and necessary data
use solana_sdk::{account::Account, instruction::AccountMeta, pubkey::Pubkey, sysvar};

#[derive(Serialize, Deserialize, PartialEq, Clone, Copy, Default, Debug)]
pub enum SwapMode {
    #[default]
    ExactIn,
    ExactOut,
}

impl FromStr for SwapMode {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "ExactIn" => Ok(SwapMode::ExactIn),
            "ExactOut" => Ok(SwapMode::ExactOut),
            _ => Err(anyhow!("{} is not a valid SwapMode", s)),
        }
    }
}

use std::convert::TryInto;

pub struct ReflectSwap {
    // User accounts (dynamic)
    pub user: Pubkey,
    pub user_receipt_ata: Pubkey,
    pub user_usdc_ata: Pubkey,
    
    // Protocol accounts (static from ReflectAmm)
    pub main: Pubkey,
    pub usdc_controller: Pubkey,
    pub admin_permissions: Pubkey,
    pub controller_usdc_ata: Pubkey,
    pub receipt_mint: Pubkey,
    
    // Drift accounts
    pub drift_program: Pubkey,
    pub drift_state: Pubkey,
    pub drift_user_stats: Pubkey,
    pub referrer_user_stats: Pubkey,
    pub referrer_user: Pubkey,
    pub drift_user_account: Pubkey,
    pub drift_spot_market_vault: Pubkey,
    pub drift_vault: Pubkey,
    
    // Remaining accounts
    pub usdc_oracle: Pubkey,
    pub drift_usdc_spot_market: Pubkey,
}

impl TryFrom<ReflectSwap> for Vec<AccountMeta> {
    type Error = anyhow::Error;

    fn try_from(swap: ReflectSwap) -> Result<Self, Self::Error> {
        Ok(vec![
            // #1 - user (signer)
            AccountMeta::new(swap.user, true),
            // #2 - main
            AccountMeta::new(swap.main, false),
            // #3 - usdc_controller
            AccountMeta::new(swap.usdc_controller, false),
            // #4 - admin_permissions (optional, but we pass it)
            AccountMeta::new_readonly(swap.admin_permissions, false),
            // #5 - user_receipt_ata
            AccountMeta::new(swap.user_receipt_ata, false),
            // #6 - user_usdc_ata
            AccountMeta::new(swap.user_usdc_ata, false),
            // #7 - controller_usdc_ata
            AccountMeta::new(swap.controller_usdc_ata, false),
            // #8 - receipt_mint
            AccountMeta::new(swap.receipt_mint, false),
            // #9 - drift program
            AccountMeta::new_readonly(swap.drift_program, false),
            // #10 - drift state
            AccountMeta::new(swap.drift_state, false),
            // #11 - user_stats
            AccountMeta::new(swap.drift_user_stats, false),
            // #12 - referrer_user_stats
            AccountMeta::new(swap.referrer_user_stats, false),
            // #13 - referrer_user
            AccountMeta::new(swap.referrer_user, false),
            // #14 - user_account (reflect drift user)
            AccountMeta::new(swap.drift_user_account, false),
            // #15 - spot_market_vault
            AccountMeta::new(swap.drift_spot_market_vault, false),
            // #16 - drift_vault
            AccountMeta::new(swap.drift_vault, false),
            // #17 - token_program
            AccountMeta::new_readonly(token_program::ID, false),
            // #18 - system_program
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            // #19 - clock
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            // #20 - usdc_oracle (remaining)
            AccountMeta::new(swap.usdc_oracle, false),
            // #21 - drift_usdc_spot_market (remaining)
            AccountMeta::new(swap.drift_usdc_spot_market, false),
        ])
    }
}

#[derive(Debug)]
pub struct QuoteParams {
    pub amount: u64,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub swap_mode: SwapMode,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Quote {
    pub in_amount: u64,
    pub out_amount: u64,
    pub fee_amount: u64,
    pub fee_mint: Pubkey,
    pub fee_pct: Decimal,
}

pub type QuoteMintToReferrer = HashMap<Pubkey, Pubkey, ahash::RandomState>;

pub struct SwapParams<'a, 'b> {
    pub swap_mode: SwapMode,
    pub in_amount: u64,
    pub out_amount: u64,
    pub source_mint: Pubkey,
    pub destination_mint: Pubkey,
    pub source_token_account: Pubkey,
    pub destination_token_account: Pubkey,
    /// This can be the user or the program authority over the source_token_account.
    pub token_transfer_authority: Pubkey,
    pub quote_mint_to_referrer: Option<&'a QuoteMintToReferrer>,
    pub jupiter_program_id: &'b Pubkey,
    /// Instead of returning the relevant Err, replace dynamic accounts with the default Pubkey
    /// This is useful for crawling market with no tick array
    pub missing_dynamic_accounts_as_default: bool,
}

impl SwapParams<'_, '_> {
    /// A placeholder to indicate an optional account or used as a terminator when consuming remaining accounts
    /// Using the jupiter program id
    pub fn placeholder_account_meta(&self) -> AccountMeta {
        AccountMeta::new_readonly(*self.jupiter_program_id, false)
    }
}

pub struct SwapAndAccountMetas {
    pub swap: Swap,
    pub account_metas: Vec<AccountMeta>,
}

pub type AccountMap = HashMap<Pubkey, Account, ahash::RandomState>;

pub fn try_get_account_data<'a>(account_map: &'a AccountMap, address: &Pubkey) -> Result<&'a [u8]> {
    account_map
        .get(address)
        .map(|account| account.data.as_slice())
        .with_context(|| format!("Could not find address: {address}"))
}

pub fn try_get_account_data_and_owner<'a>(
    account_map: &'a AccountMap,
    address: &Pubkey,
) -> Result<(&'a [u8], &'a Pubkey)> {
    let account = account_map
        .get(address)
        .with_context(|| format!("Could not find address: {address}"))?;
    Ok((account.data.as_slice(), &account.owner))
}

pub struct AmmContext {
    pub clock_ref: ClockRef,
}

#[derive(Clone, Debug, Default)]
pub struct ReflectAmm {
    pub label: String,
    pub program_id: Pubkey,
    
    // Core accounts
    pub main: Pubkey,
    pub usdc_plus_controller: Pubkey,
    pub admin_permissions: Pubkey,
    pub usdc_plus_mint: Pubkey,
    pub controller_usdc_ata: Pubkey,
    
    // Drift accounts
    pub drift_program: Pubkey,
    pub drift_state: Pubkey,
    pub drift_user_stats: Pubkey,
    pub usdc_plus_drift_user_acc: Pubkey,
    pub drift_spot_market_vault: Pubkey,
    pub drift_vault: Pubkey,
    pub referrer_user_stats: Pubkey,
    pub referrer_user: Pubkey,

    // Added in remaining
    pub drift_usdc_spot_market: Pubkey,
    pub usdc_oracle: Pubkey,

    // Rates
    pub protocol_tvl: u64,
    pub effective_supply: u64,
}

impl ReflectAmm {
    pub fn new() -> Self {
        ReflectAmm {            
            label: REFLECT_LABEL.to_owned(),
            program_id: reflect::ID,
            
            // Core accounts
            main: reflect_main::ID,
            usdc_plus_controller: usdc_controller::ID,
            admin_permissions: admin_permissions::ID,
            usdc_plus_mint: usdc_plus_mint::ID,
            controller_usdc_ata: controller_usdc_ata::ID,
            
            // Drift accounts
            drift_program: drift::ID,
            drift_state: drift_state::ID,
            drift_user_stats: drift_user_stats::ID,
            usdc_plus_drift_user_acc: reflect_user_account_strategy_0::ID,
            drift_spot_market_vault: drift_spot_market_vault::ID,
            drift_vault: drift_vault::ID,
            referrer_user_stats: referrer_user_stats::ID,
            referrer_user: referrer_user::ID,
            
            // Remaining accounts
            drift_usdc_spot_market: usdc_spot_market::ID,
            usdc_oracle: usdc_oracle::ID,
            
            // Rates
            protocol_tvl: 0,
            effective_supply: 0,
        }
    }
}

pub trait Amm {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self>
    where
        Self: Sized;
    /// A human readable label of the underlying DEX
    fn label(&self) -> String;
    fn program_id(&self) -> Pubkey;
    /// The pool state or market state address
    fn key(&self) -> Pubkey;
    /// The mints that can be traded
    fn get_reserve_mints(&self) -> Vec<Pubkey>;
    /// The accounts necessary to produce a quote
    fn get_accounts_to_update(&self) -> Vec<Pubkey>;
    /// Picks necessary accounts to update it's internal state
    /// Heavy deserialization and precomputation caching should be done in this function
    fn update(&mut self, account_map: &AccountMap) -> Result<()>;

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote>;

    /// Indicates which Swap has to be performed along with all the necessary account metas
    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas>;

    /// Indicates if get_accounts_to_update might return a non constant vec
    fn has_dynamic_accounts(&self) -> bool {
        false
    }

    /// Indicates whether `update` needs to be called before `get_reserve_mints`
    fn requires_update_for_reserve_mints(&self) -> bool {
        false
    }

    // Indicates that whether ExactOut mode is supported
    fn supports_exact_out(&self) -> bool {
        false
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync>;

    /// It can only trade in one direction from its first mint to second mint, assuming it is a two mint AMM
    fn unidirectional(&self) -> bool {
        false
    }

    /// For testing purposes, provide a mapping of dependency programs to function
    fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
        vec![]
    }

    fn get_accounts_len(&self) -> usize {
        32 // Default to a near whole legacy transaction to penalize no implementation
    }

    /// The identifier of the underlying liquidity
    ///
    /// Example:
    /// For RaydiumAmm uses Openbook market A this will return Some(A)
    /// For Openbook market A, it will also return Some(A)
    fn underlying_liquidities(&self) -> Option<HashSet<Pubkey>> {
        None
    }

    /// Provides a shortcut to establish if the AMM can be used for trading
    /// If the market is active at all
    fn is_active(&self) -> bool {

        // This could be the check for the bool which blocks mint,
        true
    }
}

impl Amm for ReflectAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, _amm_context: &AmmContext) -> Result<Self> {       

        Ok(ReflectAmm::new())
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    /// ID of reflect delta neutral.
    fn program_id(&self) -> Pubkey {
        self.program_id
    }
    
    // This signs to drift and holds tokens.
    fn key(&self) -> Pubkey {
        self.usdc_plus_controller
    }

    /// Mints between which you can exhcange.
    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![usdc_mint::ID, usdc_plus_mint::ID]
    }

    /// Accounts needed to generate a quote.
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        // Whatver the exchnage library needs for exchange with drift only.
        vec![self.usdc_plus_controller, self.usdc_plus_drift_user_acc, self.usdc_plus_mint, self.drift_usdc_spot_market]      
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {


        // Get 
        let usdc_plus_mint = try_get_account_data(account_map, &self.usdc_plus_mint)?;
        let usdc_plus_drift_user_acc = try_get_account_data(account_map, &self.usdc_plus_drift_user_acc)?;
        let drift_usdc_spot_market = try_get_account_data(account_map, &self.drift_usdc_spot_market)?;
        let usdc_plus_controller = try_get_account_data(account_map, &self.usdc_plus_controller)?;        

        let (protocol_tvl, supply) = usdc_plus_exchange::get_exchange_components(
            usdc_plus_controller,
            drift_usdc_spot_market,
            usdc_plus_drift_user_acc,
            usdc_plus_mint,
        )?;

        self.protocol_tvl = protocol_tvl;
        self.effective_supply = supply;
            

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let (in_amount, out_amount) = match quote_params.swap_mode {
            SwapMode::ExactIn => {
                let out = if quote_params.input_mint == usdc_mint::ID {
                    usdc_plus_exchange::compute_tokens_from_usdc(
                        quote_params.amount,
                        self.protocol_tvl,
                        self.effective_supply,
                    )?
                } else {
                    usdc_plus_exchange::compute_usdc_from_tokens(
                        quote_params.amount,
                        self.protocol_tvl,
                        self.effective_supply,
                    )?
                };
                (quote_params.amount, out)
            }
            SwapMode::ExactOut => {
                let inp = if quote_params.input_mint == usdc_mint::ID {                    
                    // Inverse: in = out * effective_tvl / supply.
                    usdc_plus_exchange::compute_usdc_from_tokens(
                        quote_params.amount,
                        self.protocol_tvl,
                        self.effective_supply,
                    )?
                } else {                    
                    // Inverse: in = out * supply / effective_tvl.
                    usdc_plus_exchange::compute_tokens_from_usdc(
                        quote_params.amount,
                        self.protocol_tvl,
                        self.effective_supply,
                    )?
                };
                (inp, quote_params.amount)
            }
        };

        Ok(Quote {
            in_amount,
            out_amount,
            fee_amount: 0,
            fee_mint: quote_params.input_mint,
            ..Quote::default()
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let SwapParams {
            source_mint,
            destination_mint,
            source_token_account,
            destination_token_account,
            token_transfer_authority,
            ..
        } = swap_params;

        // Validate mint pair
        let valid_pair = (*source_mint == usdc_mint::ID && *destination_mint == usdc_plus_mint::ID)
            || (*source_mint == usdc_plus_mint::ID && *destination_mint == usdc_mint::ID);
        
        if !valid_pair {
            return Err(anyhow!(
                "Invalid mint pair: source {} destination {}",
                source_mint,
                destination_mint
            ));
        }

        let is_deposit = *source_mint == usdc_mint::ID;

        let (user_usdc_ata, user_receipt_ata) = if is_deposit {
            (*source_token_account, *destination_token_account)
        } else {
            (*destination_token_account, *source_token_account)
        };

        Ok(SwapAndAccountMetas {
            swap: Swap::ReflectS1,
            account_metas: ReflectSwap {
                user: *token_transfer_authority,
                user_receipt_ata,
                user_usdc_ata,
                main: self.main,
                usdc_controller: self.usdc_plus_controller,
                admin_permissions: self.admin_permissions,
                controller_usdc_ata: self.controller_usdc_ata,
                receipt_mint: self.usdc_plus_mint,
                drift_program: self.drift_program,
                drift_state: self.drift_state,
                drift_user_stats: self.drift_user_stats,
                referrer_user_stats: self.referrer_user_stats,
                referrer_user: self.referrer_user,
                drift_user_account: self.usdc_plus_drift_user_acc,
                drift_spot_market_vault: self.drift_spot_market_vault,
                drift_vault: self.drift_vault,
                usdc_oracle: self.usdc_oracle,
                drift_usdc_spot_market: self.drift_usdc_spot_market,
            }
            .try_into()?,
        })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }

    fn supports_exact_out(&self) -> bool {
        true
    }
}


impl Clone for Box<dyn Amm + Send + Sync> {
    fn clone(&self) -> Box<dyn Amm + Send + Sync> {
        self.clone_amm()
    }
}

pub type AmmLabel = &'static str;

pub trait AmmProgramIdToLabel {
    const PROGRAM_ID_TO_LABELS: &[(Pubkey, AmmLabel)];
}

pub trait SingleProgramAmm {
    const PROGRAM_ID: Pubkey;
    const LABEL: AmmLabel;
}

impl<T: SingleProgramAmm> AmmProgramIdToLabel for T {
    const PROGRAM_ID_TO_LABELS: &[(Pubkey, AmmLabel)] = &[(Self::PROGRAM_ID, Self::LABEL)];
}

#[macro_export]
macro_rules! single_program_amm {
    ($amm_struct:ty, $program_id:expr, $label:expr) => {
        impl SingleProgramAmm for $amm_struct {
            const PROGRAM_ID: Pubkey = $program_id;
            const LABEL: &'static str = $label;
        }
    };
}

#[derive(Clone, Deserialize, Serialize)]
pub struct KeyedAccount {
    pub key: Pubkey,
    pub account: Account,
    pub params: Option<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Market {
    #[serde(with = "field_as_string")]
    pub pubkey: Pubkey,
    #[serde(with = "field_as_string")]
    pub owner: Pubkey,
    /// Additional data an Amm requires, Amm dependent and decoded in the Amm implementation
    pub params: Option<Value>,
}

impl From<KeyedAccount> for Market {
    fn from(
        KeyedAccount {
            key,
            account,
            params,
        }: KeyedAccount,
    ) -> Self {
        Market {
            pubkey: key,
            owner: account.owner,
            params,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct KeyedUiAccount {
    pub pubkey: String,
    #[serde(flatten)]
    pub ui_account: UiAccount,
    /// Additional data an Amm requires, Amm dependent and decoded in the Amm implementation
    pub params: Option<Value>,
}

impl From<KeyedAccount> for KeyedUiAccount {
    fn from(keyed_account: KeyedAccount) -> Self {
        let KeyedAccount {
            key,
            account,
            params,
        } = keyed_account;

        let ui_account = encode_ui_account(&key, &account, UiAccountEncoding::Base64, None, None);

        KeyedUiAccount {
            pubkey: key.to_string(),
            ui_account,
            params,
        }
    }
}

impl TryFrom<KeyedUiAccount> for KeyedAccount {
    type Error = Error;

    fn try_from(keyed_ui_account: KeyedUiAccount) -> Result<Self, Self::Error> {
        let KeyedUiAccount {
            pubkey,
            ui_account,
            params,
        } = keyed_ui_account;
        let account = ui_account
            .decode()
            .unwrap_or_else(|| panic!("Failed to decode ui_account for {pubkey}"));

        Ok(KeyedAccount {
            key: Pubkey::from_str(&pubkey)?,
            account,
            params,
        })
    }
}

#[derive(Default, Clone)]
pub struct ClockRef {
    pub slot: Arc<AtomicU64>,
    /// The timestamp of the first `Slot` in this `Epoch`.
    pub epoch_start_timestamp: Arc<AtomicI64>,
    /// The current `Epoch`.
    pub epoch: Arc<AtomicU64>,
    pub leader_schedule_epoch: Arc<AtomicU64>,
    pub unix_timestamp: Arc<AtomicI64>,
}

impl ClockRef {
    pub fn update(&self, clock: Clock) {
        self.epoch
            .store(clock.epoch, std::sync::atomic::Ordering::Relaxed);
        self.slot
            .store(clock.slot, std::sync::atomic::Ordering::Relaxed);
        self.unix_timestamp
            .store(clock.unix_timestamp, std::sync::atomic::Ordering::Relaxed);
        self.epoch_start_timestamp.store(
            clock.epoch_start_timestamp,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.leader_schedule_epoch.store(
            clock.leader_schedule_epoch,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

impl From<Clock> for ClockRef {
    fn from(clock: Clock) -> Self {
        ClockRef {
            epoch: Arc::new(AtomicU64::new(clock.epoch)),
            epoch_start_timestamp: Arc::new(AtomicI64::new(clock.epoch_start_timestamp)),
            leader_schedule_epoch: Arc::new(AtomicU64::new(clock.leader_schedule_epoch)),
            slot: Arc::new(AtomicU64::new(clock.slot)),
            unix_timestamp: Arc::new(AtomicI64::new(clock.unix_timestamp)),
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::rpc_client::RpcClient;
    use solana_sdk::pubkey::Pubkey;
    use std::collections::HashMap;

    const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

    fn create_account_map(rpc: &RpcClient, pubkeys: &[Pubkey]) -> AccountMap {
        let accounts = rpc.get_multiple_accounts(pubkeys).unwrap();
        let mut map = AccountMap::default();
        for (i, account) in accounts.into_iter().enumerate() {
            if let Some(acc) = account {
                map.insert(pubkeys[i], acc);
            }
        }
        map
    }

    #[test]
    fn test_reflect_amm_accounts_to_update() {
        let amm = ReflectAmm::new();
        let accounts = amm.get_accounts_to_update();
        
        assert_eq!(accounts.len(), 4);
        assert!(accounts.contains(&amm.usdc_plus_controller));
        assert!(accounts.contains(&amm.usdc_plus_drift_user_acc));
        assert!(accounts.contains(&amm.usdc_plus_mint));
        assert!(accounts.contains(&amm.drift_usdc_spot_market));
    }

    #[test]
    fn test_reflect_amm_update_and_quote() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        // Fetch accounts needed for the update.
        let accounts_to_update = amm.get_accounts_to_update();
        let account_map = create_account_map(&rpc, &accounts_to_update);
        
        // Update AMM state.
        amm.update(&account_map).unwrap();
        
        // Verify variables were set.
        assert!(amm.protocol_tvl > 0, "USDC rate should be set");
        assert!(amm.effective_supply > 0, "USDC+ rate should be set");
    }

    #[test]
    fn test_reflect_amm_quote_usdc_to_usdc_plus() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        let accounts_to_update = amm.get_accounts_to_update();
        let account_map = create_account_map(&rpc, &accounts_to_update);
        amm.update(&account_map).unwrap();
        
        // Quote for 100 USDC (6 decimals)
        let in_amount: u64 = 100_000_000;
        let quote = amm.quote(&QuoteParams {
            amount: in_amount,
            input_mint: usdc_mint::ID,
            output_mint: usdc_plus_mint::ID,
            swap_mode: SwapMode::ExactIn,
        }).unwrap();
        
        println!(
            "Quote: {} USDC -> {} USDC+",
            in_amount as f64 / 1_000_000.0,
            quote.out_amount as f64 / 1_000_000.0
        );
        
        assert!(quote.out_amount > 0, "Output amount should be > 0");
        assert_eq!(quote.in_amount, in_amount);
        assert_eq!(quote.fee_amount, 0);
    }

    #[test]
    fn test_reflect_amm_quote_usdc_plus_to_usdc() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        let accounts_to_update = amm.get_accounts_to_update();
        let account_map = create_account_map(&rpc, &accounts_to_update);
        amm.update(&account_map).unwrap();
        
        // Quote for 100 USDC+ (6 decimals).
        let in_amount: u64 = 100_000_000;
        let quote = amm.quote(&QuoteParams {
            amount: in_amount,
            input_mint: usdc_plus_mint::ID,
            output_mint: usdc_mint::ID,
            swap_mode: SwapMode::ExactIn,
        }).unwrap();
        
        println!(
            "Quote: {} USDC+ -> {} USDC",
            in_amount as f64 / 1_000_000.0,
            quote.out_amount as f64 / 1_000_000.0
        );
        
        assert!(quote.out_amount > 0, "Output amount should be > 0");
        assert_eq!(quote.in_amount, in_amount);
        assert_eq!(quote.fee_amount, 0);
    }

    #[test]
    fn test_reflect_amm_quote_roundtrip() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        let accounts_to_update = amm.get_accounts_to_update();
        let account_map = create_account_map(&rpc, &accounts_to_update);
        amm.update(&account_map).unwrap();
        
        // Start with 1000 USDC.
        let initial_usdc: u64 = 1_000_000_000;
        
        // USDC -> USDC+.
        let quote1 = amm.quote(&QuoteParams {
            amount: initial_usdc,
            input_mint: usdc_mint::ID,
            output_mint: usdc_plus_mint::ID,
            swap_mode: SwapMode::ExactIn,
        }).unwrap();
        
        // USDC+ -> USDC.
        let quote2 = amm.quote(&QuoteParams {
            amount: quote1.out_amount,
            input_mint: usdc_plus_mint::ID,
            output_mint: usdc_mint::ID,
            swap_mode: SwapMode::ExactIn,
        }).unwrap();
        
        println!(
            "Roundtrip: {} USDC -> {} USDC+ -> {} USDC",
            initial_usdc as f64 / 1_000_000.0,
            quote1.out_amount as f64 / 1_000_000.0,
            quote2.out_amount as f64 / 1_000_000.0
        );
        
        // Should get back approximately the same amount (within some tolerance due to rounding).
        let diff = (initial_usdc as i64 - quote2.out_amount as i64).abs();
        let tolerance = initial_usdc / 10000; // 0.01% tolerance
        assert!(
            diff <= tolerance as i64,
            "Roundtrip should be approximately equal: {} vs {} (diff: {})",
            initial_usdc,
            quote2.out_amount,
            diff
        );
    }

    #[test]
    fn test_reflect_amm_quote_invalid_mint() {
        let amm = ReflectAmm::new();
        
        let invalid_mint = Pubkey::new_unique();
        let result = amm.quote(&QuoteParams {
            amount: 100_000_000,
            input_mint: invalid_mint,
            output_mint: usdc_plus_mint::ID,
            swap_mode: SwapMode::ExactIn,
        });
        
        assert!(result.is_err(), "Should fail with invalid input mint");
    }

    #[test]
    fn test_reflect_amm_swap_and_account_metas_deposit() {
        let amm = ReflectAmm::new();
        
        let user = Pubkey::new_unique();
        let user_usdc_ata = Pubkey::new_unique();
        let user_usdc_plus_ata = Pubkey::new_unique();
        let jupiter_program = Pubkey::new_unique();
        
        let swap_params = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 100_000_000,
            out_amount: 99_000_000,
            source_mint: usdc_mint::ID,
            destination_mint: usdc_plus_mint::ID,
            source_token_account: user_usdc_ata,
            destination_token_account: user_usdc_plus_ata,
            token_transfer_authority: user,
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            missing_dynamic_accounts_as_default: false,
        };
        
        let result = amm.get_swap_and_account_metas(&swap_params).unwrap();
        
        assert!(!result.account_metas.is_empty());
        // First account should be the user (signer)
        assert_eq!(result.account_metas[0].pubkey, user);
        assert!(result.account_metas[0].is_signer);
    }

    #[test]
    fn test_reflect_amm_swap_and_account_metas_withdraw() {
        let amm = ReflectAmm::new();
        
        let user = Pubkey::new_unique();
        let user_usdc_ata = Pubkey::new_unique();
        let user_usdc_plus_ata = Pubkey::new_unique();
        let jupiter_program = Pubkey::new_unique();
        
        let swap_params = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 100_000_000,
            out_amount: 101_000_000,
            source_mint: usdc_plus_mint::ID,
            destination_mint: usdc_mint::ID,
            source_token_account: user_usdc_plus_ata,
            destination_token_account: user_usdc_ata,
            token_transfer_authority: user,
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            missing_dynamic_accounts_as_default: false,
        };
        
        let result = amm.get_swap_and_account_metas(&swap_params).unwrap();
        
        assert!(!result.account_metas.is_empty());
        assert_eq!(result.account_metas[0].pubkey, user);
        assert!(result.account_metas[0].is_signer);
    }

    #[test]
    fn test_reflect_amm_swap_invalid_mint_pair() {
        let amm = ReflectAmm::new();
        
        let user = Pubkey::new_unique();
        let invalid_mint = Pubkey::new_unique();
        let jupiter_program = Pubkey::new_unique();
        
        let swap_params = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 100_000_000,
            out_amount: 99_000_000,
            source_mint: invalid_mint,
            destination_mint: usdc_plus_mint::ID,
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: user,
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            missing_dynamic_accounts_as_default: false,
        };
        
        let result = amm.get_swap_and_account_metas(&swap_params);
        assert!(result.is_err(), "Should fail with invalid mint pair");
    }

    #[test]
    fn test_reflect_amm_swap_same_mint() {
        let amm = ReflectAmm::new();
        
        let user = Pubkey::new_unique();
        let jupiter_program = Pubkey::new_unique();
        
        let swap_params = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 100_000_000,
            out_amount: 100_000_000,
            source_mint: usdc_mint::ID,
            destination_mint: usdc_mint::ID, // Same mint!
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: user,
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter_program,
            missing_dynamic_accounts_as_default: false,
        };
        
        let result = amm.get_swap_and_account_metas(&swap_params);
        assert!(result.is_err(), "Should fail when source and destination mint are the same");
    }

    #[test]
    fn test_reflect_amm_clone() {
        let amm = ReflectAmm::new();
        let cloned = amm.clone_amm();
        
        assert_eq!(cloned.label(), amm.label());
        assert_eq!(cloned.program_id(), amm.program_id());
        assert_eq!(cloned.key(), amm.key());
    }

    #[test]
    fn test_reflect_amm_trait_methods() {
        let amm = ReflectAmm::new();
        
        assert_eq!(amm.label(), REFLECT_LABEL);
        assert_eq!(amm.program_id(), reflect::ID);
        assert_eq!(amm.key(), usdc_controller::ID);
        assert!(!amm.has_dynamic_accounts());
        assert!(!amm.requires_update_for_reserve_mints());
        assert!(amm.supports_exact_out());
        assert!(!amm.unidirectional());
        assert!(amm.is_active());
    }
}