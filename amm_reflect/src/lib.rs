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
use solana_sdk::{account::Account, instruction::AccountMeta, pubkey::Pubkey};

// Import reflect-exchange-rates functions
use reflect_exchange_rates::accounts_api::{
    get_tvl_accounts_usdc, get_tvl_accounts_usdt,
    get_supply_change_accounts_usdc, get_supply_change_accounts_usdt,
};
use reflect_exchange_rates::usdc_plus_exchange::{
    exchange_amount_usdc_data, exchange_amount_usdc_plus_data,
    get_exchange_components_usdc_data,
};
use reflect_exchange_rates::usdt_plus_exchange::{
    exchange_amount_usdt_data, exchange_amount_usdt_plus_data,
    get_exchange_components_usdt_data,
};

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


/// Route type for Reflect AMM (USDC or USDT)
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReflectRoute {
    Usdc,
    Usdt,
}

impl ReflectRoute {
    pub fn from_mints(input_mint: &Pubkey, output_mint: &Pubkey) -> Result<Self> {
        let usdc_mint = usdc_mint::ID;
        let usdc_plus_mint = usdc_plus_mint::ID;
        let usdt_mint = usdt_mint::ID;
        let usdt_plus_mint = reflect_exchange_rates::ids::usdt_plus_mint::ID;
        
        if (*input_mint == usdc_mint && *output_mint == usdc_plus_mint)
            || (*input_mint == usdc_plus_mint && *output_mint == usdc_mint)
        {
            Ok(ReflectRoute::Usdc)
        } else if (*input_mint == usdt_mint && *output_mint == usdt_plus_mint)
            || (*input_mint == usdt_plus_mint && *output_mint == usdt_mint)
        {
            Ok(ReflectRoute::Usdt)
        } else {
            Err(anyhow!(
                "Invalid mint pair: {} -> {}. Must be USDC/USDC+ or USDT/USDT+",
                input_mint,
                output_mint
            ))
        }
    }
    
    pub fn base_mint(&self) -> Pubkey {
        match self {
            ReflectRoute::Usdc => usdc_mint::ID,
            ReflectRoute::Usdt => usdt_mint::ID,
        }
    }
    
    pub fn receipt_mint(&self) -> Pubkey {
        match self {
            ReflectRoute::Usdc => usdc_plus_mint::ID,
            ReflectRoute::Usdt => reflect_exchange_rates::ids::usdt_plus_mint::ID,
        }
    }
    
    pub fn controller(&self) -> Pubkey {
        match self {
            ReflectRoute::Usdc => usdc_controller::ID,
            ReflectRoute::Usdt => reflect_exchange_rates::ids::usdt_controller::ID,
        }
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
    pub admin_permissions: Pubkey,
    
    // Route-specific state
    pub route: Option<ReflectRoute>,
    
    // Cached controller data for TVL/exchange calculations
    pub controller_data: Option<Vec<u8>>,
    pub receipt_mint_data: Option<Vec<u8>>,
    
    // Protocol account data (dynamic, fetched from controller)
    pub protocol_account_data: Vec<Vec<u8>>,
    
    // Rates (cached)
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
            admin_permissions: admin_permissions::ID,
            
            route: None,
            controller_data: None,
            receipt_mint_data: None,
            protocol_account_data: Vec::new(),
            
            // Rates
            protocol_tvl: 0,
            effective_supply: 0,
        }
    }
    
    /// Determine route from mints and set it
    pub fn set_route_from_mints(&mut self, input_mint: &Pubkey, output_mint: &Pubkey) -> Result<()> {
        self.route = Some(ReflectRoute::from_mints(input_mint, output_mint)?);
        Ok(())
    }
    
    /// Get the current route, or determine it from reserve mints
    pub fn get_route(&self) -> Option<ReflectRoute> {
        self.route
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
    fn from_keyed_account(_keyed_account: &KeyedAccount, _amm_context: &AmmContext) -> Result<Self> {       
        Ok(ReflectAmm::new())
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    /// ID of reflect delta neutral.
    fn program_id(&self) -> Pubkey {
        self.program_id
    }
    
    /// Returns the controller key for the current route
    fn key(&self) -> Pubkey {
        self.route
            .map(|r| r.controller())
            .unwrap_or(usdc_controller::ID) // Default to USDC controller
    }

    /// Mints between which you can exchange (supports both USDC and USDT routes)
    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![
            usdc_mint::ID,
            usdc_plus_mint::ID,
            usdt_mint::ID,
            reflect_exchange_rates::ids::usdt_plus_mint::ID,
        ]
    }

    /// Accounts needed to generate a quote.
    /// Uses get_tvl_accounts functions from reflect-exchange-rates to get dynamic protocol accounts
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        // If we have cached controller data, use it to get TVL accounts
        if let Some(ref controller_data) = self.controller_data {
            let route = self.route.unwrap_or(ReflectRoute::Usdc);
            match route {
                ReflectRoute::Usdc => {
                    get_tvl_accounts_usdc(controller_data)
                        .map_err(|e| anyhow!("Failed to get USDC TVL accounts: {:?}", e))
                        .unwrap_or_else(|_| vec![usdc_controller::ID, usdc_plus_mint::ID])
                }
                ReflectRoute::Usdt => {
                    get_tvl_accounts_usdt(controller_data)
                        .map_err(|e| anyhow!("Failed to get USDT TVL accounts: {:?}", e))
                        .unwrap_or_else(|_| vec![reflect_exchange_rates::ids::usdt_controller::ID, reflect_exchange_rates::ids::usdt_plus_mint::ID])
                }
            }
        } else {
            // Fallback: return base accounts (controller + receipt mint)
            vec![usdc_controller::ID, usdc_plus_mint::ID]
        }
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        // Determine route if not set - try USDC first
        let route = self.route.unwrap_or_else(|| {
            // Try to determine from available accounts
            if account_map.contains_key(&usdc_controller::ID) {
                ReflectRoute::Usdc
            } else if account_map.contains_key(&reflect_exchange_rates::ids::usdt_controller::ID) {
                ReflectRoute::Usdt
            } else {
                ReflectRoute::Usdc // Default
            }
        });
        self.route = Some(route);

        let controller_key = route.controller();
        let receipt_mint_key = route.receipt_mint();

        // Get controller and receipt mint data
        let controller_data = try_get_account_data(account_map, &controller_key)?.to_vec();
        let receipt_mint_data = try_get_account_data(account_map, &receipt_mint_key)?.to_vec();

        // Get TVL accounts using reflect-exchange-rates
        let tvl_accounts = match route {
            ReflectRoute::Usdc => get_tvl_accounts_usdc(&controller_data)
                .map_err(|e| anyhow!("Failed to get USDC TVL accounts: {:?}", e))?,
            ReflectRoute::Usdt => get_tvl_accounts_usdt(&controller_data)
                .map_err(|e| anyhow!("Failed to get USDT TVL accounts: {:?}", e))?,
        };

        // Fetch protocol account data (skip first 2: controller and receipt_mint)
        let mut protocol_data = Vec::new();
        for account_key in tvl_accounts.iter().skip(2) {
            if let Ok(data) = try_get_account_data(account_map, account_key) {
                protocol_data.push(data.to_vec());
            }
        }

        // Convert protocol_data to slice references for exchange calculation
        let protocol_slices: Vec<&[u8]> = protocol_data.iter().map(|v| v.as_slice()).collect();

        // Calculate exchange components using reflect-exchange-rates
        let (deposited_vault, receipt_token_supply) = match route {
            ReflectRoute::Usdc => {
                get_exchange_components_usdc_data(
                    &controller_data,
                    &receipt_mint_data,
                    &protocol_slices,
                )
                .map_err(|e| anyhow!("Failed to get USDC exchange components: {:?}", e))?
            }
            ReflectRoute::Usdt => {
                get_exchange_components_usdt_data(
                    &controller_data,
                    &receipt_mint_data,
                    &protocol_slices,
                )
                .map_err(|e| anyhow!("Failed to get USDT exchange components: {:?}", e))?
            }
        };

        // Cache the data
        self.controller_data = Some(controller_data);
        self.receipt_mint_data = Some(receipt_mint_data);
        self.protocol_account_data = protocol_data;
        self.protocol_tvl = deposited_vault;
        self.effective_supply = receipt_token_supply;

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        // Determine route from mints
        let route = ReflectRoute::from_mints(&quote_params.input_mint, &quote_params.output_mint)?;

        // Ensure we have cached data
        let controller_data = self.controller_data.as_ref()
            .ok_or_else(|| anyhow!("Controller data not loaded. Call update() first."))?;
        let receipt_mint_data = self.receipt_mint_data.as_ref()
            .ok_or_else(|| anyhow!("Receipt mint data not loaded. Call update() first."))?;
        
        let protocol_slices: Vec<&[u8]> = self.protocol_account_data.iter().map(|v| v.as_slice()).collect();

        let (in_amount, out_amount) = match quote_params.swap_mode {
            SwapMode::ExactIn => {
                let out = match route {
                    ReflectRoute::Usdc => {
                        if quote_params.input_mint == usdc_mint::ID {
                            exchange_amount_usdc_data(
                                controller_data,
                                receipt_mint_data,
                                &protocol_slices,
                                quote_params.amount,
                            )
                            .map_err(|e| anyhow!("Failed to exchange USDC: {:?}", e))?
                        } else {
                            exchange_amount_usdc_plus_data(
                                controller_data,
                                receipt_mint_data,
                                &protocol_slices,
                                quote_params.amount,
                            )
                            .map_err(|e| anyhow!("Failed to exchange USDC+: {:?}", e))?
                        }
                    }
                    ReflectRoute::Usdt => {
                        if quote_params.input_mint == usdt_mint::ID {
                            exchange_amount_usdt_data(
                                controller_data,
                                receipt_mint_data,
                                &protocol_slices,
                                quote_params.amount,
                            )
                            .map_err(|e| anyhow!("Failed to exchange USDT: {:?}", e))?
                        } else {
                            exchange_amount_usdt_plus_data(
                                controller_data,
                                receipt_mint_data,
                                &protocol_slices,
                                quote_params.amount,
                            )
                            .map_err(|e| anyhow!("Failed to exchange USDT+: {:?}", e))?
                        }
                    }
                };
                (quote_params.amount, out)
            }
            SwapMode::ExactOut => {
                // For ExactOut, we need to invert the calculation
                // This is an approximation - for exact calculations, you'd need iterative solving
                let inp = match route {
                    ReflectRoute::Usdc => {
                        if quote_params.input_mint == usdc_mint::ID {
                            // Approximate: if we want X USDC+ out, estimate USDC in needed
                            // This is a simplified calculation - real implementation might need iteration
                            exchange_amount_usdc_plus_data(
                                controller_data,
                                receipt_mint_data,
                                &protocol_slices,
                                quote_params.amount,
                            )
                            .map_err(|e| anyhow!("Failed to exchange USDC+ (ExactOut): {:?}", e))?
                        } else {
                            exchange_amount_usdc_data(
                                controller_data,
                                receipt_mint_data,
                                &protocol_slices,
                                quote_params.amount,
                            )
                            .map_err(|e| anyhow!("Failed to exchange USDC (ExactOut): {:?}", e))?
                        }
                    }
                    ReflectRoute::Usdt => {
                        if quote_params.input_mint == usdt_mint::ID {
                            exchange_amount_usdt_plus_data(
                                controller_data,
                                receipt_mint_data,
                                &protocol_slices,
                                quote_params.amount,
                            )
                            .map_err(|e| anyhow!("Failed to exchange USDT+ (ExactOut): {:?}", e))?
                        } else {
                            exchange_amount_usdt_data(
                                controller_data,
                                receipt_mint_data,
                                &protocol_slices,
                                quote_params.amount,
                            )
                            .map_err(|e| anyhow!("Failed to exchange USDT (ExactOut): {:?}", e))?
                        }
                    }
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

        // Determine route from mints
        let route = ReflectRoute::from_mints(source_mint, destination_mint)?;

        // Ensure we have controller data
        let controller_data = self.controller_data.as_ref()
            .ok_or_else(|| anyhow!("Controller data not loaded. Call update() first."))?;

        // Get supply change accounts using reflect-exchange-rates
        let account_pubkeys = match route {
            ReflectRoute::Usdc => {
                get_supply_change_accounts_usdc(
                    token_transfer_authority,
                    controller_data,
                    Some(self.admin_permissions),
                )
                .map_err(|e| anyhow!("Failed to get USDC supply change accounts: {:?}", e))?
            }
            ReflectRoute::Usdt => {
                get_supply_change_accounts_usdt(
                    token_transfer_authority,
                    controller_data,
                    Some(self.admin_permissions),
                )
                .map_err(|e| anyhow!("Failed to get USDT supply change accounts: {:?}", e))?
            }
        };

        // Determine which token accounts to use based on swap direction
        let is_deposit = match route {
            ReflectRoute::Usdc => *source_mint == usdc_mint::ID,
            ReflectRoute::Usdt => *source_mint == usdt_mint::ID,
        };

        // The account order from get_supply_change_accounts is:
        // [user, admin_permissions, main, controller, receipt_mint, user_receipt_ata, 
        //  user_earn_ata, controller_earn_ata, token_program, associated_token_program,
        //  sysvar_instructions, system_program, ...protocol_accounts]
        
        // We need to replace user_receipt_ata and user_earn_ata with the actual accounts from swap_params
        let mut account_metas = Vec::new();
        
        for (idx, pubkey) in account_pubkeys.iter().enumerate() {
            let is_signer = idx == 0; // user is signer
            let is_writable = match idx {
                0 => true,  // user
                2 => true,  // main
                3 => true,  // controller
                4 => true,  // receipt_mint
                5 => true,  // user_receipt_ata (will be replaced)
                6 => true,  // user_earn_ata (will be replaced)
                7 => true,  // controller_earn_ata
                _ => false, // others are readonly
            };

            // Replace user_receipt_ata (index 5) and user_earn_ata (index 6) with actual accounts
            let final_pubkey = match idx {
                5 => {
                    // user_receipt_ata
                    if is_deposit {
                        *destination_token_account
                    } else {
                        *source_token_account
                    }
                }
                6 => {
                    // user_earn_ata
                    if is_deposit {
                        *source_token_account
                    } else {
                        *destination_token_account
                    }
                }
                _ => *pubkey,
            };

            account_metas.push(AccountMeta {
                pubkey: final_pubkey,
                is_signer,
                is_writable,
            });
        }

        Ok(SwapAndAccountMetas {
            swap: Swap::ReflectS1,
            account_metas,
        })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }

    fn supports_exact_out(&self) -> bool {
        true
    }

    fn get_accounts_len(&self) -> usize {
        // Return actual account count if we have cached account data
        // Otherwise return a reasonable estimate (base accounts + some protocol accounts)
        if let Some(ref controller_data) = self.controller_data {
            let route = self.route.unwrap_or(ReflectRoute::Usdc);
            match route {
                ReflectRoute::Usdc => {
                    get_tvl_accounts_usdc(controller_data)
                        .map(|accounts| accounts.len())
                        .unwrap_or(12) // Base accounts for supply change
                }
                ReflectRoute::Usdt => {
                    get_tvl_accounts_usdt(controller_data)
                        .map(|accounts| accounts.len())
                        .unwrap_or(12) // Base accounts for supply change
                }
            }
        } else {
            // Default estimate: base accounts (12) + some protocol accounts
            20
        }
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

    const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

    fn create_account_map(rpc: &RpcClient, pubkeys: &[Pubkey]) -> AccountMap {
        let accounts = rpc.get_multiple_accounts(pubkeys)
            .expect("Failed to fetch accounts from RPC");
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
        let mut amm = ReflectAmm::new();
        // Set route to USDC for testing
        amm.set_route_from_mints(&usdc_mint::ID, &usdc_plus_mint::ID).unwrap();
        
        // Without controller data, should return fallback accounts
        let accounts = amm.get_accounts_to_update();
        assert!(accounts.len() >= 2);
        assert!(accounts.contains(&usdc_controller::ID));
        assert!(accounts.contains(&usdc_plus_mint::ID));
    }

    /// Integration test: Requires deployed accounts on mainnet
    /// This test will fail until accounts are deployed
    #[test]
    #[ignore = "Requires deployed accounts on mainnet"]
    fn test_reflect_amm_update_and_quote() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        // Set route to USDC
        amm.set_route_from_mints(&usdc_mint::ID, &usdc_plus_mint::ID).unwrap();
        
        // Fetch accounts needed for the update (start with base accounts)
        let base_accounts = vec![usdc_controller::ID, usdc_plus_mint::ID];
        let mut account_map = create_account_map(&rpc, &base_accounts);
        
        // Get controller account - this will fail if not deployed
        let controller_account = account_map.get(&usdc_controller::ID)
            .expect("USDC controller account not found - account may not be deployed");
        
        // Get TVL accounts using controller data
        let controller_data = controller_account.data.as_slice();
        let tvl_accounts = get_tvl_accounts_usdc(controller_data)
            .expect("Failed to get TVL accounts from controller data");
        
        // Fetch all protocol accounts
        let tvl_map = create_account_map(&rpc, &tvl_accounts);
        account_map.extend(tvl_map);
        
        // Update AMM state
        amm.update(&account_map)
            .expect("Failed to update AMM - check account data format");
        
        // Verify variables were set
        assert!(amm.protocol_tvl > 0, "Protocol TVL should be set");
        assert!(amm.effective_supply > 0, "Effective supply should be set");
    }

    /// Integration test: Requires deployed accounts on mainnet
    /// Tests USDC -> USDC+ quote calculation
    #[test]
    #[ignore = "Requires deployed accounts on mainnet"]
    fn test_reflect_amm_quote_usdc_to_usdc_plus() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        amm.set_route_from_mints(&usdc_mint::ID, &usdc_plus_mint::ID).unwrap();
        
        // Fetch base accounts
        let base_accounts = vec![usdc_controller::ID, usdc_plus_mint::ID];
        let mut account_map = create_account_map(&rpc, &base_accounts);
        
        // Get TVL accounts
        let controller_account = account_map.get(&usdc_controller::ID)
            .expect("USDC controller account not found");
        let controller_data = controller_account.data.as_slice();
        let tvl_accounts = get_tvl_accounts_usdc(controller_data)
            .expect("Failed to get TVL accounts");
        
        let tvl_map = create_account_map(&rpc, &tvl_accounts);
        account_map.extend(tvl_map);
        
        amm.update(&account_map)
            .expect("Failed to update AMM");
        
        // Quote for 100 USDC (6 decimals)
        let in_amount: u64 = 100_000_000;
        let quote = amm.quote(&QuoteParams {
            amount: in_amount,
            input_mint: usdc_mint::ID,
            output_mint: usdc_plus_mint::ID,
            swap_mode: SwapMode::ExactIn,
        }).expect("Failed to get quote");
        
        println!(
            "Quote: {} USDC -> {} USDC+",
            in_amount as f64 / 1_000_000.0,
            quote.out_amount as f64 / 1_000_000.0
        );
        
        assert!(quote.out_amount > 0, "Output amount should be > 0");
        assert_eq!(quote.in_amount, in_amount);
        assert_eq!(quote.fee_amount, 0);
    }

    /// Integration test: Requires deployed accounts on mainnet
    /// Tests USDC+ -> USDC quote calculation
    #[test]
    #[ignore = "Requires deployed accounts on mainnet"]
    fn test_reflect_amm_quote_usdc_plus_to_usdc() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        amm.set_route_from_mints(&usdc_plus_mint::ID, &usdc_mint::ID).unwrap();
        
        // Fetch base accounts
        let base_accounts = vec![usdc_controller::ID, usdc_plus_mint::ID];
        let mut account_map = create_account_map(&rpc, &base_accounts);
        
        // Get TVL accounts
        let controller_account = account_map.get(&usdc_controller::ID)
            .expect("USDC controller account not found");
        let controller_data = controller_account.data.as_slice();
        let tvl_accounts = get_tvl_accounts_usdc(controller_data)
            .expect("Failed to get TVL accounts");
        
        let tvl_map = create_account_map(&rpc, &tvl_accounts);
        account_map.extend(tvl_map);
        
        amm.update(&account_map)
            .expect("Failed to update AMM");
        
        // Quote for 100 USDC+ (6 decimals)
        let in_amount: u64 = 100_000_000;
        let quote = amm.quote(&QuoteParams {
            amount: in_amount,
            input_mint: usdc_plus_mint::ID,
            output_mint: usdc_mint::ID,
            swap_mode: SwapMode::ExactIn,
        }).expect("Failed to get quote");
        
        println!(
            "Quote: {} USDC+ -> {} USDC",
            in_amount as f64 / 1_000_000.0,
            quote.out_amount as f64 / 1_000_000.0
        );
        
        assert!(quote.out_amount > 0, "Output amount should be > 0");
        assert_eq!(quote.in_amount, in_amount);
        assert_eq!(quote.fee_amount, 0);
    }

    /// Integration test: Requires deployed accounts on mainnet
    /// Tests roundtrip conversion accuracy
    #[test]
    #[ignore = "Requires deployed accounts on mainnet"]
    fn test_reflect_amm_quote_roundtrip() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        amm.set_route_from_mints(&usdc_mint::ID, &usdc_plus_mint::ID).unwrap();
        
        // Fetch base accounts
        let base_accounts = vec![usdc_controller::ID, usdc_plus_mint::ID];
        let mut account_map = create_account_map(&rpc, &base_accounts);
        
        // Get TVL accounts
        let controller_account = account_map.get(&usdc_controller::ID)
            .expect("USDC controller account not found");
        let controller_data = controller_account.data.as_slice();
        let tvl_accounts = get_tvl_accounts_usdc(controller_data)
            .expect("Failed to get TVL accounts");
        
        let tvl_map = create_account_map(&rpc, &tvl_accounts);
        account_map.extend(tvl_map);
        
        amm.update(&account_map)
            .expect("Failed to update AMM");
        
        // Start with 1000 USDC
        let initial_usdc: u64 = 1_000_000_000;
        
        // USDC -> USDC+
        let quote1 = amm.quote(&QuoteParams {
            amount: initial_usdc,
            input_mint: usdc_mint::ID,
            output_mint: usdc_plus_mint::ID,
            swap_mode: SwapMode::ExactIn,
        }).expect("Failed to get deposit quote");
        
        // USDC+ -> USDC
        let quote2 = amm.quote(&QuoteParams {
            amount: quote1.out_amount,
            input_mint: usdc_plus_mint::ID,
            output_mint: usdc_mint::ID,
            swap_mode: SwapMode::ExactIn,
        }).expect("Failed to get redemption quote");
        
        println!(
            "Roundtrip: {} USDC -> {} USDC+ -> {} USDC",
            initial_usdc as f64 / 1_000_000.0,
            quote1.out_amount as f64 / 1_000_000.0,
            quote2.out_amount as f64 / 1_000_000.0
        );
        
        // Should get back approximately the same amount (within tolerance due to rounding/yield)
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

    /// Integration test: Requires deployed accounts on mainnet
    /// Tests swap account generation for deposit
    #[test]
    #[ignore = "Requires deployed accounts on mainnet"]
    fn test_reflect_amm_swap_and_account_metas_deposit() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        // Fetch controller account
        let controller_account = rpc.get_account(&usdc_controller::ID)
            .expect("USDC controller account not found - account may not be deployed");
        
        let controller_data = controller_account.data;
        
        // Verify controller data can be deserialized
        get_tvl_accounts_usdc(&controller_data)
            .expect("Failed to deserialize controller data");
        
        amm.controller_data = Some(controller_data);
        amm.set_route_from_mints(&usdc_mint::ID, &usdc_plus_mint::ID)
            .expect("Failed to set route");
        
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
        
        let result = amm.get_swap_and_account_metas(&swap_params)
            .expect("Failed to get swap account metas");
        
        assert!(!result.account_metas.is_empty(), "Account metas should not be empty");
        // First account should be the user (signer)
        assert_eq!(result.account_metas[0].pubkey, user);
        assert!(result.account_metas[0].is_signer, "User should be signer");
        
        // Verify account order matches expected structure
        assert!(result.account_metas.len() >= 12, "Should have at least 12 base accounts");
    }

    /// Integration test: Requires deployed accounts on mainnet
    /// Tests swap account generation for withdrawal
    #[test]
    #[ignore = "Requires deployed accounts on mainnet"]
    fn test_reflect_amm_swap_and_account_metas_withdraw() {
        let rpc = RpcClient::new(RPC_URL);
        let mut amm = ReflectAmm::new();
        
        // Fetch controller account
        let controller_account = rpc.get_account(&usdc_controller::ID)
            .expect("USDC controller account not found - account may not be deployed");
        
        let controller_data = controller_account.data;
        
        // Verify controller data can be deserialized
        get_tvl_accounts_usdc(&controller_data)
            .expect("Failed to deserialize controller data");
        
        amm.controller_data = Some(controller_data);
        amm.set_route_from_mints(&usdc_plus_mint::ID, &usdc_mint::ID)
            .expect("Failed to set route");
        
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
        
        let result = amm.get_swap_and_account_metas(&swap_params)
            .expect("Failed to get swap account metas");
        
        assert!(!result.account_metas.is_empty(), "Account metas should not be empty");
        assert_eq!(result.account_metas[0].pubkey, user);
        assert!(result.account_metas[0].is_signer, "User should be signer");
        
        // Verify account order matches expected structure
        assert!(result.account_metas.len() >= 12, "Should have at least 12 base accounts");
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
        let mut amm = ReflectAmm::new();
        amm.set_route_from_mints(&usdc_mint::ID, &usdc_plus_mint::ID).unwrap();
        
        assert_eq!(amm.label(), REFLECT_LABEL);
        assert_eq!(amm.program_id(), reflect::ID);
        assert_eq!(amm.key(), usdc_controller::ID);
        assert!(!amm.has_dynamic_accounts());
        assert!(!amm.requires_update_for_reserve_mints());
        assert!(amm.supports_exact_out());
        assert!(!amm.unidirectional());
        assert!(amm.is_active());
        
        // Test reserve mints includes both USDC and USDT routes
        let reserve_mints = amm.get_reserve_mints();
        assert!(reserve_mints.contains(&usdc_mint::ID));
        assert!(reserve_mints.contains(&usdc_plus_mint::ID));
        assert!(reserve_mints.contains(&usdt_mint::ID));
        assert!(reserve_mints.contains(&reflect_exchange_rates::ids::usdt_plus_mint::ID));
    }
}