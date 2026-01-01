use anyhow::anyhow;
use constants::*;
use jupiter_amm_interface::{
    try_get_account_data, AccountMap, Amm, AmmContext, KeyedAccount, Quote,
    QuoteParams, Swap, SwapAndAccountMetas, SwapMode, SwapParams,
};
use solana_sdk::pubkey::Pubkey;
use types::ReflectSwap;

pub mod constants;
mod types;

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

impl Amm for ReflectAmm {
    fn from_keyed_account(
        _keyed_account: &KeyedAccount,
        _amm_context: &AmmContext,
    ) -> anyhow::Result<Self> {
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
        vec![
            self.usdc_plus_controller,
            self.usdc_plus_drift_user_acc,
            self.usdc_plus_mint,
            self.drift_usdc_spot_market,
        ]
    }

    fn update(&mut self, account_map: &AccountMap) -> anyhow::Result<()> {
        // Get
        let usdc_plus_mint =
            try_get_account_data(account_map, &self.usdc_plus_mint)?;
        let usdc_plus_drift_user_acc =
            try_get_account_data(account_map, &self.usdc_plus_drift_user_acc)?;
        let drift_usdc_spot_market =
            try_get_account_data(account_map, &self.drift_usdc_spot_market)?;
        let usdc_plus_controller =
            try_get_account_data(account_map, &self.usdc_plus_controller)?;

        let (protocol_tvl, supply) =
            usdc_plus_exchange::get_exchange_components(
                usdc_plus_controller,
                drift_usdc_spot_market,
                usdc_plus_drift_user_acc,
                usdc_plus_mint,
            )?;

        self.protocol_tvl = protocol_tvl;
        self.effective_supply = supply;

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> anyhow::Result<Quote> {
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

    fn get_swap_and_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> anyhow::Result<SwapAndAccountMetas> {
        let SwapParams {
            source_mint,
            destination_mint,
            source_token_account,
            destination_token_account,
            token_transfer_authority,
            ..
        } = swap_params;

        // Validate mint pair
        let valid_pair = (*source_mint == usdc_mint::ID
            && *destination_mint == usdc_plus_mint::ID)
            || (*source_mint == usdc_plus_mint::ID
                && *destination_mint == usdc_mint::ID);

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
            swap: Swap::TokenSwap, // Placeholder, should be ReflectS1
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

#[cfg(test)]
mod tests {
    use solana_client::rpc_client::RpcClient;
    use solana_sdk::pubkey::Pubkey;

    use super::*;

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
        let quote = amm
            .quote(&QuoteParams {
                amount: in_amount,
                input_mint: usdc_mint::ID,
                output_mint: usdc_plus_mint::ID,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();

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
        let quote = amm
            .quote(&QuoteParams {
                amount: in_amount,
                input_mint: usdc_plus_mint::ID,
                output_mint: usdc_mint::ID,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();

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
        let quote1 = amm
            .quote(&QuoteParams {
                amount: initial_usdc,
                input_mint: usdc_mint::ID,
                output_mint: usdc_plus_mint::ID,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();

        // USDC+ -> USDC.
        let quote2 = amm
            .quote(&QuoteParams {
                amount: quote1.out_amount,
                input_mint: usdc_plus_mint::ID,
                output_mint: usdc_mint::ID,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();

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
        assert!(
            result.is_err(),
            "Should fail when source and destination mint are the same"
        );
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
