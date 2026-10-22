//! Phase 4 test-suite: Address Lookup Table integration, Jito tip assembly and
//! the Block Engine JSON-RPC client.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - Network mocking: the client talks to the in-process, std-only HTTP server in
//!   `tests/common` (ephemeral loopback port). Nothing leaves the machine.
//! - ALT address vectors come from an independent Python implementation of
//!   `find_program_address`; Base64 vectors from RFC 4648 and Python's `base64`.
//! - Wallet and mint keys derive from fixed seeds: runs are reproducible.

mod common;

use std::collections::HashMap;
use std::error::Error;
use std::time::Duration;

use common::{MockResponse, MockServer};
use krypton::builder::alt::{
    create_lookup_table_instruction, decode_lookup_table, derive_lookup_table_address,
    extend_lookup_table_instruction, launch_table_setup, DecodedLookupTable, LaunchLookupTable,
    ADDRESS_LOOKUP_TABLE_PROGRAM_ID, LOOKUP_TABLE_META_SIZE, MAX_ADDRESSES_PER_EXTEND,
    NOT_DEACTIVATED,
};
use krypton::builder::constants::{
    BUYBACK_FEE_RECIPIENTS, MAX_TRANSACTION_SIZE, SYSTEM_PROGRAM_ID,
};
use krypton::builder::pda::lookup_table_addresses;
use krypton::builder::transaction::estimate_wire_size;
use krypton::builder::{
    BuyPlan, CreateOptions, CreateVariant, CurveParams, LaunchBuilder, LaunchMetadata,
    LaunchParams, MintSigner, DEFAULT_TOTAL_FEE_BPS,
};
use krypton::config::{Config, ABSOLUTE_MAX_JITO_TIP_LAMPORTS, MIN_JITO_TIP_LAMPORTS};
use krypton::errors::{BuilderError, JitoError, LauncherError};
use krypton::network::jito::{
    assemble_launch_transaction, build_jito_tip_instruction, encode_base64, serialize_transaction,
    tip_account_for, BundleId, InflightStatus, JitoClient, JitoTip, LaunchBundleInputs,
    BUNDLES_PATH, JITO_TIP_ACCOUNTS, MAX_BUNDLE_TRANSACTIONS, MAX_RETRIES_CAP,
};
use krypton::security::SecureWallet;
use reqwest::Url;
use serde_json::{json, Value};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use solana_sdk::message::VersionedMessage;
use solana_sdk::transaction::VersionedTransaction;

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

fn blockhash() -> Hash {
    Hash::new_from_array([3u8; 32])
}

fn test_wallet() -> Result<SecureWallet, Box<dyn Error>> {
    let bytes = Keypair::new_from_array([1u8; 32]).to_bytes();
    Ok(SecureWallet::from_keypair_bytes(&bytes)?)
}

fn standard_params() -> Result<LaunchParams, Box<dyn Error>> {
    let uri = format!("https://ipfs.io/ipfs/{}", "b".repeat(59));
    Ok(LaunchParams {
        metadata: LaunchMetadata::new("Krypton Test", "KRYPT", &uri)?,
        variant: CreateVariant::CreateV2,
        options: CreateOptions::default(),
        creator: None,
        buy: BuyPlan::from_budget(
            &CurveParams::default(),
            3_040_000_000,
            DEFAULT_TOTAL_FEE_BPS,
            500,
        )?,
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
    table: LaunchLookupTable,
}

fn fixture() -> Result<Fixture, Box<dyn Error>> {
    let wallet = test_wallet()?;
    let mint = MintSigner::from_seed([2u8; 32]);
    let builder = LaunchBuilder::new(wallet.pubkey(), mint.pubkey(), standard_params()?)?;
    let table = LaunchLookupTable::from_static(key(0xAA))?;
    Ok(Fixture {
        wallet,
        mint,
        builder,
        table,
    })
}

fn minimal_config_vars() -> HashMap<String, String> {
    let secret = bs58::encode(Keypair::new_from_array([7u8; 32]).to_bytes()).into_string();
    [
        ("RPC_URL", "https://rpc.example.com"),
        ("JITO_ENGINE_URL", "https://jito.example.com"),
        ("PRIVATE_KEY", secret.as_str()),
        ("CREATOR_SOL_AMOUNT", "3.04"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect()
}

/// A signed launch transaction with the default tip, built through the real
/// assembly path.
fn signed_launch(fx: &Fixture) -> Result<VersionedTransaction, Box<dyn Error>> {
    let tip = JitoTip::new(JITO_TIP_ACCOUNTS[0], 100_000, 1_000_000)?;
    let inputs = LaunchBundleInputs {
        available_lamports: 10_000_000_000,
        create_rent_lamports: 10_000_000,
        tip: &tip,
        recent_blockhash: blockhash(),
        table: &fx.table,
    };
    Ok(assemble_launch_transaction(
        &fx.builder,
        &inputs,
        &fx.wallet,
        &fx.mint,
    )?)
}

fn v0_message(
    transaction: &VersionedTransaction,
) -> Result<&solana_sdk::message::v0::Message, Box<dyn Error>> {
    let VersionedMessage::V0(message) = &transaction.message else {
        return Err("expected a v0 message".into());
    };
    Ok(message)
}

/// Overwrites `bytes.len()` bytes of `data` starting at `offset`.
fn put(data: &mut [u8], offset: usize, bytes: &[u8]) {
    for (index, byte) in bytes.iter().enumerate() {
        if let Some(slot) = data.get_mut(offset + index) {
            *slot = *byte;
        }
    }
}

/// Raw data of a table account holding `addresses`.
fn table_account_data(
    deactivation_slot: u64,
    last_extended_slot: u64,
    authority: Option<Pubkey>,
    addresses: &[Pubkey],
) -> Vec<u8> {
    let mut data = vec![0u8; LOOKUP_TABLE_META_SIZE];
    put(&mut data, 0, &1u32.to_le_bytes());
    put(&mut data, 4, &deactivation_slot.to_le_bytes());
    put(&mut data, 12, &last_extended_slot.to_le_bytes());
    if let Some(authority) = authority {
        put(&mut data, 21, &[1u8]);
        put(&mut data, 22, &authority.to_bytes());
    }
    for address in addresses {
        data.extend_from_slice(&address.to_bytes());
    }
    data
}

// =================================================================================
// Address Lookup Table: instructions and addresses
// =================================================================================

#[test]
fn the_lookup_table_program_id_is_the_published_address() {
    assert_eq!(
        ADDRESS_LOOKUP_TABLE_PROGRAM_ID.to_string(),
        "AddressLookupTab1e1111111111111111111111111"
    );
}

#[test]
fn table_addresses_match_an_independent_derivation() -> TestResult {
    // Vectors from a separate Python implementation of `find_program_address`
    // with seeds [authority, slot as u64 LE].
    let (address, bump) = derive_lookup_table_address(&key(9), 123_456)?;
    assert_eq!(
        address.to_string(),
        "EQgzxvy2VcmGyWLsMkxjdvLmNT7yBUJeAMHTwAdoo7ju"
    );
    assert_eq!(bump, 255);

    let (address, bump) = derive_lookup_table_address(&key(9), 7)?;
    assert_eq!(
        address.to_string(),
        "JDjoSsN1rSACFYzKb3casU9ueEtUEVZQiE9CFbrm9AsL"
    );
    assert_eq!(bump, 255);

    // A different slot or authority gives a different table.
    let (other_slot, _) = derive_lookup_table_address(&key(9), 8)?;
    let (other_authority, _) = derive_lookup_table_address(&key(10), 7)?;
    assert_ne!(other_slot, address);
    assert_ne!(other_authority, address);
    Ok(())
}

#[test]
fn create_instruction_layout_is_exact() -> TestResult {
    let (instruction, table) = create_lookup_table_instruction(&key(9), &key(8), 7)?;
    assert_eq!(
        table.to_string(),
        "JDjoSsN1rSACFYzKb3casU9ueEtUEVZQiE9CFbrm9AsL"
    );
    assert_eq!(instruction.program_id, ADDRESS_LOOKUP_TABLE_PROGRAM_ID);

    // u32 index 0, u64 recent slot, u8 bump.
    let mut expected = vec![0, 0, 0, 0];
    expected.extend_from_slice(&7u64.to_le_bytes());
    expected.push(255);
    assert_eq!(instruction.data, expected);

    let layout: Vec<(Pubkey, bool, bool)> = instruction
        .accounts
        .iter()
        .map(|meta| (meta.pubkey, meta.is_signer, meta.is_writable))
        .collect();
    assert_eq!(
        layout,
        vec![
            (table, false, true),
            (key(9), true, false),
            (key(8), true, true),
            (SYSTEM_PROGRAM_ID, false, false),
        ]
    );
    Ok(())
}

#[test]
fn extend_instruction_layout_and_limits() -> TestResult {
    let table = key(5);
    let instruction = extend_lookup_table_instruction(&table, &key(9), &key(8), &[key(1), key(2)])?;
    assert_eq!(instruction.program_id, ADDRESS_LOOKUP_TABLE_PROGRAM_ID);

    // u32 index 2, u64 element count, the addresses back to back.
    let mut expected = vec![2, 0, 0, 0];
    expected.extend_from_slice(&2u64.to_le_bytes());
    expected.extend_from_slice(&[1u8; 32]);
    expected.extend_from_slice(&[2u8; 32]);
    assert_eq!(instruction.data, expected);
    assert_eq!(instruction.accounts.len(), 4);
    assert_eq!(
        instruction
            .accounts
            .iter()
            .map(|meta| (meta.is_signer, meta.is_writable))
            .collect::<Vec<_>>(),
        vec![(false, true), (true, false), (true, true), (false, false)]
    );

    assert!(matches!(
        err_of(extend_lookup_table_instruction(
            &table,
            &key(9),
            &key(8),
            &[]
        ))?,
        BuilderError::InvalidLookupTable { .. }
    ));
    let too_many = vec![key(1); MAX_ADDRESSES_PER_EXTEND + 1];
    assert!(matches!(
        err_of(extend_lookup_table_instruction(
            &table,
            &key(9),
            &key(8),
            &too_many
        ))?,
        BuilderError::InvalidLookupTable { .. }
    ));
    let at_limit = vec![key(1); MAX_ADDRESSES_PER_EXTEND];
    extend_lookup_table_instruction(&table, &key(9), &key(8), &at_limit)?;
    Ok(())
}

#[test]
fn launch_table_setup_creates_and_fills_the_table() -> TestResult {
    let (instructions, table) = launch_table_setup(&key(9), &key(8), 7)?;
    let wanted = lookup_table_addresses()?;
    assert_eq!(wanted.len(), 14);

    // 14 addresses fit one extend instruction.
    assert_eq!(instructions.len(), 2);
    let create = instructions.first().ok_or("missing create")?;
    let extend = instructions.get(1).ok_or("missing extend")?;
    assert_eq!(create.data.first(), Some(&0));
    assert_eq!(extend.data.first(), Some(&2));
    assert_eq!(extend.data.len(), 4 + 8 + 14 * 32);
    assert_eq!(extend.accounts.first().map(|meta| meta.pubkey), Some(table));

    // The stored addresses are exactly the static launch set, in order.
    let payload: Vec<Pubkey> = extend
        .data
        .get(12..)
        .ok_or("short payload")?
        .chunks_exact(32)
        .map(|chunk| <[u8; 32]>::try_from(chunk).map(Pubkey::new_from_array))
        .collect::<Result<_, _>>()?;
    assert_eq!(payload, wanted);
    Ok(())
}

// =================================================================================
// Address Lookup Table: account decoding and the launch handle
// =================================================================================

#[test]
fn decoding_round_trips_a_table_account() -> TestResult {
    let addresses = lookup_table_addresses()?;
    let data = table_account_data(NOT_DEACTIVATED, 7, Some(key(9)), &addresses);
    assert_eq!(data.len(), LOOKUP_TABLE_META_SIZE + 14 * 32);

    let decoded = decode_lookup_table(&data)?;
    assert_eq!(
        decoded,
        DecodedLookupTable {
            deactivation_slot: NOT_DEACTIVATED,
            last_extended_slot: 7,
            authority: Some(key(9)),
            addresses,
        }
    );
    assert!(decoded.is_active());
    Ok(())
}

#[test]
fn a_frozen_table_has_no_authority_and_an_empty_one_has_no_addresses() -> TestResult {
    let decoded = decode_lookup_table(&table_account_data(NOT_DEACTIVATED, 0, None, &[]))?;
    assert_eq!(decoded.authority, None);
    assert!(decoded.addresses.is_empty());
    Ok(())
}

#[test]
fn warm_up_requires_one_slot_after_the_last_extension() -> TestResult {
    let decoded = decode_lookup_table(&table_account_data(NOT_DEACTIVATED, 100, None, &[]))?;
    assert!(!decoded.is_usable_at(99));
    assert!(!decoded.is_usable_at(100));
    assert!(decoded.is_usable_at(101));

    let deactivating = decode_lookup_table(&table_account_data(150, 100, None, &[]))?;
    assert!(!deactivating.is_active());
    assert!(!deactivating.is_usable_at(1_000));
    Ok(())
}

#[test]
fn malformed_table_data_is_rejected_without_panicking() -> TestResult {
    let good = table_account_data(NOT_DEACTIVATED, 7, Some(key(9)), &[key(1)]);

    // Truncated at every length below the header, and a ragged address area.
    for len in 0..LOOKUP_TABLE_META_SIZE {
        assert!(decode_lookup_table(good.get(..len).ok_or("slice")?).is_err());
    }
    assert!(decode_lookup_table(good.get(..good.len() - 1).ok_or("slice")?).is_err());

    // Wrong state discriminator (0 = uninitialized, 2 = other).
    for state in [0u8, 2] {
        let mut bad = good.clone();
        put(&mut bad, 0, &[state]);
        assert!(decode_lookup_table(&bad).is_err(), "state {state}");
    }

    // Authority tag other than 0 / 1.
    let mut bad = good.clone();
    put(&mut bad, 21, &[2u8]);
    assert!(decode_lookup_table(&bad).is_err());

    // More than 256 addresses.
    let oversized = table_account_data(NOT_DEACTIVATED, 0, None, &vec![key(1); 257]);
    assert!(matches!(
        err_of(decode_lookup_table(&oversized))?,
        BuilderError::InvalidLookupTable { .. }
    ));
    Ok(())
}

#[test]
fn the_launch_handle_requires_a_complete_warm_and_active_table() -> TestResult {
    let wanted = lookup_table_addresses()?;
    let table_address = key(0xAA);

    let complete = decode_lookup_table(&table_account_data(NOT_DEACTIVATED, 7, None, &wanted))?;
    let handle = LaunchLookupTable::from_decoded(table_address, &complete, 8)?;
    assert_eq!(handle.address(), table_address);
    assert_eq!(handle.addresses(), wanted.as_slice());
    assert!(handle.covers_launch()?);

    // Extended in the current slot: not usable yet.
    assert!(matches!(
        err_of(LaunchLookupTable::from_decoded(table_address, &complete, 7))?,
        BuilderError::InvalidLookupTable { .. }
    ));

    // One launch address missing.
    let incomplete = decode_lookup_table(&table_account_data(
        NOT_DEACTIVATED,
        7,
        None,
        wanted.get(1..).ok_or("slice")?,
    ))?;
    assert!(matches!(
        err_of(LaunchLookupTable::from_decoded(
            table_address,
            &incomplete,
            8
        ))?,
        BuilderError::InvalidLookupTable { .. }
    ));

    // Being deactivated.
    let deactivating = decode_lookup_table(&table_account_data(7, 7, None, &wanted))?;
    assert!(matches!(
        err_of(LaunchLookupTable::from_decoded(
            table_address,
            &deactivating,
            8
        ))?,
        BuilderError::InvalidLookupTable { .. }
    ));
    Ok(())
}

#[test]
fn the_static_handle_exposes_the_launch_set_as_a_compiler_table() -> TestResult {
    let handle = LaunchLookupTable::from_static(key(0xAA))?;
    let account = handle.to_account();
    assert_eq!(account.key, key(0xAA));
    assert_eq!(account.addresses, lookup_table_addresses()?);
    Ok(())
}

// =================================================================================
// Transaction size with the table and the tip
// =================================================================================

#[test]
fn the_table_brings_the_launch_down_to_905_bytes() -> TestResult {
    let fx = fixture()?;
    let without = fx.builder.compile(blockhash(), &[], &[])?;
    let with = fx.builder.compile_with_table(blockhash(), &fx.table, &[])?;

    assert_eq!(with.wire_size, 905);
    assert!(with.wire_size + 250 < without.wire_size);
    assert_eq!(with.message.address_table_lookups.len(), 1);
    Ok(())
}

#[test]
fn compile_with_table_equals_compile_with_the_account() -> TestResult {
    let fx = fixture()?;
    let via_handle = fx.builder.compile_with_table(blockhash(), &fx.table, &[])?;
    let via_account = fx
        .builder
        .compile(blockhash(), &[fx.table.to_account()], &[])?;
    assert_eq!(via_handle.wire_size, via_account.wire_size);
    assert_eq!(
        via_handle.message.account_keys,
        via_account.message.account_keys
    );
    Ok(())
}

#[test]
fn the_tip_fits_with_generous_headroom() -> TestResult {
    let fx = fixture()?;
    let tip = build_jito_tip_instruction(&fx.builder.payer(), &JITO_TIP_ACCOUNTS[3], 100_000)?;
    let plain = fx.builder.compile_with_table(blockhash(), &fx.table, &[])?;
    let tipped =
        fx.builder
            .compile_with_table(blockhash(), &fx.table, std::slice::from_ref(&tip))?;

    // The tip costs a transfer instruction plus two static keys (the tip account
    // and the now directly invoked System program): well under 100 bytes.
    assert!(tipped.wire_size > plain.wire_size);
    assert!(tipped.wire_size <= plain.wire_size + 100);
    assert!(tipped.wire_size < MAX_TRANSACTION_SIZE);
    assert!(MAX_TRANSACTION_SIZE - tipped.wire_size >= 200, "headroom");
    Ok(())
}

#[test]
fn the_tip_account_stays_a_static_key_outside_the_table() -> TestResult {
    let fx = fixture()?;
    let tip_account = JITO_TIP_ACCOUNTS[5];
    let tip = build_jito_tip_instruction(&fx.builder.payer(), &tip_account, 100_000)?;
    let compiled =
        fx.builder
            .compile_with_table(blockhash(), &fx.table, std::slice::from_ref(&tip))?;

    assert!(compiled.message.account_keys.contains(&tip_account));
    assert!(compiled.message.account_keys.contains(&SYSTEM_PROGRAM_ID));
    assert!(!fx.table.addresses().contains(&tip_account));
    for lookup in &compiled.message.address_table_lookups {
        assert_eq!(lookup.account_key, fx.table.address());
    }
    Ok(())
}

// =================================================================================
// Tip instruction and tip limits
// =================================================================================

#[test]
fn tip_accounts_are_the_eight_official_addresses() {
    let expected = [
        "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
        "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
        "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
        "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
        "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
        "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
        "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
        "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
    ];
    let actual: Vec<String> = JITO_TIP_ACCOUNTS.iter().map(ToString::to_string).collect();
    assert_eq!(actual, expected);
    let unique: std::collections::HashSet<_> = JITO_TIP_ACCOUNTS.iter().collect();
    assert_eq!(unique.len(), 8);
    // None of them is a buyback recipient or a launch table entry.
    for account in &JITO_TIP_ACCOUNTS {
        assert!(!BUYBACK_FEE_RECIPIENTS.contains(account));
    }
}

#[test]
fn the_tip_account_pick_is_deterministic_and_spreads_over_the_list() {
    let mut seen = std::collections::HashSet::new();
    for byte in 0..=255u8 {
        let mint = key(byte);
        let first = tip_account_for(&mint);
        assert_eq!(first, tip_account_for(&mint));
        assert!(JITO_TIP_ACCOUNTS.contains(&first));
        seen.insert(first);
    }
    assert_eq!(seen.len(), 8);
}

#[test]
fn the_tip_instruction_is_a_system_transfer() -> TestResult {
    let payer = key(1);
    let instruction = build_jito_tip_instruction(&payer, &JITO_TIP_ACCOUNTS[0], 100_000)?;
    assert_eq!(instruction.program_id, SYSTEM_PROGRAM_ID);

    // u32 index 2 (Transfer), u64 lamports.
    assert_eq!(
        instruction.data,
        vec![2, 0, 0, 0, 0xA0, 0x86, 0x01, 0, 0, 0, 0, 0]
    );
    let layout: Vec<(Pubkey, bool, bool)> = instruction
        .accounts
        .iter()
        .map(|meta| (meta.pubkey, meta.is_signer, meta.is_writable))
        .collect();
    assert_eq!(
        layout,
        vec![(payer, true, true), (JITO_TIP_ACCOUNTS[0], false, true)]
    );
    Ok(())
}

#[test]
fn tips_outside_the_hard_bounds_are_refused() -> TestResult {
    let payer = key(1);
    let account = JITO_TIP_ACCOUNTS[0];
    build_jito_tip_instruction(&payer, &account, MIN_JITO_TIP_LAMPORTS)?;
    build_jito_tip_instruction(&payer, &account, ABSOLUTE_MAX_JITO_TIP_LAMPORTS)?;

    assert!(matches!(
        err_of(build_jito_tip_instruction(
            &payer,
            &account,
            MIN_JITO_TIP_LAMPORTS - 1
        ))?,
        JitoError::TipTooLow {
            lamports: 999,
            min: 1_000
        }
    ));
    assert!(matches!(
        err_of(build_jito_tip_instruction(
            &payer,
            &account,
            ABSOLUTE_MAX_JITO_TIP_LAMPORTS + 1
        ))?,
        JitoError::TipTooHigh {
            max: 10_000_000,
            ..
        }
    ));
    Ok(())
}

#[test]
fn a_tip_respects_the_configured_cap_but_never_the_absolute_ceiling() -> TestResult {
    let account = JITO_TIP_ACCOUNTS[0];
    assert_eq!(
        JitoTip::new(account, 1_000_000, 1_000_000)?.lamports(),
        1_000_000
    );
    assert!(matches!(
        err_of(JitoTip::new(account, 1_000_001, 1_000_000))?,
        JitoError::TipTooHigh {
            lamports: 1_000_001,
            max: 1_000_000
        }
    ));
    assert!(matches!(
        err_of(JitoTip::new(account, 999, 1_000_000))?,
        JitoError::TipTooLow { .. }
    ));
    // A cap wider than the ceiling is clamped to the ceiling.
    JitoTip::new(account, ABSOLUTE_MAX_JITO_TIP_LAMPORTS, u64::MAX)?;
    assert!(matches!(
        err_of(JitoTip::new(
            account,
            ABSOLUTE_MAX_JITO_TIP_LAMPORTS + 1,
            u64::MAX
        ))?,
        JitoError::TipTooHigh {
            max: 10_000_000,
            ..
        }
    ));
    Ok(())
}

#[test]
fn the_tip_follows_the_runtime_configuration() -> TestResult {
    let config = Config::from_vars(minimal_config_vars())?;
    let tip = JitoTip::from_config(&config, JITO_TIP_ACCOUNTS[2])?;
    assert_eq!(tip.lamports(), config.jito_tip_lamports);
    assert_eq!(tip.account(), JITO_TIP_ACCOUNTS[2]);

    let payer = key(1);
    let instruction = tip.instruction(&payer)?;
    assert_eq!(
        instruction.data.get(4..12),
        Some(config.jito_tip_lamports.to_le_bytes().as_slice())
    );
    Ok(())
}

// =================================================================================
// Launch assembly: balance guard, tip last, wire format
// =================================================================================

#[test]
fn the_assembled_launch_ends_with_the_tip_and_is_signed() -> TestResult {
    let fx = fixture()?;
    let transaction = signed_launch(&fx)?;
    assert_eq!(transaction.signatures.len(), 2);

    let message = v0_message(&transaction)?;
    let last = message.instructions.last().ok_or("no instructions")?;
    assert_eq!(
        message.account_keys.get(usize::from(last.program_id_index)),
        Some(&SYSTEM_PROGRAM_ID)
    );
    let mut expected = vec![2, 0, 0, 0];
    expected.extend_from_slice(&100_000u64.to_le_bytes());
    assert_eq!(last.data, expected);

    // Accounts of the tip: payer (index 0) and the tip account.
    let payer_index = last.accounts.first().copied().ok_or("missing payer")?;
    let tip_index = last.accounts.get(1).copied().ok_or("missing tip account")?;
    assert_eq!(
        message.account_keys.get(usize::from(payer_index)),
        Some(&fx.builder.payer())
    );
    assert_eq!(
        message.account_keys.get(usize::from(tip_index)),
        Some(&JITO_TIP_ACCOUNTS[0])
    );

    // The whole transaction fits one packet.
    assert!(estimate_wire_size(message) < MAX_TRANSACTION_SIZE);
    Ok(())
}

#[test]
fn the_balance_guard_includes_the_tip() -> TestResult {
    let fx = fixture()?;
    let tip = JitoTip::new(JITO_TIP_ACCOUNTS[0], 100_000, 1_000_000)?;
    let required = fx.builder.params().required_balance(10_000_000, 100_000)?;

    let inputs = |available: u64| LaunchBundleInputs {
        available_lamports: available,
        create_rent_lamports: 10_000_000,
        tip: &tip,
        recent_blockhash: blockhash(),
        table: &fx.table,
    };

    // Exactly the requirement (tip included) is enough.
    assemble_launch_transaction(&fx.builder, &inputs(required), &fx.wallet, &fx.mint)?;

    // One lamport less is refused, before anything is compiled or signed.
    let err = err_of(assemble_launch_transaction(
        &fx.builder,
        &inputs(required - 1),
        &fx.wallet,
        &fx.mint,
    ))?;
    assert!(matches!(
        err,
        JitoError::Builder(BuilderError::InsufficientBalance { required: r, available })
            if r == required && available == required - 1
    ));

    // Without the tip the same balance would have been "enough": the tip counts.
    let without_tip = fx.builder.params().required_balance(10_000_000, 0)?;
    assert_eq!(required - without_tip, 100_000);
    Ok(())
}

#[test]
fn a_foreign_signer_cannot_sign_the_launch() -> TestResult {
    let fx = fixture()?;
    let tip = JitoTip::new(JITO_TIP_ACCOUNTS[0], 100_000, 1_000_000)?;
    let inputs = LaunchBundleInputs {
        available_lamports: 10_000_000_000,
        create_rent_lamports: 10_000_000,
        tip: &tip,
        recent_blockhash: blockhash(),
        table: &fx.table,
    };
    let stranger = MintSigner::from_seed([9u8; 32]);
    let err = err_of(assemble_launch_transaction(
        &fx.builder,
        &inputs,
        &fx.wallet,
        &stranger,
    ))?;
    assert!(matches!(
        err,
        JitoError::Builder(BuilderError::MintSignerMismatch)
    ));
    Ok(())
}

#[test]
fn the_wire_format_matches_the_size_model() -> TestResult {
    let fx = fixture()?;
    let transaction = signed_launch(&fx)?;
    let message = v0_message(&transaction)?;
    let wire = serialize_transaction(&transaction);

    // short_vec(2) = 0x02, two 64-byte signatures, then the message.
    assert_eq!(wire.first(), Some(&2));
    assert_eq!(wire.len(), estimate_wire_size(message));
    assert_eq!(
        wire.len(),
        1 + 2 * 64 + transaction.message.serialize().len()
    );
    assert_eq!(
        wire.get(1..65),
        transaction.signatures.first().map(AsRef::<[u8]>::as_ref)
    );
    assert_eq!(
        wire.get(65..129),
        transaction.signatures.get(1).map(AsRef::<[u8]>::as_ref)
    );
    assert_eq!(
        wire.get(129..),
        Some(transaction.message.serialize().as_slice())
    );
    Ok(())
}

// =================================================================================
// Base64
// =================================================================================

#[test]
fn base64_matches_rfc_4648_and_a_reference_encoder() {
    for (input, expected) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(encode_base64(input.as_bytes()), expected);
    }
    assert_eq!(encode_base64(&[0xFF, 0xFE, 0xFD]), "//79");

    let every_byte: Vec<u8> = (0..=255u8).collect();
    assert_eq!(
        encode_base64(&every_byte),
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq+wsbKztLW2t7i5uru8vb6/wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj5OXm5+jp6uvs7e7v8PHy8/T19vf4+fr7/P3+/w=="
    );
}

// =================================================================================
// Bundle ids
// =================================================================================

#[test]
fn bundle_ids_are_validated_and_normalized() -> TestResult {
    let upper = "AB".repeat(32);
    let id = BundleId::parse(&upper)?;
    assert_eq!(id.as_str(), "ab".repeat(32));
    assert_eq!(id.to_string(), "ab".repeat(32));
    assert_eq!(BundleId::parse(&format!("  {upper}\n"))?, id);

    let not_hex = "g".repeat(64);
    let too_short = "a".repeat(63);
    let too_long = "a".repeat(65);
    for bad in [
        "",
        "abc",
        not_hex.as_str(),
        too_short.as_str(),
        too_long.as_str(),
    ] {
        assert!(
            matches!(err_of(BundleId::parse(bad))?, JitoError::InvalidResponse(_)),
            "{bad:?}"
        );
    }
    Ok(())
}

// =================================================================================
// Client: construction and endpoint rules
// =================================================================================

fn engine(server: &MockServer) -> Result<Url, Box<dyn Error>> {
    Ok(Url::parse(&server.url())?)
}

fn client_for(server: &MockServer, dry_run: bool) -> Result<JitoClient, Box<dyn Error>> {
    Ok(JitoClient::new(&engine(server)?, dry_run)?.with_retries(0, Duration::from_millis(1)))
}

fn rpc_ok(result: &Value) -> MockResponse {
    MockResponse::json(
        200,
        &json!({ "jsonrpc": "2.0", "result": result, "id": 1 }).to_string(),
    )
}

fn json_body(request: &common::RecordedRequest) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_slice(&request.body)?)
}

#[test]
fn engine_urls_must_be_https_or_loopback_http_without_secrets() -> TestResult {
    for good in [
        "https://mainnet.block-engine.jito.wtf",
        "https://frankfurt.mainnet.block-engine.jito.wtf/",
        "http://127.0.0.1:8899",
        "http://localhost:3000",
        "http://[::1]:3000",
    ] {
        JitoClient::new(&Url::parse(good)?, false)?;
    }
    for bad in [
        "http://jito.example.com",
        "ftp://jito.example.com",
        "https://user:pass@jito.example.com",
        "https://jito.example.com/?api-key=secret",
        "https://jito.example.com/#fragment",
    ] {
        let err = err_of(JitoClient::new(&Url::parse(bad)?, false))?;
        assert!(
            matches!(err, JitoError::InvalidEndpoint { .. }),
            "{bad}: {err}"
        );
        // The message never echoes the URL.
        assert!(!err.to_string().contains("secret"));
        assert!(!err.to_string().contains("pass"));
    }
    Ok(())
}

#[test]
fn the_client_follows_the_runtime_configuration() -> TestResult {
    let config = Config::from_vars(minimal_config_vars())?;
    let client = JitoClient::from_config(&config)?;
    assert!(client.is_dry_run(), "DRY_RUN defaults to true");

    let mut vars = minimal_config_vars();
    vars.insert("DRY_RUN".to_owned(), "false".to_owned());
    let live = JitoClient::from_config(&Config::from_vars(vars)?)?;
    assert!(!live.is_dry_run());
    Ok(())
}

#[test]
fn debug_output_shows_neither_path_nor_credentials() -> TestResult {
    let client = JitoClient::new(&Url::parse("https://jito.example.com")?, true)?;
    let rendered = format!("{client:?}");
    assert!(rendered.contains("jito.example.com"));
    assert!(!rendered.contains("api/v1"));
    Ok(())
}

// The retry budget of the read-only calls is bounded (a violation fails the build).
const _: () = assert!(MAX_RETRIES_CAP <= 5);

#[test]
fn the_bundle_limits_match_the_block_engine_documentation() {
    assert_eq!(MAX_BUNDLE_TRANSACTIONS, 5);
    assert_eq!(BUNDLES_PATH, "/api/v1/bundles");
}

#[test]
fn jito_errors_convert_into_the_launcher_error() {
    let err: LauncherError = JitoError::DryRun.into();
    assert!(matches!(err, LauncherError::Jito(JitoError::DryRun)));
    assert_eq!(
        err.to_string(),
        "Jito error: dry-run mode is active: the bundle was not submitted"
    );
}

// =================================================================================
// Client: JSON-RPC behaviour against the mock server
// =================================================================================

#[tokio::test]
async fn tip_accounts_are_fetched_and_decoded() -> TestResult {
    let accounts: Vec<String> = JITO_TIP_ACCOUNTS.iter().map(ToString::to_string).collect();
    let server = MockServer::start(move |_, _| rpc_ok(&json!(accounts)))?;
    let client = client_for(&server, true)?;

    // Read-only calls are allowed in dry-run mode.
    let fetched = client.tip_accounts().await?;
    assert_eq!(fetched, JITO_TIP_ACCOUNTS.to_vec());

    let requests = server.requests();
    let request = requests.first().ok_or("no request")?;
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, BUNDLES_PATH);
    assert!(request
        .header("content-type")
        .is_some_and(|value| value.starts_with("application/json")));
    let body = json_body(request)?;
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["method"], "getTipAccounts");
    assert_eq!(body["params"], json!([]));
    Ok(())
}

#[tokio::test]
async fn malformed_tip_account_lists_are_rejected() -> TestResult {
    for payload in [
        json!([]),
        json!(["not-a-valid-address"]),
        json!(["11111111111111111111111111111"]),
        json!([42]),
    ] {
        let server = MockServer::start(move |_, _| rpc_ok(&payload))?;
        let client = client_for(&server, true)?;
        assert!(client.tip_accounts().await.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn read_calls_retry_transient_failures() -> TestResult {
    let accounts: Vec<String> = JITO_TIP_ACCOUNTS.iter().map(ToString::to_string).collect();
    let server = MockServer::start(move |index, _| {
        if index < 2 {
            MockResponse::json(503, "{\"error\":\"busy\"}")
        } else {
            rpc_ok(&json!(accounts))
        }
    })?;
    let client =
        JitoClient::new(&engine(&server)?, true)?.with_retries(2, Duration::from_millis(1));

    assert_eq!(client.tip_accounts().await?.len(), 8);
    assert_eq!(server.request_count(), 3);
    Ok(())
}

#[tokio::test]
async fn persistent_failures_report_the_exhausted_retries() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::json(503, "overloaded"))?;
    let client =
        JitoClient::new(&engine(&server)?, true)?.with_retries(1, Duration::from_millis(1));

    let err = err_of(client.tip_accounts().await)?;
    assert!(
        matches!(
            &err,
            JitoError::RetriesExhausted { attempts: 2, source }
                if matches!(**source, JitoError::Api { status: 503, .. })
        ),
        "{err}"
    );
    assert_eq!(server.request_count(), 2);
    Ok(())
}

#[tokio::test]
async fn rate_limiting_and_credentials_map_to_their_own_errors() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::json(429, "{}"))?;
    let client = client_for(&server, true)?;
    assert!(matches!(
        err_of(client.tip_accounts().await)?,
        JitoError::RateLimited
    ));

    for status in [401u16, 403] {
        let server = MockServer::start(move |_, _| MockResponse::json(status, "{}"))?;
        let client = client_for(&server, true)?;
        let err = err_of(client.tip_accounts().await)?;
        assert!(
            matches!(err, JitoError::Unauthorized { status: s } if s == status),
            "{err}"
        );
        // Permanent errors are never retried.
        assert_eq!(server.request_count(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn json_rpc_errors_are_typed_and_sanitized() -> TestResult {
    let server = MockServer::start(|_, _| {
        MockResponse::json(
            200,
            &json!({
                "jsonrpc": "2.0",
                "error": { "code": -32602, "message": "bad params, see https://secret.example/x?key=abc" },
                "id": 1
            })
            .to_string(),
        )
    })?;
    let client = client_for(&server, true)?;

    let err = err_of(client.tip_accounts().await)?;
    let JitoError::Rpc { code, message } = &err else {
        return Err(format!("unexpected error: {err}").into());
    };
    assert_eq!(*code, -32602);
    assert!(message.contains("<url>"));
    assert!(!message.contains("secret.example"));
    assert!(!err.to_string().contains("key=abc"));
    Ok(())
}

#[tokio::test]
async fn unexpected_bodies_are_invalid_responses() -> TestResult {
    for body in ["not json at all", "{}", "{\"result\":null}", "[]"] {
        let server = MockServer::start(move |_, _| MockResponse::json(200, body))?;
        let client = client_for(&server, true)?;
        assert!(
            matches!(
                err_of(client.tip_accounts().await)?,
                JitoError::InvalidResponse(_)
            ),
            "{body}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn oversized_responses_are_refused() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::raw(200, vec![b'x'; 70 * 1024]))?;
    let client = client_for(&server, true)?;
    assert!(matches!(
        err_of(client.tip_accounts().await)?,
        JitoError::InvalidResponse(_)
    ));
    Ok(())
}

#[tokio::test]
async fn slow_servers_time_out() -> TestResult {
    let server = MockServer::start(|_, _| rpc_ok(&json!([])).delayed(Duration::from_millis(800)))?;
    let client = client_for(&server, true)?.with_timeout(Duration::from_millis(100));
    assert!(matches!(
        err_of(client.tip_accounts().await)?,
        JitoError::Timeout
    ));
    Ok(())
}

// --- sendBundle ------------------------------------------------------------------

const BUNDLE_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

#[tokio::test]
async fn dry_run_never_touches_the_network() -> TestResult {
    let server = MockServer::start(|_, _| rpc_ok(&json!(BUNDLE_ID)))?;
    let client = client_for(&server, true)?;
    let transaction = signed_launch(&fixture()?)?;

    assert!(matches!(
        err_of(client.send_bundle(std::slice::from_ref(&transaction)).await)?,
        JitoError::DryRun
    ));
    assert_eq!(server.request_count(), 0);
    Ok(())
}

#[tokio::test]
async fn a_bundle_is_sent_as_base64_and_returns_its_id() -> TestResult {
    let server = MockServer::start(|_, _| rpc_ok(&json!(BUNDLE_ID)))?;
    let client = client_for(&server, false)?;
    let transaction = signed_launch(&fixture()?)?;

    let id = client
        .send_bundle(std::slice::from_ref(&transaction))
        .await?;
    assert_eq!(id.as_str(), BUNDLE_ID);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body = json_body(requests.first().ok_or("no request")?)?;
    assert_eq!(body["method"], "sendBundle");
    assert_eq!(body["params"][1], json!({ "encoding": "base64" }));
    let sent = body["params"][0]
        .as_array()
        .ok_or("the first parameter must be an array")?;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent.first().and_then(Value::as_str),
        Some(encode_base64(&serialize_transaction(&transaction)).as_str())
    );
    Ok(())
}

#[tokio::test]
async fn bundle_shape_errors_are_caught_before_any_request() -> TestResult {
    let server = MockServer::start(|_, _| rpc_ok(&json!(BUNDLE_ID)))?;
    let client = client_for(&server, false)?;
    let transaction = signed_launch(&fixture()?)?;

    assert!(matches!(
        err_of(client.send_bundle(&[]).await)?,
        JitoError::EmptyBundle
    ));
    let six = vec![transaction; MAX_BUNDLE_TRANSACTIONS + 1];
    assert!(matches!(
        err_of(client.send_bundle(&six).await)?,
        JitoError::BundleTooLarge { count: 6, max: 5 }
    ));
    assert_eq!(server.request_count(), 0);
    Ok(())
}

#[tokio::test]
async fn sending_is_attempted_exactly_once_even_with_retries_configured() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::json(503, "busy"))?;
    let client =
        JitoClient::new(&engine(&server)?, false)?.with_retries(3, Duration::from_millis(1));
    let transaction = signed_launch(&fixture()?)?;

    let err = err_of(client.send_bundle(std::slice::from_ref(&transaction)).await)?;
    assert!(matches!(err, JitoError::Api { status: 503, .. }), "{err}");
    assert_eq!(server.request_count(), 1);
    Ok(())
}

#[tokio::test]
async fn a_malformed_bundle_id_is_rejected() -> TestResult {
    let server = MockServer::start(|_, _| rpc_ok(&json!("not-a-bundle-id")))?;
    let client = client_for(&server, false)?;
    let transaction = signed_launch(&fixture()?)?;
    assert!(matches!(
        err_of(client.send_bundle(std::slice::from_ref(&transaction)).await)?,
        JitoError::InvalidResponse(_)
    ));
    Ok(())
}

// --- getInflightBundleStatuses -----------------------------------------------------

async fn status_for(value: Value) -> Result<InflightStatus, Box<dyn Error>> {
    let server = MockServer::start(move |_, _| {
        rpc_ok(&json!({ "context": { "slot": 280_999_000u64 }, "value": value }))
    })?;
    let client = client_for(&server, true)?;
    let status = client.inflight_status(&BundleId::parse(BUNDLE_ID)?).await?;

    // The request names the bundle.
    let requests = server.requests();
    let body = json_body(requests.first().ok_or("no request")?)?;
    assert_eq!(body["method"], "getInflightBundleStatuses");
    assert_eq!(body["params"], json!([[BUNDLE_ID]]));
    Ok(status)
}

#[tokio::test]
async fn inflight_statuses_are_decoded() -> TestResult {
    let entry = |status: &str, slot: Value| json!([{ "bundle_id": BUNDLE_ID, "status": status, "landed_slot": slot }]);
    assert_eq!(
        status_for(entry("Landed", json!(280_999_001u64))).await?,
        InflightStatus::Landed { slot: 280_999_001 }
    );
    assert_eq!(
        status_for(entry("Pending", Value::Null)).await?,
        InflightStatus::Pending
    );
    assert_eq!(
        status_for(entry("Failed", Value::Null)).await?,
        InflightStatus::Failed
    );
    assert_eq!(
        status_for(entry("Invalid", Value::Null)).await?,
        InflightStatus::Invalid
    );
    assert_eq!(status_for(Value::Null).await?, InflightStatus::NotFound);
    assert_eq!(status_for(json!([])).await?, InflightStatus::NotFound);
    Ok(())
}

#[tokio::test]
async fn inconsistent_inflight_statuses_are_invalid_responses() -> TestResult {
    let landed_without_slot = json!([{ "bundle_id": BUNDLE_ID, "status": "Landed" }]);
    assert!(status_for(landed_without_slot).await.is_err());

    let unknown = json!([{ "bundle_id": BUNDLE_ID, "status": "Teleported" }]);
    assert!(status_for(unknown).await.is_err());
    Ok(())
}
