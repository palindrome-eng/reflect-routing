//! Drift probe against the oracle-stacking rlp build: a slash-wiped pool (value zero, LP supply
//! above the dead shares) refuses deposits; does the venue?

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
use std::path::PathBuf;

use reflect_sdk::common::ids::TOKEN_PROGRAM;
use reflect_sdk::rlp::ids::{DOPPLER_ORACLE, PYTH_RECEIVER, RLP_PROGRAM};
use reflect_sdk::rlp::ix::{
    add_asset, initialize_lp, initialize_pool_reserve, initialize_rlp, update_action_role, AddAssetAccounts, AdminMainAccounts, InitializeLpAccounts,
    InitializePoolReserveAccounts, PriceBand, Update,
};
use reflect_sdk::rlp::{pda, AccessLevel, Action, OracleLeg, OracleProvider, Role};
use reflect_titan_common::harness::*;
use reflect_titan_common::trading_venue::{QuoteRequest, SwapType, TradingVenue};
use reflect_titan_rlp::ReflectRlpVenue;
use solana_sdk::clock::Clock;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

const NOW: Clock = Clock { slot: 500_000, epoch_start_timestamp: 0, epoch: 0, leader_schedule_epoch: 0, unix_timestamp: 1_800_000_000 };

#[tokio::test(flavor = "current_thread")]
#[ignore = "SDK v0.11.0 lacks the PoolWipedOut deposit gate; re-enable after the protocol-core sync"]
async fn slash_wiped_pool_refuses_deposits() {
    let mut svm = litesvm::LiteSVM::new();
    svm.add_program(RLP_PROGRAM, &program_bytes(&fixtures(), "rlp_stacking"));
    set_clock(&mut svm, &NOW);
    let admin = funded_user(&mut svm);
    let pid = &RLP_PROGRAM;
    let pool = pda::derive_liquidity_pool(0, pid).0;
    let settings = pda::derive_settings(pid).0;
    let mint_a = Pubkey::new_unique();
    let lp_mint = Pubkey::new_unique();
    forge_account(&mut svm, mint_a, TOKEN_PROGRAM, mint_bytes(&admin.pubkey(), 1_000_000_000_000, 6));
    forge_account(&mut svm, lp_mint, TOKEN_PROGRAM, mint_bytes(&pool, 0, 6));
    let oracle_a = Pubkey::new_unique();
    forge_account(&mut svm, oracle_a, PYTH_RECEIVER, pyth_bytes(&[0xa1; 32], 200_000_000, 1_000_000, -8, NOW.unix_timestamp - 5, NOW.slot - 5));
    let permissions = pda::derive_permissions(&admin.pubkey(), pid).0;
    send(&mut svm, "initialize_rlp", initialize_rlp(admin.pubkey(), permissions, settings, 25, pid), &admin);
    let asset_a = pda::derive_asset(&mint_a, pid).0;
    let leg = OracleLeg { oracle: oracle_a.to_bytes(), feed_id: [0xa1; 32], max_staleness: 0, inverse: false, provider: OracleProvider::Pyth, max_confidence_bps: 0, decimals: 0 };
    send(
        &mut svm,
        "add_asset",
        add_asset(
            &AddAssetAccounts { signer: admin.pubkey(), admin: permissions, settings, asset: asset_a, asset_mint: mint_a, oracle: oracle_a },
            AccessLevel::Public,
            &[leg],
            0,
            PriceBand { min_price_1e6: 1_000_000, max_price_1e6: 3_000_000 },
            vec![],
            pid,
        ),
        &admin,
    );
    send(
        &mut svm,
        "initialize_lp",
        initialize_lp(
            &InitializeLpAccounts {
                signer: admin.pubkey(),
                permissions,
                settings,
                liquidity_pool: pool,
                lp_token_mint: lp_mint,
                dead_shares_vault: pda::token_ata(&pool, &lp_mint),
            },
            3_600,
            None,
            &[0],
            &Pubkey::new_unique(),
            pid,
        ),
        &admin,
    );
    send(
        &mut svm,
        "reserve",
        initialize_pool_reserve(
            &InitializePoolReserveAccounts {
                signer: admin.pubkey(),
                permissions,
                settings,
                liquidity_pool: pool,
                asset: asset_a,
                asset_mint: mint_a,
                pool_asset_account: pda::token_ata(&pool, &mint_a),
            },
            0,
            pid,
        ),
        &admin,
    );
    let main = AdminMainAccounts { admin: admin.pubkey(), settings, admin_permissions: permissions };
    send(&mut svm, "open deposit", update_action_role(&main, Action::Deposit, Role::Public, Update::Add, pid), &admin);
    let _ = DOPPLER_ORACLE;

    let user = funded_user(&mut svm);
    forge_ata(&mut svm, &user.pubkey(), &mint_a, 10_000_000_000);
    let ulp = forge_ata(&mut svm, &user.pubkey(), &lp_mint, 0);
    let mut v = ReflectRlpVenue::new(RLP_PROGRAM, pool, vec![mint_a]);
    v.update_state(&SvmCache(&svm)).await.unwrap();
    let ix = v.generate_swap_instruction(QuoteRequest { input_mint: mint_a, output_mint: lp_mint, amount: 1_000_000_000, swap_type: SwapType::ExactIn }, user.pubkey()).unwrap();
    send(&mut svm, "genesis deposit", ix, &user);
    let supply = mint_supply(&svm, &lp_mint);
    assert!(supply > 1_000, "real LP outstanding above the dead shares: {supply}");

    // The slash: empty the reserve while the LP supply stays.
    let vault = pda::token_ata(&pool, &mint_a);
    forge_account(&mut svm, vault, TOKEN_PROGRAM, token_account_bytes(&mint_a, &pool, 0));

    let mut v = ReflectRlpVenue::new(RLP_PROGRAM, pool, vec![mint_a]);
    v.update_state(&SvmCache(&svm)).await.unwrap();
    let q = v.quote(QuoteRequest { input_mint: mint_a, output_mint: lp_mint, amount: 1_000_000_000, swap_type: SwapType::ExactIn }).unwrap();
    let quoted = (!q.not_enough_liquidity).then_some(q.expected_output);
    let before = token_balance(&svm, &ulp);
    let executed = match v.generate_swap_instruction(QuoteRequest { input_mint: mint_a, output_mint: lp_mint, amount: 1_000_000_000, swap_type: SwapType::ExactIn }, user.pubkey())
    {
        Ok(ix) => match try_send(&mut svm, ix, &user) {
            Ok(()) => Some(token_balance(&svm, &ulp) - before),
            Err(logs) => {
                eprintln!("      program: {}", logs.iter().rev().find(|l| l.contains("Error") || l.contains("failed")).cloned().unwrap_or_default());
                None
            },
        },
        Err(e) => {
            eprintln!("      venue refused to build: {e:?}");
            None
        },
    };
    eprintln!("  wiped pool deposit: venue {quoted:?} program {executed:?}");
    assert_eq!(quoted.is_some(), executed.is_some(), "venue {quoted:?} vs program {executed:?}");
}
