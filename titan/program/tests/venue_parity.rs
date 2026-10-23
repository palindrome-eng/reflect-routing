//! The program's route types and loader-key slots match the off-chain builder and the SDK.
use anchor_lang::{AnchorSerialize, Discriminator};
use reflect_sdk::rlp::ix::{deposit as rlp_deposit, swap as rlp_swap, DepositAccounts, SwapAccounts};
use reflect_sdk::strategy::ix::{mint_index, mint_lending, redeem_lending, IndexAccounts, LendingAccounts};
use reflect_sdk::{proxy, rlp, strategy};
use reflect_titan_common::swap_route::{self, Venue as RouteVenue};
use reflect_titan_router::instructions::venues::reflect::{INDEX_CONTROLLER, LENDING_CONTROLLER, PROXY_BRANDED_MINT, RLP_POOL};
use reflect_titan_router::state::{SwapSpecInputV2, TitanPda, Venue};
use solana_pubkey::Pubkey;

fn pk() -> Pubkey {
    Pubkey::new_unique()
}

#[test]
fn venue_enum_matches_route_builder() {
    let cases = [
        (Venue::RaydiumAmm, RouteVenue::RaydiumAmm),
        (Venue::ReflectLending { min_out: 1 }, RouteVenue::ReflectLending { min_out: 1 }),
        (Venue::ReflectBonds { min_out: u64::MAX }, RouteVenue::ReflectBonds { min_out: u64::MAX }),
        (Venue::ReflectRlp { min_out: 7 }, RouteVenue::ReflectRlp { min_out: 7 }),
        (Venue::ReflectWhitelabel { min_out: 0 }, RouteVenue::ReflectWhitelabel { min_out: 0 }),
    ];
    for (program, builder) in cases {
        assert_eq!(program.try_to_vec().unwrap(), builder.to_borsh_bytes());
    }
    let spec = SwapSpecInputV2 { venue: Venue::ReflectRlp { min_out: 5 }, from: 0, to: 1, weight_nanos: swap_route::ROUTE_WEIGHT_ALL, n_accounts: 3 };
    let expected = swap_route::encode_swap_route_v3_data(5_000, 2, &[swap_route::SwapSpecInputV2 { venue: RouteVenue::ReflectRlp { min_out: 5 }, from: 0, to: 1, weight_nanos: swap_route::ROUTE_WEIGHT_ALL, n_accounts: 3 }]);
    let mut data = vec![42u8];
    data.extend_from_slice(&5_000u64.to_le_bytes());
    data.push(2);
    data.extend_from_slice(&1u32.to_le_bytes());
    data.extend_from_slice(&spec.try_to_vec().unwrap());
    assert_eq!(data, expected);
}

#[test]
fn router_constants_match_the_builder() {
    assert_eq!(reflect_titan_router::ID, swap_route::ROUTER_PROGRAM_ID);
    assert_eq!(TitanPda::SEED, swap_route::TITAN_PDA_SEED);
    assert_eq!(TitanPda::DISCRIMINATOR, swap_route::TITAN_PDA_DISC);
}

/// The adapter reads its loader key from a fixed named-meta slot; the SDK builders define that order.
#[test]
fn loader_key_slots_match_the_sdk_meta_order() {
    let lending = LendingAccounts {
        user: pk(),
        admin_permissions: None,
        main: pk(),
        token_registry: pk(),
        pool_registry: pk(),
        strategy_controller: pk(),
        receipt_mint: pk(),
        user_receipt_ata: pk(),
        user_earn_ata: pk(),
        strategy_controller_earn_token_account: pk(),
    };
    for ix in [mint_lending(&lending, 1, 1, vec![], &strategy::ids::REFLECT_PROGRAM), redeem_lending(&lending, 1, 1, vec![], &strategy::ids::REFLECT_PROGRAM)] {
        assert_eq!(ix.accounts[LENDING_CONTROLLER].pubkey, lending.strategy_controller);
    }

    let index = IndexAccounts {
        user: pk(),
        admin_permissions: None,
        main: pk(),
        token_registry: pk(),
        strategy_controller: pk(),
        receipt_mint: pk(),
        user_receipt_ata: pk(),
        asset_registry: pk(),
        multiocular: pk(),
    };
    assert_eq!(mint_index(&index, 1, 1, &[], vec![], &strategy::ids::REFLECT_PROGRAM).accounts[INDEX_CONTROLLER].pubkey, index.strategy_controller);

    let pool = pk();
    let deposit = DepositAccounts {
        signer: pk(),
        settings: pk(),
        permissions: None,
        liquidity_pool: pool,
        lp_token: pk(),
        user_lp_account: pk(),
        asset: pk(),
        asset_mint: pk(),
        user_asset_account: pk(),
        pool_asset_account: pk(),
        oracle: pk(),
    };
    assert_eq!(rlp_deposit(&deposit, 0, 1, 1, vec![], &rlp::ids::RLP_PROGRAM).accounts[RLP_POOL].pubkey, pool);
    let swap = SwapAccounts {
        signer: pk(),
        admin: None,
        settings: pk(),
        liquidity_pool: pool,
        token_from: pk(),
        token_from_asset: pk(),
        token_from_oracle: pk(),
        token_to: pk(),
        token_to_asset: pk(),
        token_to_oracle: pk(),
        token_from_pool: pk(),
        token_to_pool: pk(),
        token_from_signer_account: pk(),
        token_to_signer_account: pk(),
    };
    assert_eq!(rlp_swap(&swap, 1, None, vec![], &rlp::ids::RLP_PROGRAM).accounts[RLP_POOL].pubkey, pool);

    let branded = pk();
    let wrap = proxy::ix::WrapAccounts {
        user: pk(),
        stablecoin_user_token_account: pk(),
        branded_user_token_account: pk(),
        proxy_state: pk(),
        stablecoin_proxy_state_vault: pk(),
        branded_mint: branded,
        oracle: pk(),
        asset: pk(),
        stablecoin_mint: pk(),
    };
    for ix in [proxy::ix::wrap(&wrap, 1, 1, &[], &proxy::ids::PROXY_PROGRAM), proxy::ix::unwrap(&wrap, 1, 1, &[], &proxy::ids::PROXY_PROGRAM)] {
        assert_eq!(ix.accounts[PROXY_BRANDED_MINT].pubkey, branded);
    }
}
