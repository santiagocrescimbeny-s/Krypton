//! Phase 3 test-suite: PDA derivation, bonding-curve math, instruction
//! encoding and atomic transaction assembly.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - PDA vectors were computed with an independent Python implementation of
//!   `find_program_address` (SHA-256 + Ed25519 curve check) and cross-checked
//!   against the addresses published in the official Pump.fun documentation
//!   (`global` and `fee_config`). Instruction discriminators are the values of
//!   the official IDL (`idl/pump.json`).
//! - Curve vectors come from exact integer arithmetic in Python.
//! - Wallet and mint keys are derived from fixed seeds: runs are reproducible.

use std::collections::HashMap;
use std::error::Error;

use krypton::builder::constants::{
    select_buyback_fee_recipient, ASSOCIATED_TOKEN_PROGRAM_ID, BUYBACK_FEE_RECIPIENTS,
    COMPUTE_BUDGET_PROGRAM_ID, DEFAULT_BUYBACK_FEE_RECIPIENT, DISCRIMINATOR_BUY_V3,
    DISCRIMINATOR_CREATE, DISCRIMINATOR_CREATE_V2, INITIAL_REAL_TOKEN_RESERVES,
    INITIAL_VIRTUAL_SOL_RESERVES, INITIAL_VIRTUAL_TOKEN_RESERVES, MAX_TRANSACTION_SIZE,
    MAYHEM_PROGRAM_ID, MPL_TOKEN_METADATA_PROGRAM_ID, PUMP_FEES_PROGRAM_ID, PUMP_PROGRAM_ID,
    RENT_SYSVAR_ID, SYSTEM_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, TOKEN_TOTAL_SUPPLY,
    WSOL_MINT,
};
use krypton::builder::instructions::{
    buy_v3_data, buy_v3_instruction, create_associated_token_account_idempotent,
    create_instruction, create_v2_data, legacy_create_data, set_compute_unit_limit,
    set_compute_unit_price,
};
use krypton::builder::math::{
    BuyPlan, CurveParams, DEFAULT_TOTAL_FEE_BPS, MAX_FEE_BPS, MAX_SLIPPAGE_BPS,
};
use krypton::builder::pda::{self, find_pda, lookup_table_addresses};
use krypton::builder::transaction::estimate_wire_size;
use krypton::builder::{
    CreateOptions, CreateVariant, FundingSnapshot, LaunchAddresses, LaunchBuilder, LaunchMetadata,
    LaunchParams, MintSigner,
};
use krypton::config::{Config, ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS};
use krypton::errors::{BuilderError, LauncherError};
use krypton::security::SecureWallet;
use krypton::storage::{IpfsCid, TokenMetadata, UploadedMetadata};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::{AddressLookupTableAccount, VersionedMessage};
use solana_sdk::signature::Signature;
use solana_sdk::signer::Signer;

type TestResult = Result<(), Box<dyn Error>>;

// --- Helpers -------------------------------------------------------------------

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn key(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte; 32])
}

/// Mint used by the PDA vectors: `[7; 32]` -> `US517G59...`.
fn vector_mint() -> Pubkey {
    key(7)
}

/// User used by the PDA vectors: `[9; 32]` -> `cGfHiC6K...`.
fn vector_user() -> Pubkey {
    key(9)
}

fn text(address: &Pubkey) -> String {
    address.to_string()
}

fn test_wallet() -> Result<SecureWallet, Box<dyn Error>> {
    let bytes = Keypair::new_from_array([1u8; 32]).to_bytes();
    Ok(SecureWallet::from_keypair_bytes(&bytes)?)
}

/// 12-byte name, 5-byte symbol and an 80-byte URI (gateway + CIDv1 length).
fn standard_metadata() -> Result<LaunchMetadata, Box<dyn Error>> {
    let uri = format!("https://ipfs.io/ipfs/{}", "b".repeat(59));
    assert_eq!(uri.len(), 80);
    Ok(LaunchMetadata::new("Krypton Test", "KRYPT", &uri)?)
}

fn standard_plan() -> Result<BuyPlan, Box<dyn Error>> {
    Ok(BuyPlan::from_budget(
        &CurveParams::default(),
        3_040_000_000,
        DEFAULT_TOTAL_FEE_BPS,
        500,
    )?)
}

fn standard_params() -> Result<LaunchParams, Box<dyn Error>> {
    Ok(LaunchParams {
        metadata: standard_metadata()?,
        variant: CreateVariant::CreateV2,
        options: CreateOptions::default(),
        creator: None,
        buy: standard_plan()?,
        compute_unit_limit: 300_000,
        priority_fee_micro_lamports: 100_000,
        max_priority_fee_lamports: 5_000_000,
        buyback_fee_recipient: None,
    })
}

struct Fixture {
    wallet: SecureWallet,
    mint: MintSigner,
    builder: LaunchBuilder,
}

fn fixture_with(params: LaunchParams) -> Result<Fixture, Box<dyn Error>> {
    let wallet = test_wallet()?;
    let mint = MintSigner::from_seed([2u8; 32]);
    let builder = LaunchBuilder::new(wallet.pubkey(), mint.pubkey(), params)?;
    Ok(Fixture {
        wallet,
        mint,
        builder,
    })
}

fn fixture() -> Result<Fixture, Box<dyn Error>> {
    fixture_with(standard_params()?)
}

fn blockhash() -> Hash {
    Hash::new_from_array([3u8; 32])
}

fn lookup_table() -> Result<AddressLookupTableAccount, Box<dyn Error>> {
    Ok(AddressLookupTableAccount {
        key: key(0xAA),
        addresses: lookup_table_addresses()?,
    })
}

fn key_at(keys: &[Pubkey], index: u8) -> Result<Pubkey, Box<dyn Error>> {
    keys.get(usize::from(index))
        .copied()
        .ok_or_else(|| "account index out of range".into())
}

fn minimal_config_vars(creator_sol: &str) -> HashMap<String, String> {
    let secret = bs58::encode(Keypair::new_from_array([7u8; 32]).to_bytes()).into_string();
    [
        ("RPC_URL", "https://rpc.example.com"),
        ("JITO_ENGINE_URL", "https://jito.example.com"),
        ("PRIVATE_KEY", secret.as_str()),
        ("CREATOR_SOL_AMOUNT", creator_sol),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect()
}

// =================================================================================
// Constants
// =================================================================================

#[test]
fn program_ids_match_the_official_addresses() {
    assert_eq!(
        text(&PUMP_PROGRAM_ID),
        "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
    );
    assert_eq!(
        text(&PUMP_FEES_PROGRAM_ID),
        "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ"
    );
    assert_eq!(
        text(&MAYHEM_PROGRAM_ID),
        "MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e"
    );
    assert_eq!(
        text(&MPL_TOKEN_METADATA_PROGRAM_ID),
        "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s"
    );
    assert_eq!(
        text(&ASSOCIATED_TOKEN_PROGRAM_ID),
        "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
    );
    assert_eq!(
        text(&TOKEN_PROGRAM_ID),
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
    );
    assert_eq!(
        text(&TOKEN_2022_PROGRAM_ID),
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
    );
    assert_eq!(text(&SYSTEM_PROGRAM_ID), "11111111111111111111111111111111");
    assert_eq!(
        text(&COMPUTE_BUDGET_PROGRAM_ID),
        "ComputeBudget111111111111111111111111111111"
    );
    assert_eq!(
        text(&RENT_SYSVAR_ID),
        "SysvarRent111111111111111111111111111111111"
    );
    assert_eq!(
        text(&WSOL_MINT),
        "So11111111111111111111111111111111111111112"
    );
}

#[test]
fn discriminators_match_the_official_idl() {
    assert_eq!(DISCRIMINATOR_CREATE, [24, 30, 200, 40, 5, 28, 7, 119]);
    assert_eq!(
        DISCRIMINATOR_CREATE_V2,
        [214, 144, 76, 236, 95, 139, 49, 180]
    );
    assert_eq!(DISCRIMINATOR_BUY_V3, [7, 5, 29, 196, 245, 23, 101, 80]);
}

#[test]
fn buyback_recipients_are_eight_distinct_wallets() -> TestResult {
    assert_eq!(BUYBACK_FEE_RECIPIENTS.len(), 8);
    for (index, left) in BUYBACK_FEE_RECIPIENTS.iter().enumerate() {
        for right in BUYBACK_FEE_RECIPIENTS.iter().skip(index + 1) {
            assert_ne!(left, right);
        }
    }
    let first = BUYBACK_FEE_RECIPIENTS
        .first()
        .ok_or("recipient list is empty")?;
    assert_eq!(*first, DEFAULT_BUYBACK_FEE_RECIPIENT);
    assert_eq!(text(first), "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD");
    Ok(())
}

#[test]
fn buyback_selection_is_deterministic_and_in_range() {
    for byte in [0u8, 1, 7, 8, 9, 200, 255] {
        let mint = key(byte);
        let chosen = select_buyback_fee_recipient(&mint);
        assert_eq!(chosen, select_buyback_fee_recipient(&mint));
        assert!(BUYBACK_FEE_RECIPIENTS.contains(&chosen));
    }
    // First byte 0 and 8 map to the same slot; 1 maps to the next one.
    assert_eq!(
        select_buyback_fee_recipient(&key(0)),
        select_buyback_fee_recipient(&key(8))
    );
    assert_ne!(
        select_buyback_fee_recipient(&key(0)),
        select_buyback_fee_recipient(&key(1))
    );
}

// =================================================================================
// PDA derivation
// =================================================================================

#[test]
fn global_accounts_match_published_pump_addresses() -> TestResult {
    // `global` and `fee_config` are published in the official Pump.fun docs.
    assert_eq!(
        text(&pda::global()?),
        "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf"
    );
    assert_eq!(
        text(&pda::fee_config()?),
        "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt"
    );
    Ok(())
}

#[test]
fn singleton_pdas_match_reference_vectors() -> TestResult {
    assert_eq!(
        text(&pda::event_authority()?),
        "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1"
    );
    assert_eq!(
        text(&pda::mint_authority()?),
        "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM"
    );
    assert_eq!(
        text(&pda::global_volume_accumulator()?),
        "Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y"
    );
    assert_eq!(
        text(&pda::mayhem_global_params()?),
        "13ec7XdrjF3h3YcqBTFDSReRcUFwbCnJaAQspM4j6DDJ"
    );
    assert_eq!(
        text(&pda::mayhem_sol_vault()?),
        "BwWK17cbHxwWBKZkUYvzxLcNQ1YVyaFezduWbtm2de6s"
    );
    Ok(())
}

#[test]
fn per_mint_pdas_match_reference_vectors() -> TestResult {
    let mint = vector_mint();
    assert_eq!(text(&mint), "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx");

    let curve = pda::bonding_curve(&mint)?;
    assert_eq!(text(&curve), "C9g5VaBtV8rx1sfxiVAnnwQSw1FxNhx33ExWh89rJwTe");
    assert_eq!(
        text(&pda::associated_bonding_curve(
            &curve,
            &mint,
            &TOKEN_2022_PROGRAM_ID
        )?),
        "4XCS1AkKYefzbRcSrSjJ9eLzguj7p6rjsrsH29yKsxpD"
    );
    assert_eq!(
        text(&pda::associated_bonding_curve(
            &curve,
            &mint,
            &TOKEN_PROGRAM_ID
        )?),
        "2ZGuNkaqXnzsp4P7kVXpSstFa1nLvuwFxE3n4UQbcQmT"
    );
    assert_eq!(
        text(&pda::metadata(&mint)?),
        "ujra22P6fooR9bYj8aMN7hqB5EcXizecG4pWQXNqa1C"
    );
    assert_eq!(
        text(&pda::mayhem_state(&mint)?),
        "8sNyMnA2W9dTiq5Rdyew6vDMdAf8KKNadgqLVSE7o1ew"
    );
    let vault = pda::mayhem_sol_vault()?;
    assert_eq!(
        text(&pda::associated_token_address(
            &vault,
            &mint,
            &TOKEN_2022_PROGRAM_ID
        )?),
        "3c1gB77YDVDYqs3vUi2Zz4EQbzgcKpemEwL6eGYwy7Jd"
    );
    Ok(())
}

#[test]
fn per_user_pdas_match_reference_vectors() -> TestResult {
    let mint = vector_mint();
    let user = vector_user();
    assert_eq!(text(&user), "cGfHiC6Kgg3FpFZvgwGcswsCRtp4aBP2fzuXRQPizuN");

    assert_eq!(
        text(&pda::associated_token_address(
            &user,
            &mint,
            &TOKEN_2022_PROGRAM_ID
        )?),
        "41vqGaPiDrEG5pobSEFEEueDaowFtoJ3HmNxwoNBw2Gm"
    );
    assert_eq!(
        text(&pda::associated_token_address(
            &user,
            &mint,
            &TOKEN_PROGRAM_ID
        )?),
        "8GAwMPxYsE9MMapMvTwnThNeVyNh8CdYJMDmz6eEUspJ"
    );
    assert_eq!(
        text(&pda::user_volume_accumulator(&user)?),
        "4hSscnTTiBE4KufnE5zYXxx7PnBtmByNiK7hC8uXTKAf"
    );
    assert_eq!(
        text(&pda::creator_vault(&user)?),
        "EnyUWwh6v3VmiBxbnyRWXGhJgoNEzGRTWFqeJGRg1YtN"
    );
    Ok(())
}

#[test]
fn canonical_bumps_match_reference_vectors() -> TestResult {
    let mint = vector_mint();
    let mint_bytes = mint.to_bytes();
    let (global, global_bump) = find_pda("global", &[b"global"], &PUMP_PROGRAM_ID)?;
    assert_eq!(global, pda::global()?);
    assert_eq!(global_bump, 255);

    let (curve, curve_bump) = find_pda(
        "bonding_curve",
        &[b"bonding-curve", mint_bytes.as_slice()],
        &PUMP_PROGRAM_ID,
    )?;
    assert_eq!(curve, pda::bonding_curve(&mint)?);
    assert_eq!(curve_bump, 255);

    let pump_bytes = PUMP_PROGRAM_ID.to_bytes();
    let (_, fee_bump) = find_pda(
        "fee_config",
        &[b"fee_config", pump_bytes.as_slice()],
        &PUMP_FEES_PROGRAM_ID,
    )?;
    assert_eq!(fee_bump, 253);
    Ok(())
}

#[test]
fn find_pda_rejects_invalid_seeds() -> TestResult {
    // A seed longer than 32 bytes can never be valid.
    let oversized = [0u8; 33];
    let err = err_of(find_pda(
        "oversized",
        &[oversized.as_slice()],
        &PUMP_PROGRAM_ID,
    ))?;
    assert_eq!(
        err,
        BuilderError::PdaDerivationFailed {
            account: "oversized"
        }
    );
    Ok(())
}

#[test]
fn pda_derivation_is_deterministic_and_mint_specific() -> TestResult {
    assert_eq!(pda::bonding_curve(&key(1))?, pda::bonding_curve(&key(1))?);
    assert_ne!(pda::bonding_curve(&key(1))?, pda::bonding_curve(&key(2))?);
    assert_ne!(pda::metadata(&key(1))?, pda::metadata(&key(2))?);
    assert_ne!(pda::mayhem_state(&key(1))?, pda::mayhem_state(&key(2))?);
    Ok(())
}

#[test]
fn launch_addresses_match_individual_derivations() -> TestResult {
    let mint = vector_mint();
    let user = vector_user();
    let addresses = LaunchAddresses::derive(&mint, &user, CreateVariant::CreateV2)?;

    assert_eq!(addresses.variant, CreateVariant::CreateV2);
    assert_eq!(addresses.token_program, TOKEN_2022_PROGRAM_ID);
    assert_eq!(addresses.mint, mint);
    assert_eq!(addresses.user, user);
    assert_eq!(addresses.global, pda::global()?);
    assert_eq!(addresses.mint_authority, pda::mint_authority()?);
    assert_eq!(addresses.event_authority, pda::event_authority()?);
    assert_eq!(addresses.bonding_curve, pda::bonding_curve(&mint)?);
    assert_eq!(
        text(&addresses.associated_bonding_curve),
        "4XCS1AkKYefzbRcSrSjJ9eLzguj7p6rjsrsH29yKsxpD"
    );
    assert_eq!(addresses.metadata, pda::metadata(&mint)?);
    assert_eq!(addresses.mayhem_global_params, pda::mayhem_global_params()?);
    assert_eq!(addresses.mayhem_sol_vault, pda::mayhem_sol_vault()?);
    assert_eq!(addresses.mayhem_state, pda::mayhem_state(&mint)?);
    assert_eq!(
        text(&addresses.mayhem_token_vault),
        "3c1gB77YDVDYqs3vUi2Zz4EQbzgcKpemEwL6eGYwy7Jd"
    );
    assert_eq!(
        text(&addresses.associated_user),
        "41vqGaPiDrEG5pobSEFEEueDaowFtoJ3HmNxwoNBw2Gm"
    );
    assert_eq!(
        text(&addresses.associated_quote_bonding_curve),
        "EjsUWqQ5MEuJUN3trpdVtdA4JNcBeB7KdPccKXafeT8d"
    );
    assert_eq!(
        text(&addresses.associated_quote_user),
        "8ayH659MF7R9RBmgs5mcV4qi4oFxBAvTqysAb9hDuYpG"
    );
    assert_eq!(
        addresses.user_volume_accumulator,
        pda::user_volume_accumulator(&user)?
    );
    assert_eq!(addresses.fee_config, pda::fee_config()?);
    Ok(())
}

#[test]
fn legacy_variant_uses_the_spl_token_program() -> TestResult {
    let addresses =
        LaunchAddresses::derive(&vector_mint(), &vector_user(), CreateVariant::LegacyCreate)?;
    assert_eq!(addresses.token_program, TOKEN_PROGRAM_ID);
    assert_eq!(
        text(&addresses.associated_bonding_curve),
        "2ZGuNkaqXnzsp4P7kVXpSstFa1nLvuwFxE3n4UQbcQmT"
    );
    assert_eq!(
        text(&addresses.associated_user),
        "8GAwMPxYsE9MMapMvTwnThNeVyNh8CdYJMDmz6eEUspJ"
    );
    assert_eq!(CreateVariant::default(), CreateVariant::CreateV2);
    assert_eq!(
        CreateVariant::CreateV2.token_program(),
        TOKEN_2022_PROGRAM_ID
    );
    Ok(())
}

#[test]
fn lookup_table_addresses_are_unique_and_launch_independent() -> TestResult {
    let addresses = lookup_table_addresses()?;
    assert!(addresses.len() >= 10);
    for (index, left) in addresses.iter().enumerate() {
        for right in addresses.iter().skip(index + 1) {
            assert_ne!(left, right);
        }
    }
    assert!(addresses.contains(&pda::global()?));
    assert!(addresses.contains(&pda::fee_config()?));
    assert!(addresses.contains(&WSOL_MINT));
    // Programs invoked by the transaction itself are not loadable from an ALT.
    assert!(!addresses.contains(&PUMP_PROGRAM_ID));
    assert!(!addresses.contains(&COMPUTE_BUDGET_PROGRAM_ID));
    assert!(!addresses.contains(&ASSOCIATED_TOKEN_PROGRAM_ID));
    Ok(())
}

// =================================================================================
// Bonding-curve math
// =================================================================================

#[test]
fn default_curve_matches_the_documented_launch_parameters() -> TestResult {
    let curve = CurveParams::default();
    assert_eq!(curve.virtual_token_reserves, INITIAL_VIRTUAL_TOKEN_RESERVES);
    assert_eq!(curve.virtual_sol_reserves, INITIAL_VIRTUAL_SOL_RESERVES);
    assert_eq!(curve.real_token_reserves, INITIAL_REAL_TOKEN_RESERVES);
    assert_eq!(curve.token_total_supply, TOKEN_TOTAL_SUPPLY);
    // 30 SOL / 1,073,000,000 tokens (6 decimals) / 1B supply.
    assert_eq!(curve.virtual_sol_reserves, 30_000_000_000);
    assert_eq!(curve.virtual_token_reserves, 1_073_000_000_000_000);
    assert_eq!(curve.token_total_supply, 1_000_000_000_000_000);
    curve.validate()?;
    Ok(())
}

#[test]
fn three_sol_budget_buys_about_nine_point_seven_six_percent() -> TestResult {
    let curve = CurveParams::default();
    let plan = BuyPlan::from_budget(&curve, 3_040_000_000, 125, 500)?;

    assert_eq!(plan.token_amount, 97_618_434_810_938);
    assert_eq!(plan.net_cost_lamports, 3_002_469_135);
    assert_eq!(plan.expected_cost_lamports, 3_040_000_000);
    assert_eq!(plan.max_sol_cost_lamports, 3_192_000_000);
    assert_eq!(plan.fee_bps, 125);
    assert_eq!(plan.slippage_bps, 500);
    // 9.76 % of the supply, as in the strategy brief (~3.04 SOL / ~9.75 %).
    assert_eq!(plan.supply_share_bps(&curve)?, 976);
    Ok(())
}

#[test]
fn supply_target_of_975_bps_costs_about_three_sol() -> TestResult {
    let curve = CurveParams::default();
    let plan = BuyPlan::from_supply_bps(&curve, 975, 125, 500)?;

    assert_eq!(plan.token_amount, 97_500_000_000_000);
    assert_eq!(plan.net_cost_lamports, 2_998_462_328);
    assert_eq!(plan.expected_cost_lamports, 3_035_943_108);
    assert_eq!(plan.max_sol_cost_lamports, 3_187_740_264);
    assert_eq!(plan.supply_share_bps(&curve)?, 975);
    Ok(())
}

#[test]
fn one_sol_budget_vector() -> TestResult {
    let plan = BuyPlan::from_budget(&CurveParams::default(), 1_000_000_000, 125, 500)?;
    assert_eq!(plan.token_amount, 34_199_203_154_141);
    assert_eq!(plan.net_cost_lamports, 987_654_320);
    assert_eq!(plan.expected_cost_lamports, 999_999_999);
    assert_eq!(plan.max_sol_cost_lamports, 1_049_999_999);
    Ok(())
}

#[test]
fn budget_plans_never_cost_more_than_the_budget_before_slippage() -> TestResult {
    let curve = CurveParams::default();
    for budget in [
        1_000_000u64,
        123_456_789,
        500_000_000,
        1_000_000_000,
        3_040_000_000,
        10_000_000_000,
        50_000_000_000,
    ] {
        let plan = BuyPlan::from_budget(&curve, budget, 125, 500)?;
        assert!(plan.expected_cost_lamports <= budget, "budget {budget}");
        assert!(plan.max_sol_cost_lamports >= plan.expected_cost_lamports);
        assert!(plan.token_amount > 0);
    }
    Ok(())
}

#[test]
fn more_sol_always_buys_more_tokens() -> TestResult {
    let curve = CurveParams::default();
    let mut previous = 0u64;
    for budget in [100_000_000u64, 500_000_000, 1_000_000_000, 5_000_000_000] {
        let plan = BuyPlan::from_budget(&curve, budget, 125, 0)?;
        assert!(plan.token_amount > previous);
        previous = plan.token_amount;
    }
    Ok(())
}

#[test]
fn zero_slippage_keeps_the_cap_equal_to_the_expected_cost() -> TestResult {
    let plan = BuyPlan::from_budget(&CurveParams::default(), 1_000_000_000, 125, 0)?;
    assert_eq!(plan.max_sol_cost_lamports, plan.expected_cost_lamports);
    Ok(())
}

#[test]
fn zero_fee_makes_gross_equal_net() -> TestResult {
    let plan = BuyPlan::from_budget(&CurveParams::default(), 1_000_000_000, 0, 0)?;
    assert_eq!(plan.expected_cost_lamports, plan.net_cost_lamports);
    Ok(())
}

#[test]
fn exact_token_amount_round_trips_through_the_curve() -> TestResult {
    let curve = CurveParams::default();
    let plan = BuyPlan::from_token_amount(&curve, 1_000_000_000_000, 125, 500)?;
    // 1,000,000 tokens: 30e9 * 1e12 / (1.073e15 - 1e12), rounded up.
    assert_eq!(plan.net_cost_lamports, 27_985_075);
    assert!(plan.expected_cost_lamports > plan.net_cost_lamports);
    assert!(plan.max_sol_cost_lamports > plan.expected_cost_lamports);
    Ok(())
}

#[test]
fn zero_budget_and_zero_tokens_are_rejected() -> TestResult {
    let curve = CurveParams::default();
    assert_eq!(
        err_of(BuyPlan::from_budget(&curve, 0, 125, 500))?,
        BuilderError::ZeroBuyAmount
    );
    assert_eq!(
        err_of(BuyPlan::from_token_amount(&curve, 0, 125, 500))?,
        BuilderError::ZeroBuyAmount
    );
    // A single lamport is far too little to buy even one base unit.
    assert_eq!(
        err_of(BuyPlan::from_budget(&curve, 1, 125, 500))?,
        BuilderError::ZeroBuyAmount
    );
    Ok(())
}

#[test]
fn huge_budgets_are_capped_to_the_tokens_for_sale() -> TestResult {
    let curve = CurveParams::default();
    for budget in [1_000_000_000_000u64, u64::MAX] {
        let plan = BuyPlan::from_budget(&curve, budget, 125, 500)?;
        assert_eq!(plan.token_amount, curve.real_token_reserves);
        assert!(plan.expected_cost_lamports <= budget);
    }
    Ok(())
}

#[test]
fn requesting_more_than_the_curve_holds_fails() -> TestResult {
    let curve = CurveParams::default();
    let err = err_of(BuyPlan::from_token_amount(
        &curve,
        curve.real_token_reserves + 1,
        125,
        500,
    ))?;
    assert_eq!(
        err,
        BuilderError::BuyExceedsReserves {
            requested: curve.real_token_reserves + 1,
            available: curve.real_token_reserves,
        }
    );
    Ok(())
}

#[test]
fn basis_point_inputs_are_bounded() -> TestResult {
    let curve = CurveParams::default();
    assert_eq!(
        err_of(BuyPlan::from_budget(
            &curve,
            1_000_000_000,
            MAX_FEE_BPS + 1,
            500
        ))?,
        BuilderError::InvalidFee {
            bps: MAX_FEE_BPS + 1,
            max: MAX_FEE_BPS
        }
    );
    assert_eq!(
        err_of(BuyPlan::from_budget(
            &curve,
            1_000_000_000,
            125,
            MAX_SLIPPAGE_BPS + 1
        ))?,
        BuilderError::InvalidSlippage {
            bps: MAX_SLIPPAGE_BPS + 1,
            max: MAX_SLIPPAGE_BPS
        }
    );
    // The boundaries themselves are accepted.
    BuyPlan::from_budget(&curve, 1_000_000_000, MAX_FEE_BPS, MAX_SLIPPAGE_BPS)?;
    Ok(())
}

#[test]
fn supply_share_above_one_hundred_percent_is_rejected() -> TestResult {
    let curve = CurveParams::default();
    assert!(matches!(
        err_of(BuyPlan::from_supply_bps(&curve, 10_001, 125, 500))?,
        BuilderError::InvalidCurve { .. }
    ));
    // 100 % exceeds the tokens for sale (79.31 %).
    assert!(matches!(
        err_of(BuyPlan::from_supply_bps(&curve, 10_000, 125, 500))?,
        BuilderError::BuyExceedsReserves { .. }
    ));
    Ok(())
}

#[test]
fn inconsistent_curves_are_rejected() -> TestResult {
    let base = CurveParams::default();
    let zero_sol = CurveParams {
        virtual_sol_reserves: 0,
        ..base
    };
    let zero_tokens = CurveParams {
        virtual_token_reserves: 0,
        ..base
    };
    let zero_supply = CurveParams {
        token_total_supply: 0,
        ..base
    };
    let real_above_virtual = CurveParams {
        real_token_reserves: base.virtual_token_reserves + 1,
        ..base
    };
    for curve in [zero_sol, zero_tokens, zero_supply, real_above_virtual] {
        assert!(matches!(
            err_of(curve.validate())?,
            BuilderError::InvalidCurve { .. }
        ));
        assert!(matches!(
            err_of(BuyPlan::from_budget(&curve, 1_000_000_000, 125, 500))?,
            BuilderError::InvalidCurve { .. }
        ));
    }
    Ok(())
}

#[test]
fn draining_the_virtual_reserve_is_rejected() -> TestResult {
    let curve = CurveParams {
        virtual_token_reserves: 1_000,
        virtual_sol_reserves: 1_000,
        real_token_reserves: 1_000,
        token_total_supply: 1_000,
    };
    assert!(matches!(
        err_of(BuyPlan::from_token_amount(&curve, 1_000, 125, 500))?,
        BuilderError::InvalidCurve { .. }
    ));
    Ok(())
}

#[test]
fn overflowing_cost_is_reported_not_wrapped() -> TestResult {
    // Reserves chosen so that the u128 product overflows.
    let curve = CurveParams {
        virtual_token_reserves: u64::MAX,
        virtual_sol_reserves: u64::MAX,
        real_token_reserves: u64::MAX - 1,
        token_total_supply: u64::MAX,
    };
    let result = BuyPlan::from_token_amount(&curve, u64::MAX - 1, 125, 500);
    // Either the cost exceeds u64 or an intermediate overflows; never a panic.
    assert!(matches!(
        err_of(result)?,
        BuilderError::ArithmeticOverflow { .. }
    ));
    Ok(())
}

// =================================================================================
// Instruction encoding
// =================================================================================

#[test]
fn metadata_validation_rejects_bad_fields() -> TestResult {
    assert_eq!(
        err_of(LaunchMetadata::new("", "SYM", "https://x"))?,
        BuilderError::EmptyField { field: "name" }
    );
    assert_eq!(
        err_of(LaunchMetadata::new("Name", "", "https://x"))?,
        BuilderError::EmptyField { field: "symbol" }
    );
    assert_eq!(
        err_of(LaunchMetadata::new("Name", "SYM", ""))?,
        BuilderError::EmptyField { field: "uri" }
    );
    assert_eq!(
        err_of(LaunchMetadata::new(&"n".repeat(33), "SYM", "https://x"))?,
        BuilderError::FieldTooLong {
            field: "name",
            max: 32,
            actual: 33
        }
    );
    assert_eq!(
        err_of(LaunchMetadata::new("Name", &"s".repeat(14), "https://x"))?,
        BuilderError::FieldTooLong {
            field: "symbol",
            max: 13,
            actual: 14
        }
    );
    assert_eq!(
        err_of(LaunchMetadata::new("Name", "SYM", &"u".repeat(201)))?,
        BuilderError::FieldTooLong {
            field: "uri",
            max: 200,
            actual: 201
        }
    );
    assert_eq!(
        err_of(LaunchMetadata::new("Na\nme", "SYM", "https://x"))?,
        BuilderError::InvalidFieldContent { field: "name" }
    );
    Ok(())
}

#[test]
fn metadata_limits_are_inclusive() -> TestResult {
    LaunchMetadata::new(&"n".repeat(32), &"s".repeat(13), &"u".repeat(200))?;
    Ok(())
}

#[test]
fn metadata_is_built_from_phase_two_outputs() -> TestResult {
    let token = TokenMetadata::new("Krypton", "KRY", "A test token");
    let cid = IpfsCid::parse("bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku")?;
    let image = IpfsCid::parse("bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi")?;
    let uploaded = UploadedMetadata {
        image_uri: format!("https://ipfs.io/ipfs/{image}"),
        metadata_uri: format!("https://ipfs.io/ipfs/{cid}"),
        image_cid: image,
        metadata_cid: cid,
    };
    let launch = LaunchMetadata::from_uploaded(&token, &uploaded)?;
    assert_eq!(launch.name, "Krypton");
    assert_eq!(launch.symbol, "KRY");
    assert_eq!(launch.uri, uploaded.metadata_uri);
    Ok(())
}

#[test]
fn compute_budget_instructions_are_byte_exact() {
    let limit = set_compute_unit_limit(300_000);
    assert_eq!(limit.program_id, COMPUTE_BUDGET_PROGRAM_ID);
    assert!(limit.accounts.is_empty());
    assert_eq!(limit.data, vec![2, 0xE0, 0x93, 0x04, 0x00]);

    let price = set_compute_unit_price(100_000);
    assert_eq!(price.program_id, COMPUTE_BUDGET_PROGRAM_ID);
    assert!(price.accounts.is_empty());
    assert_eq!(price.data, vec![3, 0xA0, 0x86, 0x01, 0, 0, 0, 0, 0]);
}

#[test]
fn idempotent_ata_instruction_layout() {
    let payer = key(1);
    let ata = key(2);
    let owner = key(3);
    let mint = key(4);
    let ix = create_associated_token_account_idempotent(
        &payer,
        &ata,
        &owner,
        &mint,
        &TOKEN_2022_PROGRAM_ID,
    );
    assert_eq!(ix.program_id, ASSOCIATED_TOKEN_PROGRAM_ID);
    assert_eq!(ix.data, vec![1]);
    let layout: Vec<(Pubkey, bool, bool)> = ix
        .accounts
        .iter()
        .map(|meta| (meta.pubkey, meta.is_signer, meta.is_writable))
        .collect();
    assert_eq!(
        layout,
        vec![
            (payer, true, true),
            (ata, false, true),
            (owner, false, false),
            (mint, false, false),
            (SYSTEM_PROGRAM_ID, false, false),
            (TOKEN_2022_PROGRAM_ID, false, false),
        ]
    );
}

fn tiny_metadata() -> Result<LaunchMetadata, Box<dyn Error>> {
    Ok(LaunchMetadata::new("Kr", "KR", "ipfs://x")?)
}

fn expected_identity(discriminator: [u8; 8], creator: &Pubkey) -> Vec<u8> {
    let mut data = discriminator.to_vec();
    data.extend_from_slice(&[2, 0, 0, 0, b'K', b'r']);
    data.extend_from_slice(&[2, 0, 0, 0, b'K', b'R']);
    data.extend_from_slice(&[8, 0, 0, 0]);
    data.extend_from_slice(b"ipfs://x");
    data.extend_from_slice(&creator.to_bytes());
    data
}

#[test]
fn create_v2_data_is_byte_exact() -> TestResult {
    let creator = key(9);
    let metadata = tiny_metadata()?;

    let mut expected = expected_identity(DISCRIMINATOR_CREATE_V2, &creator);
    expected.push(0); // is_mayhem_mode = false
    let data = create_v2_data(&metadata, &creator, CreateOptions::default())?;
    assert_eq!(data, expected);
    assert_eq!(data.len(), 8 + 6 + 6 + 12 + 32 + 1);

    let mut mayhem = expected_identity(DISCRIMINATOR_CREATE_V2, &creator);
    mayhem.push(1);
    let data = create_v2_data(
        &metadata,
        &creator,
        CreateOptions {
            mayhem_mode: true,
            holder_reward: false,
        },
    )?;
    assert_eq!(data, mayhem);
    Ok(())
}

#[test]
fn create_v2_holder_reward_appends_the_optional_tail() -> TestResult {
    let creator = key(9);
    let metadata = tiny_metadata()?;
    let mut expected = expected_identity(DISCRIMINATOR_CREATE_V2, &creator);
    expected.push(0); // mayhem = false
    expected.push(0); // is_cashback_enabled = false
    expected.extend_from_slice(&0u64.to_le_bytes()); // creator_fee_bps = 0
    expected.push(1); // is_holder_reward = true
    let data = create_v2_data(
        &metadata,
        &creator,
        CreateOptions {
            mayhem_mode: false,
            holder_reward: true,
        },
    )?;
    assert_eq!(data, expected);
    Ok(())
}

#[test]
fn legacy_create_data_is_byte_exact() -> TestResult {
    let creator = key(9);
    let data = legacy_create_data(&tiny_metadata()?, &creator)?;
    assert_eq!(data, expected_identity(DISCRIMINATOR_CREATE, &creator));
    Ok(())
}

#[test]
fn buy_v3_data_is_byte_exact() -> TestResult {
    let plan = standard_plan()?;
    let data = buy_v3_data(&plan);
    assert_eq!(data.len(), 24);

    let mut expected = DISCRIMINATOR_BUY_V3.to_vec();
    expected.extend_from_slice(&97_618_434_810_938u64.to_le_bytes());
    expected.extend_from_slice(&3_192_000_000u64.to_le_bytes());
    assert_eq!(data, expected);
    Ok(())
}

fn layout_of(ix: &Instruction) -> Vec<(Pubkey, bool, bool)> {
    ix.accounts
        .iter()
        .map(|meta| (meta.pubkey, meta.is_signer, meta.is_writable))
        .collect()
}

#[test]
fn create_v2_account_list_follows_the_idl() -> TestResult {
    let mint = vector_mint();
    let user = vector_user();
    let addresses = LaunchAddresses::derive(&mint, &user, CreateVariant::CreateV2)?;
    let ix = create_instruction(
        &addresses,
        &user,
        &tiny_metadata()?,
        CreateOptions::default(),
    )?;

    assert_eq!(ix.program_id, PUMP_PROGRAM_ID);
    assert_eq!(
        layout_of(&ix),
        vec![
            (mint, true, true),
            (addresses.mint_authority, false, false),
            (addresses.bonding_curve, false, true),
            (addresses.associated_bonding_curve, false, true),
            (addresses.global, false, false),
            (user, true, true),
            (SYSTEM_PROGRAM_ID, false, false),
            (TOKEN_2022_PROGRAM_ID, false, false),
            (ASSOCIATED_TOKEN_PROGRAM_ID, false, false),
            (MAYHEM_PROGRAM_ID, false, true),
            (addresses.mayhem_global_params, false, false),
            (addresses.mayhem_sol_vault, false, true),
            (addresses.mayhem_state, false, true),
            (addresses.mayhem_token_vault, false, true),
            (addresses.event_authority, false, false),
            (PUMP_PROGRAM_ID, false, false),
        ]
    );
    assert_eq!(
        ix.data,
        create_v2_data(&tiny_metadata()?, &user, CreateOptions::default())?
    );
    Ok(())
}

#[test]
fn legacy_create_account_list_follows_the_idl() -> TestResult {
    let mint = vector_mint();
    let user = vector_user();
    let addresses = LaunchAddresses::derive(&mint, &user, CreateVariant::LegacyCreate)?;
    let ix = create_instruction(
        &addresses,
        &user,
        &tiny_metadata()?,
        CreateOptions::default(),
    )?;

    assert_eq!(ix.program_id, PUMP_PROGRAM_ID);
    assert_eq!(
        layout_of(&ix),
        vec![
            (mint, true, true),
            (addresses.mint_authority, false, false),
            (addresses.bonding_curve, false, true),
            (addresses.associated_bonding_curve, false, true),
            (addresses.global, false, false),
            (MPL_TOKEN_METADATA_PROGRAM_ID, false, false),
            (addresses.metadata, false, true),
            (user, true, true),
            (SYSTEM_PROGRAM_ID, false, false),
            (TOKEN_PROGRAM_ID, false, false),
            (ASSOCIATED_TOKEN_PROGRAM_ID, false, false),
            (RENT_SYSVAR_ID, false, false),
            (addresses.event_authority, false, false),
            (PUMP_PROGRAM_ID, false, false),
        ]
    );
    Ok(())
}

#[test]
fn legacy_create_rejects_v2_only_options() -> TestResult {
    let addresses =
        LaunchAddresses::derive(&vector_mint(), &vector_user(), CreateVariant::LegacyCreate)?;
    for options in [
        CreateOptions {
            mayhem_mode: true,
            holder_reward: false,
        },
        CreateOptions {
            mayhem_mode: false,
            holder_reward: true,
        },
    ] {
        let err = err_of(create_instruction(
            &addresses,
            &vector_user(),
            &tiny_metadata()?,
            options,
        ))?;
        assert!(matches!(err, BuilderError::UnsupportedOption { .. }));
    }
    Ok(())
}

#[test]
fn buy_v3_account_list_follows_the_idl() -> TestResult {
    let mint = vector_mint();
    let user = vector_user();
    let addresses = LaunchAddresses::derive(&mint, &user, CreateVariant::CreateV2)?;
    let plan = standard_plan()?;
    let recipient = key(0x55);
    let ix = buy_v3_instruction(&addresses, &plan, &recipient);

    assert_eq!(ix.program_id, PUMP_PROGRAM_ID);
    assert_eq!(ix.data, buy_v3_data(&plan));
    assert_eq!(
        layout_of(&ix),
        vec![
            (addresses.global, false, false),
            (mint, false, false),
            (WSOL_MINT, false, false),
            (TOKEN_2022_PROGRAM_ID, false, false),
            (TOKEN_PROGRAM_ID, false, false),
            (addresses.bonding_curve, false, true),
            (addresses.associated_bonding_curve, false, true),
            (addresses.associated_quote_bonding_curve, false, true),
            (user, true, true),
            (addresses.associated_user, false, true),
            (addresses.associated_quote_user, false, true),
            (addresses.user_volume_accumulator, false, true),
            (addresses.fee_config, false, false),
            (recipient, false, true),
            (SYSTEM_PROGRAM_ID, false, false),
            (addresses.event_authority, false, false),
            (PUMP_PROGRAM_ID, false, false),
        ]
    );
    Ok(())
}

#[test]
fn legacy_buy_uses_the_spl_token_program_as_base() -> TestResult {
    let addresses =
        LaunchAddresses::derive(&vector_mint(), &vector_user(), CreateVariant::LegacyCreate)?;
    let ix = buy_v3_instruction(&addresses, &standard_plan()?, &key(0x55));
    let base_program = ix.accounts.get(3).ok_or("missing base token program")?;
    assert_eq!(base_program.pubkey, TOKEN_PROGRAM_ID);
    Ok(())
}

// =================================================================================
// Transaction assembly
// =================================================================================

#[test]
fn instruction_sequence_is_budget_budget_ata_create_buy() -> TestResult {
    let fx = fixture()?;
    let instructions = fx.builder.instructions()?;
    let programs: Vec<Pubkey> = instructions.iter().map(|ix| ix.program_id).collect();
    assert_eq!(
        programs,
        vec![
            COMPUTE_BUDGET_PROGRAM_ID,
            COMPUTE_BUDGET_PROGRAM_ID,
            ASSOCIATED_TOKEN_PROGRAM_ID,
            PUMP_PROGRAM_ID,
            PUMP_PROGRAM_ID,
        ]
    );
    let create = instructions.get(3).ok_or("missing create")?;
    let buy = instructions.get(4).ok_or("missing buy")?;
    assert!(create.data.starts_with(&DISCRIMINATOR_CREATE_V2));
    assert!(buy.data.starts_with(&DISCRIMINATOR_BUY_V3));
    Ok(())
}

#[test]
fn builder_exposes_addresses_and_parameters() -> TestResult {
    let fx = fixture()?;
    assert_eq!(fx.builder.addresses().mint, fx.mint.pubkey());
    assert_eq!(fx.builder.addresses().user, fx.wallet.pubkey());
    assert_eq!(fx.builder.params().compute_unit_limit, 300_000);
    assert_eq!(fx.builder.params().buy, standard_plan()?);
    Ok(())
}

#[test]
fn compile_produces_a_v0_message_that_fits_one_packet() -> TestResult {
    let fx = fixture()?;
    let compiled = fx.builder.compile(blockhash(), &[], &[])?;
    let message = &compiled.message;

    assert_eq!(message.recent_blockhash, blockhash());
    assert_eq!(message.header.num_required_signatures, 2);
    assert!(message.address_table_lookups.is_empty());
    // The fee payer is always the first static key, the mint the second signer.
    assert_eq!(key_at(&message.account_keys, 0)?, fx.wallet.pubkey());
    assert_eq!(key_at(&message.account_keys, 1)?, fx.mint.pubkey());
    assert_eq!(message.account_keys.len(), 25);
    assert_eq!(message.instructions.len(), 5);

    assert!(compiled.wire_size <= MAX_TRANSACTION_SIZE);
    assert_eq!(compiled.wire_size, estimate_wire_size(message));
    // Regression pin of the size model (verified against the serializer below).
    assert_eq!(compiled.wire_size, 1_212);

    // Program order survives compilation.
    let programs: Vec<Pubkey> = message
        .instructions
        .iter()
        .map(|ix| key_at(&message.account_keys, ix.program_id_index))
        .collect::<Result<_, _>>()?;
    assert_eq!(
        programs,
        vec![
            COMPUTE_BUDGET_PROGRAM_ID,
            COMPUTE_BUDGET_PROGRAM_ID,
            ASSOCIATED_TOKEN_PROGRAM_ID,
            PUMP_PROGRAM_ID,
            PUMP_PROGRAM_ID,
        ]
    );
    Ok(())
}

#[test]
fn lookup_table_shrinks_the_transaction() -> TestResult {
    let fx = fixture()?;
    let without = fx.builder.compile(blockhash(), &[], &[])?;
    let table = lookup_table()?;
    let with = fx
        .builder
        .compile(blockhash(), std::slice::from_ref(&table), &[])?;

    assert_eq!(with.message.address_table_lookups.len(), 1);
    let lookup = with
        .message
        .address_table_lookups
        .first()
        .ok_or("missing lookup")?;
    assert_eq!(lookup.account_key, table.key);
    // 11 accounts of the launch live in the table; 2 of them are writable.
    assert_eq!(lookup.writable_indexes.len(), 2);
    assert_eq!(lookup.readonly_indexes.len(), 9);
    assert_eq!(with.message.account_keys.len(), 14);

    assert!(with.wire_size + 250 < without.wire_size);
    assert_eq!(with.wire_size, 905);
    Ok(())
}

#[test]
fn oversized_launch_is_refused_unless_a_table_is_used() -> TestResult {
    let uri = "u".repeat(200);
    let mut params = standard_params()?;
    params.metadata = LaunchMetadata::new("Krypton Test", "KRYPT", &uri)?;
    let fx = fixture_with(params)?;

    let err = err_of(fx.builder.compile(blockhash(), &[], &[]))?;
    assert!(matches!(
        err,
        BuilderError::TransactionTooLarge { max: 1_232, size } if size > 1_232
    ));

    let table = lookup_table()?;
    let compiled = fx
        .builder
        .compile(blockhash(), std::slice::from_ref(&table), &[])?;
    assert!(compiled.wire_size <= MAX_TRANSACTION_SIZE);
    Ok(())
}

#[test]
fn trailing_instructions_are_appended_after_the_buy() -> TestResult {
    let fx = fixture()?;
    let table = lookup_table()?;
    let tip_like = Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: Vec::new(),
        data: vec![1, 2, 3],
    };
    let compiled = fx.builder.compile(
        blockhash(),
        std::slice::from_ref(&table),
        std::slice::from_ref(&tip_like),
    )?;
    assert_eq!(compiled.message.instructions.len(), 6);
    let last = compiled
        .message
        .instructions
        .last()
        .ok_or("missing last instruction")?;
    assert_eq!(last.data, vec![1, 2, 3]);
    Ok(())
}

#[test]
fn signed_transaction_is_valid_and_matches_the_serializer() -> TestResult {
    let fx = fixture()?;
    let table = lookup_table()?;
    let transaction = fx.builder.build_signed(
        blockhash(),
        std::slice::from_ref(&table),
        &[],
        &fx.wallet,
        &fx.mint,
    )?;

    assert_eq!(transaction.signatures.len(), 2);
    assert!(transaction
        .signatures
        .iter()
        .all(|signature| *signature != Signature::default()));

    let VersionedMessage::V0(message) = &transaction.message else {
        return Err("expected a v0 message".into());
    };
    assert_eq!(message.header.num_required_signatures, 2);

    // Ed25519 is deterministic: re-signing the serialized message with the same
    // keys must reproduce the signatures, in the order of the account keys.
    let bytes = transaction.message.serialize();
    let wallet_signature = Keypair::new_from_array([1u8; 32]).sign_message(&bytes);
    let mint_signature = Keypair::new_from_array([2u8; 32]).sign_message(&bytes);
    assert_eq!(transaction.signatures.first(), Some(&wallet_signature));
    assert_eq!(transaction.signatures.get(1), Some(&mint_signature));

    // The size model equals the real wire size: signatures (1 + 2 * 64) + message.
    assert_eq!(estimate_wire_size(message), 1 + 2 * 64 + bytes.len());
    Ok(())
}

#[test]
fn size_model_matches_the_serializer_without_a_table() -> TestResult {
    let fx = fixture()?;
    let transaction = fx
        .builder
        .build_signed(blockhash(), &[], &[], &fx.wallet, &fx.mint)?;
    let VersionedMessage::V0(message) = &transaction.message else {
        return Err("expected a v0 message".into());
    };
    let bytes = transaction.message.serialize();
    assert_eq!(estimate_wire_size(message), 1 + 2 * 64 + bytes.len());
    assert_eq!(estimate_wire_size(message), 1_212);
    Ok(())
}

#[test]
fn signing_is_deterministic() -> TestResult {
    let fx = fixture()?;
    let first = fx
        .builder
        .build_signed(blockhash(), &[], &[], &fx.wallet, &fx.mint)?;
    let second = fx
        .builder
        .build_signed(blockhash(), &[], &[], &fx.wallet, &fx.mint)?;
    assert_eq!(first.signatures, second.signatures);
    Ok(())
}

#[test]
fn signing_rejects_foreign_signers() -> TestResult {
    let fx = fixture()?;
    let compiled = fx.builder.compile(blockhash(), &[], &[])?;

    let other_wallet =
        SecureWallet::from_keypair_bytes(&Keypair::new_from_array([5u8; 32]).to_bytes())?;
    assert_eq!(
        err_of(
            fx.builder
                .sign(compiled.message.clone(), &other_wallet, &fx.mint)
        )?,
        BuilderError::PayerMismatch
    );

    let other_mint = MintSigner::from_seed([6u8; 32]);
    assert_eq!(
        err_of(fx.builder.sign(compiled.message, &fx.wallet, &other_mint))?,
        BuilderError::MintSignerMismatch
    );
    Ok(())
}

#[test]
fn legacy_launch_compiles_and_signs() -> TestResult {
    let mut params = standard_params()?;
    params.variant = CreateVariant::LegacyCreate;
    let fx = fixture_with(params)?;

    let instructions = fx.builder.instructions()?;
    let create = instructions.get(3).ok_or("missing create")?;
    assert!(create.data.starts_with(&DISCRIMINATOR_CREATE));
    assert_eq!(create.accounts.len(), 14);

    let transaction = fx
        .builder
        .build_signed(blockhash(), &[], &[], &fx.wallet, &fx.mint)?;
    assert_eq!(transaction.signatures.len(), 2);
    Ok(())
}

#[test]
fn builder_rejects_v2_options_on_the_legacy_variant() -> TestResult {
    let mut params = standard_params()?;
    params.variant = CreateVariant::LegacyCreate;
    params.options = CreateOptions {
        mayhem_mode: true,
        holder_reward: false,
    };
    let err = err_of(LaunchBuilder::new(key(1), key(2), params))?;
    assert!(matches!(err, BuilderError::UnsupportedOption { .. }));
    Ok(())
}

#[test]
fn explicit_creator_and_recipient_are_honoured() -> TestResult {
    let creator = key(0x42);
    let recipient = DEFAULT_BUYBACK_FEE_RECIPIENT;
    let mut params = standard_params()?;
    params.creator = Some(creator);
    params.buyback_fee_recipient = Some(recipient);
    let fx = fixture_with(params)?;

    let instructions = fx.builder.instructions()?;
    let create = instructions.get(3).ok_or("missing create")?;
    let buy = instructions.get(4).ok_or("missing buy")?;

    // The creator pubkey sits right after the three strings in the create data.
    let creator_bytes = creator.to_bytes();
    assert!(create
        .data
        .windows(32)
        .any(|window| window == creator_bytes.as_slice()));
    let buyback = buy.accounts.get(13).ok_or("missing buyback account")?;
    assert_eq!(buyback.pubkey, recipient);
    Ok(())
}

#[test]
fn default_creator_is_the_payer_and_recipient_derives_from_the_mint() -> TestResult {
    let fx = fixture()?;
    let instructions = fx.builder.instructions()?;
    let create = instructions.get(3).ok_or("missing create")?;
    let buy = instructions.get(4).ok_or("missing buy")?;

    let payer_bytes = fx.wallet.pubkey().to_bytes();
    assert!(create
        .data
        .windows(32)
        .any(|window| window == payer_bytes.as_slice()));
    let buyback = buy.accounts.get(13).ok_or("missing buyback account")?;
    assert_eq!(
        buyback.pubkey,
        select_buyback_fee_recipient(&fx.mint.pubkey())
    );
    Ok(())
}

// =================================================================================
// Configuration, mint signer and error integration
// =================================================================================

#[test]
fn launch_params_follow_the_runtime_configuration() -> TestResult {
    let config = Config::from_vars(minimal_config_vars("3.04"))?;
    let params = LaunchParams::from_config_defaults(&config, standard_metadata()?)?;

    assert_eq!(params.buy, standard_plan()?);
    assert_eq!(params.compute_unit_limit, config.compute_unit_limit);
    assert_eq!(
        params.priority_fee_micro_lamports,
        config.priority_fee_micro_lamports
    );
    assert_eq!(
        params.max_priority_fee_lamports,
        config.max_priority_fee_lamports
    );
    assert_eq!(params.variant, CreateVariant::CreateV2);
    assert_eq!(params.options, CreateOptions::default());
    assert!(params.creator.is_none());
    assert!(params.buyback_fee_recipient.is_none());
    Ok(())
}

#[test]
fn dust_creator_buy_in_the_configuration_is_rejected() -> TestResult {
    // One lamport is accepted by the config layer but buys no token.
    let config = Config::from_vars(minimal_config_vars("0.000000001"))?;
    let err = err_of(LaunchParams::from_config_defaults(
        &config,
        standard_metadata()?,
    ))?;
    assert_eq!(err, BuilderError::ZeroBuyAmount);
    Ok(())
}

#[test]
fn mint_signers_are_unique_and_reproducible() {
    let first = MintSigner::generate();
    let second = MintSigner::generate();
    assert_ne!(first.pubkey(), second.pubkey());

    let seeded = MintSigner::from_seed([2u8; 32]);
    let again = MintSigner::from_seed([2u8; 32]);
    assert_eq!(seeded.pubkey(), again.pubkey());
    assert_eq!(seeded.pubkey(), Keypair::new_from_array([2u8; 32]).pubkey());
}

#[test]
fn mint_signer_debug_never_prints_the_seed() {
    let signer = MintSigner::from_seed([0xABu8; 32]);
    let rendered = format!("{signer:?}");
    assert!(rendered.contains("<redacted>"));
    assert!(!rendered.contains("171")); // 0xAB as a decimal byte
    assert!(!rendered.to_lowercase().contains("abab"));
}

#[test]
fn builder_errors_convert_into_the_launcher_error() {
    let err: LauncherError = BuilderError::ZeroBuyAmount.into();
    assert!(matches!(
        err,
        LauncherError::Builder(BuilderError::ZeroBuyAmount)
    ));
    assert_eq!(
        err.to_string(),
        "transaction builder error: the initial buy amount resolves to zero tokens"
    );
}

#[test]
fn builder_error_messages_are_descriptive_and_value_free() {
    let cases: Vec<(BuilderError, &str)> = vec![
        (
            BuilderError::PdaDerivationFailed { account: "global" },
            "could not derive the program address for `global`",
        ),
        (
            BuilderError::TransactionTooLarge {
                size: 1300,
                max: 1232,
            },
            "transaction is 1300 bytes, above the 1232 byte packet limit",
        ),
        (
            BuilderError::PayerMismatch,
            "the wallet does not match the transaction fee payer",
        ),
        (
            BuilderError::MintSignerMismatch,
            "the mint signer does not match the launch mint",
        ),
        (
            BuilderError::EmptyField { field: "uri" },
            "token field `uri` must not be empty",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}

// =================================================================================
// Spending guards: priority fee cap (#19), slippage bound (#20), balance (#22)
// =================================================================================

fn params_with_fee(
    price_micro_lamports: u64,
    compute_unit_limit: u32,
    cap_lamports: u64,
) -> Result<LaunchParams, Box<dyn Error>> {
    let mut params = standard_params()?;
    params.priority_fee_micro_lamports = price_micro_lamports;
    params.compute_unit_limit = compute_unit_limit;
    params.max_priority_fee_lamports = cap_lamports;
    Ok(params)
}

fn new_builder(params: LaunchParams) -> Result<LaunchBuilder, BuilderError> {
    LaunchBuilder::new(key(1), key(2), params)
}

#[test]
fn builder_accepts_a_priority_fee_exactly_at_the_cap_and_refuses_one_lamport_more() -> TestResult {
    // 5_000_000 micro-lamports x 1_000_000 CU / 1e6 = 5_000_000 lamports.
    let at_cap = params_with_fee(5_000_000, 1_000_000, 5_000_000)?;
    assert_eq!(at_cap.priority_fee_lamports(), 5_000_000);
    new_builder(at_cap)?;

    let over = params_with_fee(5_000_001, 1_000_000, 5_000_000)?;
    assert_eq!(
        err_of(new_builder(over))?,
        BuilderError::PriorityFeeTooHigh {
            fee_lamports: 5_000_001,
            max: 5_000_000
        }
    );
    Ok(())
}

#[test]
fn the_absolute_ceiling_applies_even_if_the_cap_in_the_params_is_wider() -> TestResult {
    let at_ceiling = params_with_fee(ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS, 1_000_000, u64::MAX)?;
    new_builder(at_ceiling)?;

    let beyond = params_with_fee(ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS + 1, 1_000_000, u64::MAX)?;
    assert_eq!(
        err_of(new_builder(beyond))?,
        BuilderError::PriorityFeeTooHigh {
            fee_lamports: u128::from(ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS) + 1,
            max: ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS
        }
    );
    Ok(())
}

#[test]
fn a_fee_wider_than_u64_is_refused_without_overflow() -> TestResult {
    let absurd = params_with_fee(u64::MAX, u32::MAX, u64::MAX)?;
    assert_eq!(
        err_of(new_builder(absurd))?,
        BuilderError::PriorityFeeTooHigh {
            fee_lamports: 79_228_162_495_817_593_515_540,
            max: ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS
        }
    );
    Ok(())
}

#[test]
fn the_slippage_bound_is_shared_with_the_config_layer() -> TestResult {
    assert_eq!(MAX_SLIPPAGE_BPS, krypton::config::MAX_SLIPPAGE_BPS);
    assert_eq!(MAX_SLIPPAGE_BPS, 1_000);

    let curve = CurveParams::default();
    for bps in [1_001u16, 5_000, 10_000] {
        assert_eq!(
            err_of(BuyPlan::from_budget(&curve, 1_000_000_000, 125, bps))?,
            BuilderError::InvalidSlippage {
                bps,
                max: MAX_SLIPPAGE_BPS
            }
        );
    }
    BuyPlan::from_budget(&curve, 1_000_000_000, 125, MAX_SLIPPAGE_BPS)?;
    Ok(())
}

#[test]
fn builder_refuses_a_hand_built_plan_with_excessive_slippage() -> TestResult {
    let mut params = standard_params()?;
    params.buy.slippage_bps = MAX_SLIPPAGE_BPS;
    new_builder(params.clone())?;

    params.buy.slippage_bps = MAX_SLIPPAGE_BPS + 1;
    assert_eq!(
        err_of(new_builder(params))?,
        BuilderError::InvalidSlippage {
            bps: MAX_SLIPPAGE_BPS + 1,
            max: MAX_SLIPPAGE_BPS
        }
    );
    Ok(())
}

#[test]
fn required_balance_is_the_exact_worst_case_sum() -> TestResult {
    let params = standard_params()?;
    assert_eq!(params.buy.max_sol_cost_lamports, 3_192_000_000);
    assert_eq!(params.priority_fee_lamports(), 30_000);

    // 3_192_000_000 max cost + 2_074_080 Token-2022 ATA + 1_844_400 volume
    // accumulator + 10_000 signatures + 30_000 priority + 890_880 payer reserve.
    assert_eq!(params.required_balance(0, 0)?, 3_196_849_360);
    // create rent (10_000_000) and the Jito tip (100_000) are added verbatim.
    assert_eq!(params.required_balance(10_000_000, 100_000)?, 3_206_949_360);
    Ok(())
}

#[test]
fn the_legacy_variant_reserves_the_smaller_token_account_rent() -> TestResult {
    let mut params = standard_params()?;
    params.variant = CreateVariant::LegacyCreate;
    // 2_074_080 (Token-2022 ATA) is replaced by 2_039_280 (SPL token account).
    assert_eq!(params.required_balance(0, 0)?, 3_196_814_560);
    Ok(())
}

#[test]
fn required_balance_reports_overflow_instead_of_wrapping() -> TestResult {
    let params = standard_params()?;
    assert_eq!(
        err_of(params.required_balance(u64::MAX, 0))?,
        BuilderError::ArithmeticOverflow {
            context: "required balance"
        }
    );
    assert_eq!(
        err_of(params.required_balance(0, u64::MAX))?,
        BuilderError::ArithmeticOverflow {
            context: "required balance"
        }
    );

    // A fee that does not fit `u64` cannot be summed either.
    let absurd = params_with_fee(u64::MAX, u32::MAX, u64::MAX)?;
    assert_eq!(
        err_of(absurd.required_balance(0, 0))?,
        BuilderError::ArithmeticOverflow {
            context: "priority fee"
        }
    );
    Ok(())
}

#[test]
fn ensure_funded_accepts_the_exact_requirement_and_refuses_one_lamport_less() -> TestResult {
    let fx = fixture()?;
    let required = fx.builder.params().required_balance(10_000_000, 100_000)?;
    let funded = FundingSnapshot {
        available_lamports: required,
        create_rent_lamports: 10_000_000,
        tip_lamports: 100_000,
    };
    assert_eq!(fx.builder.ensure_funded(&funded)?, required);

    let short = FundingSnapshot {
        available_lamports: required.saturating_sub(1),
        ..funded
    };
    assert_eq!(
        err_of(fx.builder.ensure_funded(&short))?,
        BuilderError::InsufficientBalance {
            required,
            available: required.saturating_sub(1)
        }
    );

    let empty = FundingSnapshot {
        available_lamports: 0,
        ..funded
    };
    assert!(matches!(
        err_of(fx.builder.ensure_funded(&empty))?,
        BuilderError::InsufficientBalance { available: 0, .. }
    ));
    Ok(())
}

#[test]
fn the_budget_alone_is_not_enough_to_launch() -> TestResult {
    // A wallet holding exactly the 3.04 SOL budget (or even the slippage cap)
    // still cannot pay rent, fees and the payer reserve.
    let fx = fixture()?;
    let just_the_cap = FundingSnapshot {
        available_lamports: fx.builder.params().buy.max_sol_cost_lamports,
        create_rent_lamports: 0,
        tip_lamports: 0,
    };
    assert!(matches!(
        err_of(fx.builder.ensure_funded(&just_the_cap))?,
        BuilderError::InsufficientBalance { .. }
    ));
    Ok(())
}

#[test]
fn checked_build_refuses_an_underfunded_payer_and_matches_the_plain_build_otherwise() -> TestResult
{
    let fx = fixture()?;
    let required = fx.builder.params().required_balance(5_000_000, 0)?;
    let funded = FundingSnapshot {
        available_lamports: required,
        create_rent_lamports: 5_000_000,
        tip_lamports: 0,
    };

    let checked =
        fx.builder
            .build_signed_checked(&funded, blockhash(), &[], &[], &fx.wallet, &fx.mint)?;
    let plain = fx
        .builder
        .build_signed(blockhash(), &[], &[], &fx.wallet, &fx.mint)?;
    assert_eq!(checked.signatures, plain.signatures);

    let short = FundingSnapshot {
        available_lamports: required.saturating_sub(1),
        ..funded
    };
    assert_eq!(
        err_of(fx.builder.build_signed_checked(
            &short,
            blockhash(),
            &[],
            &[],
            &fx.wallet,
            &fx.mint
        ))?,
        BuilderError::InsufficientBalance {
            required,
            available: required.saturating_sub(1)
        }
    );
    Ok(())
}

#[test]
fn guard_error_messages_name_the_limits() {
    let cases: Vec<(BuilderError, &str)> = vec![
        (
            BuilderError::PriorityFeeTooHigh {
                fee_lamports: 6_000_000,
                max: 5_000_000,
            },
            "priority fee of 6000000 lamports exceeds the cap of 5000000 lamports",
        ),
        (
            BuilderError::InsufficientBalance {
                required: 10,
                available: 4,
            },
            "insufficient balance: 10 lamports required, 4 available",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}
