//! Vendored Jupiter `Swap` tag + supporting types.
//!
//! This is a copy of `jupiter-amm-interface`'s `Swap` enum (Apache-2.0). It is
//! vendored rather than depended-on because `jupiter-amm-interface` v0.6.1 pulls
//! the solana 3.x/4.x split crates, which are ABI-incompatible with the solana
//! 2.2 `Pubkey`/`Instruction` that `reflect-sdk` is built on — the two cannot
//! coexist in one crate. ponytail: drop this file and depend on
//! `jupiter-amm-interface` directly once it ships a solana-2.2-compatible release.

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AccountsType {
    TransferHookA,
    TransferHookB,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemainingAccountsSlice {
    pub accounts_type: AccountsType,
    pub length: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemainingAccountsInfo {
    pub slices: Vec<RemainingAccountsSlice>,
}

/// A Jupiter backend `Swap` tag. `ReflectS1` (lending) is the one live
/// permissioned variant; every other `Reflect*` tag is a placeholder pending
/// the Jupiter-side handoff (see their doc comments). The on-chain Jupiter
/// program builds the instruction data; for most Reflect products it reads
/// the direction from the resolved account ORDER, but whitelabel's wrap/unwrap
/// share one order, so there the TAG carries direction instead.
///
/// One Rust type for the whole tag space: a new `jupiter/<product>` crate
/// appends its own variant(s) here rather than forking this enum.
#[derive(Clone, PartialEq, Debug)]
pub enum Swap {
    Saber,
    SaberAddDecimalsDeposit,
    SaberAddDecimalsWithdraw,
    TokenSwap,
    Raydium,
    Crema {
        a_to_b: bool,
    },
    Mercurial,
    Aldrin {
        side: Side,
    },
    AldrinV2 {
        side: Side,
    },
    Whirlpool {
        a_to_b: bool,
    },
    Invariant {
        x_to_y: bool,
    },
    Meteora,
    MarcoPolo {
        x_to_y: bool,
    },
    LifinityV2,
    RaydiumClmm,
    Phoenix {
        side: Side,
    },
    TokenSwapV2,
    HeliumTreasuryManagementRedeemV0,
    StakeDexStakeWrappedSol,
    MeteoraDlmm,
    OpenBookV2 {
        side: Side,
    },
    RaydiumClmmV2,
    StakeDexPrefundWithdrawStakeAndDepositStake {
        bridge_stake_seed: u32,
    },
    SanctumS {
        src_lst_value_calc_accs: u8,
        dst_lst_value_calc_accs: u8,
        src_lst_index: u32,
        dst_lst_index: u32,
    },
    SanctumSAddLiquidity {
        lst_value_calc_accs: u8,
        lst_index: u32,
    },
    SanctumSRemoveLiquidity {
        lst_value_calc_accs: u8,
        lst_index: u32,
    },
    RaydiumCP,
    WhirlpoolSwapV2 {
        a_to_b: bool,
        remaining_accounts_info: Option<RemainingAccountsInfo>,
    },
    OneIntro,
    PumpWrappedBuy,
    PumpWrappedSell,
    PerpsV2,
    PerpsV2AddLiquidity,
    PerpsV2RemoveLiquidity,
    MoonshotWrappedBuy,
    MoonshotWrappedSell,
    StabbleStableSwap,
    StabbleWeightedSwap,
    Obric {
        x_to_y: bool,
    },
    SolFi {
        is_quote_to_base: bool,
    },
    SolayerDelegateNoInit,
    SolayerUndelegateNoInit,
    ZeroFi,
    StakeDexWithdrawWrappedSol,
    VirtualsBuy,
    VirtualsSell,
    Perena {
        in_index: u8,
        out_index: u8,
    },
    PumpSwapBuy,
    PumpSwapSell,
    Gamma,
    MeteoraDlmmSwapV2 {
        remaining_accounts_info: RemainingAccountsInfo,
    },
    Woofi,
    MeteoraDammV2,
    StabbleStableSwapV2,
    StabbleWeightedSwapV2,
    RaydiumLaunchlabBuy {
        share_fee_rate: u64,
    },
    RaydiumLaunchlabSell {
        share_fee_rate: u64,
    },
    BoopdotfunWrappedBuy,
    BoopdotfunWrappedSell,
    Plasma {
        side: Side,
    },
    GoonFi {
        is_bid: bool,
        blacklist_bump: u8,
    },
    HumidiFi {
        swap_id: u64,
        is_base_to_quote: bool,
    },
    MeteoraDynamicBondingCurveSwapWithRemainingAccounts,
    TesseraV {
        side: Side,
    },

    /// lending — the one live permissioned variant.
    ReflectS1,

    // PENDING Jupiter-side assignment (cross-org): Jupiter's closed program
    // must mint real tags for mint_bond / swap_bond / redeem_bond. These
    // placeholders make the adapter compile and pin the account-order
    // contract; their discriminants are NOT wire-stable - do not serialize
    // across the handoff boundary.
    ReflectBondsMint,
    ReflectBondsSwap,
    ReflectBondsRedeem,

    // PENDING Jupiter-side assignment (cross-org): Jupiter's closed program
    // must mint real tags for wrap / unwrap. TWO tags are mandatory here -
    // wrap and unwrap share ONE account-meta order (proxy program's ix.rs),
    // so unlike ReflectS1 the tag is the only direction carrier, not the
    // account order. Discriminants are NOT wire-stable - do not serialize
    // across the handoff boundary.
    ReflectProxyWrap,
    ReflectProxyUnwrap,

    // PENDING Jupiter-side assignment (cross-org): Jupiter's closed program
    // must mint real tags for deposit / swap. Discriminants are NOT
    // wire-stable - do not serialize across the handoff boundary.
    ReflectRlpDeposit,
    ReflectRlpSwap,
}
