//! Instruction encoding for the atomic launch: Compute Budget, idempotent ATA
//! creation, Pump.fun `create_v2` / legacy `create`, and `buy_v3`.
//!
//! Account order and argument layout mirror the official Pump.fun IDL
//! (`pump-fun/pump-public-docs`, `idl/pump.json`). Arguments are Borsh encoded
//! by hand (`u32` length prefix for strings, little-endian integers), which
//! keeps the layout explicit, dependency-free and covered byte-for-byte by tests.
//!
//! Optional trailing arguments (`OptionBool` / `OptionU64`) are **omitted when
//! they hold their default value**, exactly as the program documentation allows
//! ("trailing optional arguments may be left off the end").

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use super::constants::{
    ASSOCIATED_TOKEN_PROGRAM_ID, ATA_CREATE_IDEMPOTENT, COMPUTE_BUDGET_PROGRAM_ID,
    COMPUTE_BUDGET_SET_UNIT_LIMIT, COMPUTE_BUDGET_SET_UNIT_PRICE, DISCRIMINATOR_BUY_V3,
    DISCRIMINATOR_CREATE, DISCRIMINATOR_CREATE_V2, DISCRIMINATOR_SELL_V3, MAX_NAME_BYTES,
    MAX_SYMBOL_BYTES, MAX_URI_BYTES, MAYHEM_PROGRAM_ID, MPL_TOKEN_METADATA_PROGRAM_ID,
    PUMP_PROGRAM_ID, RENT_SYSVAR_ID, SYSTEM_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
    WSOL_MINT,
};
use super::math::{BuyPlan, SellPlan};
use super::pda::{CreateVariant, LaunchAddresses};
use crate::errors::BuilderError;
use crate::storage::{TokenMetadata, UploadedMetadata};

/// On-chain token identity: the three strings stored by the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchMetadata {
    /// Token name (<= 32 bytes).
    pub name: String,
    /// Token symbol (<= 13 bytes on chain; the metadata engine caps it at 10).
    pub symbol: String,
    /// Metadata JSON URI (<= 200 bytes), normally an IPFS gateway URI.
    pub uri: String,
}

impl LaunchMetadata {
    /// Validates and wraps the three fields.
    ///
    /// # Errors
    /// [`BuilderError::EmptyField`], [`BuilderError::FieldTooLong`] or
    /// [`BuilderError::InvalidFieldContent`].
    pub fn new(name: &str, symbol: &str, uri: &str) -> Result<Self, BuilderError> {
        validate_field("name", name, MAX_NAME_BYTES)?;
        validate_field("symbol", symbol, MAX_SYMBOL_BYTES)?;
        validate_field("uri", uri, MAX_URI_BYTES)?;
        Ok(Self {
            name: name.to_owned(),
            symbol: symbol.to_owned(),
            uri: uri.to_owned(),
        })
    }

    /// Builds the on-chain identity from the Phase 2 outputs.
    ///
    /// # Errors
    /// See [`LaunchMetadata::new`].
    pub fn from_uploaded(
        token: &TokenMetadata,
        uploaded: &UploadedMetadata,
    ) -> Result<Self, BuilderError> {
        Self::new(&token.name, &token.symbol, &uploaded.metadata_uri)
    }
}

fn validate_field(field: &'static str, value: &str, max: usize) -> Result<(), BuilderError> {
    if value.is_empty() {
        return Err(BuilderError::EmptyField { field });
    }
    if value.len() > max {
        return Err(BuilderError::FieldTooLong {
            field,
            max,
            actual: value.len(),
        });
    }
    if value.chars().any(char::is_control) {
        return Err(BuilderError::InvalidFieldContent { field });
    }
    Ok(())
}

/// Optional flags of `create_v2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreateOptions {
    /// Enables mayhem mode for the coin.
    pub mayhem_mode: bool,
    /// Creates a holder-rewards coin (permanent; the creator fee goes to holders).
    pub holder_reward: bool,
}

// --- Compute Budget / ATA ------------------------------------------------------------

/// `ComputeBudgetInstruction::SetComputeUnitLimit`.
#[must_use]
pub fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(COMPUTE_BUDGET_SET_UNIT_LIMIT);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: Vec::new(),
        data,
    }
}

/// `ComputeBudgetInstruction::SetComputeUnitPrice` (micro-lamports per CU).
#[must_use]
pub fn set_compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(COMPUTE_BUDGET_SET_UNIT_PRICE);
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: Vec::new(),
        data,
    }
}

/// Associated Token Account `CreateIdempotent`: creates `ata` if missing and
/// succeeds silently if it already exists.
///
/// `buy_v3` requires the buyer's token account to exist; it never creates it.
#[must_use]
pub fn create_associated_token_account_idempotent(
    payer: &Pubkey,
    ata: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![ATA_CREATE_IDEMPOTENT],
    }
}

// --- Pump.fun create ------------------------------------------------------------------

fn push_string(data: &mut Vec<u8>, value: &str) -> Result<(), BuilderError> {
    let length = u32::try_from(value.len()).map_err(|_| BuilderError::ArithmeticOverflow {
        context: "string length",
    })?;
    data.extend_from_slice(&length.to_le_bytes());
    data.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encode_identity(
    discriminator: [u8; 8],
    metadata: &LaunchMetadata,
    creator: &Pubkey,
) -> Result<Vec<u8>, BuilderError> {
    let mut data = Vec::with_capacity(
        8 + 12 + metadata.name.len() + metadata.symbol.len() + metadata.uri.len() + 32 + 11,
    );
    data.extend_from_slice(&discriminator);
    push_string(&mut data, &metadata.name)?;
    push_string(&mut data, &metadata.symbol)?;
    push_string(&mut data, &metadata.uri)?;
    data.extend_from_slice(&creator.to_bytes());
    Ok(data)
}

/// Instruction data of `create_v2`.
///
/// Layout: discriminator, `name`, `symbol`, `uri` (Borsh strings), `creator`
/// (32 bytes), `is_mayhem_mode` (1 byte), then the optional trailing
/// `is_cashback_enabled` (`[0]`), `creator_fee_bps` (`0u64`) and
/// `is_holder_reward`, emitted only when `holder_reward` is set.
///
/// # Errors
/// [`BuilderError::ArithmeticOverflow`] if a string length exceeds `u32`.
pub fn create_v2_data(
    metadata: &LaunchMetadata,
    creator: &Pubkey,
    options: CreateOptions,
) -> Result<Vec<u8>, BuilderError> {
    let mut data = encode_identity(DISCRIMINATOR_CREATE_V2, metadata, creator)?;
    data.push(u8::from(options.mayhem_mode));
    if options.holder_reward {
        data.push(0); // is_cashback_enabled = false (deprecated, must stay false)
        data.extend_from_slice(&0u64.to_le_bytes()); // creator_fee_bps = 0 (standard schedule)
        data.push(1); // is_holder_reward = true
    }
    Ok(data)
}

/// Instruction data of the legacy `create`: discriminator, `name`, `symbol`,
/// `uri`, `creator`.
///
/// # Errors
/// [`BuilderError::ArithmeticOverflow`] if a string length exceeds `u32`.
pub fn legacy_create_data(
    metadata: &LaunchMetadata,
    creator: &Pubkey,
) -> Result<Vec<u8>, BuilderError> {
    encode_identity(DISCRIMINATOR_CREATE, metadata, creator)
}

/// Builds the creation instruction for the variant `addresses` was derived for.
///
/// - [`CreateVariant::CreateV2`]: 16 accounts, Token-2022 mint.
/// - [`CreateVariant::LegacyCreate`]: 14 accounts, SPL Token mint + Metaplex
///   metadata. `options` must be default.
///
/// # Errors
/// - [`BuilderError::UnsupportedOption`] for options the legacy variant lacks;
/// - [`BuilderError::ArithmeticOverflow`] from the encoder.
pub fn create_instruction(
    addresses: &LaunchAddresses,
    creator: &Pubkey,
    metadata: &LaunchMetadata,
    options: CreateOptions,
) -> Result<Instruction, BuilderError> {
    match addresses.variant {
        CreateVariant::CreateV2 => Ok(Instruction {
            program_id: PUMP_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(addresses.mint, true),
                AccountMeta::new_readonly(addresses.mint_authority, false),
                AccountMeta::new(addresses.bonding_curve, false),
                AccountMeta::new(addresses.associated_bonding_curve, false),
                AccountMeta::new_readonly(addresses.global, false),
                AccountMeta::new(addresses.user, true),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
                AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),
                AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
                AccountMeta::new(MAYHEM_PROGRAM_ID, false),
                AccountMeta::new_readonly(addresses.mayhem_global_params, false),
                AccountMeta::new(addresses.mayhem_sol_vault, false),
                AccountMeta::new(addresses.mayhem_state, false),
                AccountMeta::new(addresses.mayhem_token_vault, false),
                AccountMeta::new_readonly(addresses.event_authority, false),
                AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
            ],
            data: create_v2_data(metadata, creator, options)?,
        }),
        CreateVariant::LegacyCreate => {
            if options != CreateOptions::default() {
                return Err(BuilderError::UnsupportedOption {
                    reason: "mayhem and holder-reward modes require create_v2",
                });
            }
            Ok(Instruction {
                program_id: PUMP_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new(addresses.mint, true),
                    AccountMeta::new_readonly(addresses.mint_authority, false),
                    AccountMeta::new(addresses.bonding_curve, false),
                    AccountMeta::new(addresses.associated_bonding_curve, false),
                    AccountMeta::new_readonly(addresses.global, false),
                    AccountMeta::new_readonly(MPL_TOKEN_METADATA_PROGRAM_ID, false),
                    AccountMeta::new(addresses.metadata, false),
                    AccountMeta::new(addresses.user, true),
                    AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
                    AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                    AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
                    AccountMeta::new_readonly(RENT_SYSVAR_ID, false),
                    AccountMeta::new_readonly(addresses.event_authority, false),
                    AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
                ],
                data: legacy_create_data(metadata, creator)?,
            })
        }
    }
}

// --- Pump.fun buy ---------------------------------------------------------------------

/// Instruction data of `buy_v3`: discriminator, `amount`, `max_sol_cost`
/// (little-endian `u64`s). `partial_fill` is omitted (default `false`).
#[must_use]
pub fn buy_v3_data(plan: &BuyPlan) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DISCRIMINATOR_BUY_V3);
    data.extend_from_slice(&plan.token_amount.to_le_bytes());
    data.extend_from_slice(&plan.max_sol_cost_lamports.to_le_bytes());
    data
}

/// The 17 accounts shared by `buy_v3` and `sell_v3`, in IDL order.
fn trade_v3_accounts(
    addresses: &LaunchAddresses,
    buyback_fee_recipient: &Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(addresses.global, false),
        AccountMeta::new_readonly(addresses.mint, false),
        AccountMeta::new_readonly(WSOL_MINT, false),
        AccountMeta::new_readonly(addresses.token_program, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new(addresses.bonding_curve, false),
        AccountMeta::new(addresses.associated_bonding_curve, false),
        AccountMeta::new(addresses.associated_quote_bonding_curve, false),
        AccountMeta::new(addresses.user, true),
        AccountMeta::new(addresses.associated_user, false),
        AccountMeta::new(addresses.associated_quote_user, false),
        AccountMeta::new(addresses.user_volume_accumulator, false),
        AccountMeta::new_readonly(addresses.fee_config, false),
        AccountMeta::new(*buyback_fee_recipient, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        AccountMeta::new_readonly(addresses.event_authority, false),
        AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
    ]
}

/// Builds the SOL-paired `buy_v3` instruction (17 accounts).
///
/// The buyer's token account (`addresses.associated_user`) must exist when this
/// instruction runs; the launch transaction creates it right before.
#[must_use]
pub fn buy_v3_instruction(
    addresses: &LaunchAddresses,
    plan: &BuyPlan,
    buyback_fee_recipient: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: PUMP_PROGRAM_ID,
        accounts: trade_v3_accounts(addresses, buyback_fee_recipient),
        data: buy_v3_data(plan),
    }
}

// --- Pump.fun sell --------------------------------------------------------------------

/// Instruction data of `sell_v3`: discriminator, `amount`, `min_sol_output`
/// (little-endian `u64`s).
#[must_use]
pub fn sell_v3_data(plan: &SellPlan) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DISCRIMINATOR_SELL_V3);
    data.extend_from_slice(&plan.token_amount.to_le_bytes());
    data.extend_from_slice(&plan.min_sol_output_lamports.to_le_bytes());
    data
}

/// Builds the SOL-paired `sell_v3` instruction. It takes the **same 17 accounts
/// in the same order** as `buy_v3` (`docs/TRADE_V3.md`); only the data differs.
///
/// Cashback coins reject `sell_v3` on chain; the exit manager checks the flag
/// before quoting.
#[must_use]
pub fn sell_v3_instruction(
    addresses: &LaunchAddresses,
    plan: &SellPlan,
    buyback_fee_recipient: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: PUMP_PROGRAM_ID,
        accounts: trade_v3_accounts(addresses, buyback_fee_recipient),
        data: sell_v3_data(plan),
    }
}
