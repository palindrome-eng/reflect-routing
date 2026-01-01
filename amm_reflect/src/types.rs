use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey};

use crate::constants::token_program;

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
            AccountMeta::new_readonly(solana_sdk::sysvar::clock::ID, false),
            // #20 - usdc_oracle (remaining)
            AccountMeta::new(swap.usdc_oracle, false),
            // #21 - drift_usdc_spot_market (remaining)
            AccountMeta::new(swap.drift_usdc_spot_market, false),
        ])
    }
}
