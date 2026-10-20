//! Deterministic Program Derived Address (PDA) derivation for a Pump.fun launch.
//!
//! # About the "mint PDA"
//! A Pump.fun mint is **not** a PDA: it is a fresh keypair generated per launch
//! (it must sign `create`/`create_v2`). What *is* a PDA is the program-wide
//! **mint authority** (`["mint-authority"]` under the Pump program), which owns
//! minting rights over every coin. Both are exposed here:
//! [`mint_authority`] for the PDA and [`LaunchAddresses::mint`] for the keypair's
//! public address.
//!
//! Every function returns a [`BuilderError`] instead of panicking when a bump
//! cannot be found.

use solana_pubkey::Pubkey;

use super::constants::{
    ASSOCIATED_TOKEN_PROGRAM_ID, MAYHEM_PROGRAM_ID, MPL_TOKEN_METADATA_PROGRAM_ID,
    PUMP_FEES_PROGRAM_ID, PUMP_PROGRAM_ID, SEED_BONDING_CURVE, SEED_CREATOR_VAULT,
    SEED_EVENT_AUTHORITY, SEED_FEE_CONFIG, SEED_GLOBAL, SEED_GLOBAL_VOLUME_ACCUMULATOR,
    SEED_MAYHEM_GLOBAL_PARAMS, SEED_MAYHEM_SOL_VAULT, SEED_MAYHEM_STATE, SEED_METADATA,
    SEED_MINT_AUTHORITY, SEED_USER_VOLUME_ACCUMULATOR, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
    WSOL_MINT,
};
use crate::errors::BuilderError;

/// Which Pump.fun creation instruction is used (and therefore which token
/// program owns the mint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CreateVariant {
    /// `create_v2`: Token-2022 mint with a metadata pointer (current default).
    #[default]
    CreateV2,
    /// Legacy `create`: SPL Token mint with a Metaplex metadata account.
    LegacyCreate,
}

impl CreateVariant {
    /// Token program that owns the mint for this variant.
    #[must_use]
    pub fn token_program(self) -> Pubkey {
        match self {
            Self::CreateV2 => TOKEN_2022_PROGRAM_ID,
            Self::LegacyCreate => TOKEN_PROGRAM_ID,
        }
    }
}

/// Finds the canonical PDA (highest valid bump) for `seeds` under `program_id`.
///
/// # Errors
/// [`BuilderError::PdaDerivationFailed`] if no bump exists or the seeds are
/// invalid (too many / too long).
pub fn find_pda(
    account: &'static str,
    seeds: &[&[u8]],
    program_id: &Pubkey,
) -> Result<(Pubkey, u8), BuilderError> {
    Pubkey::try_find_program_address(seeds, program_id)
        .ok_or(BuilderError::PdaDerivationFailed { account })
}

fn pda_address(
    account: &'static str,
    seeds: &[&[u8]],
    program_id: &Pubkey,
) -> Result<Pubkey, BuilderError> {
    find_pda(account, seeds, program_id).map(|(address, _bump)| address)
}

/// `Global` configuration account of the Pump program.
///
/// # Errors
/// See [`find_pda`].
pub fn global() -> Result<Pubkey, BuilderError> {
    pda_address("global", &[SEED_GLOBAL], &PUMP_PROGRAM_ID)
}

/// Program-wide mint authority PDA (`["mint-authority"]`).
///
/// # Errors
/// See [`find_pda`].
pub fn mint_authority() -> Result<Pubkey, BuilderError> {
    pda_address("mint_authority", &[SEED_MINT_AUTHORITY], &PUMP_PROGRAM_ID)
}

/// Anchor `#[event_cpi]` authority of the Pump program.
///
/// # Errors
/// See [`find_pda`].
pub fn event_authority() -> Result<Pubkey, BuilderError> {
    pda_address("event_authority", &[SEED_EVENT_AUTHORITY], &PUMP_PROGRAM_ID)
}

/// Bonding curve PDA of `mint` (`["bonding-curve", mint]`).
///
/// # Errors
/// See [`find_pda`].
pub fn bonding_curve(mint: &Pubkey) -> Result<Pubkey, BuilderError> {
    let mint_bytes = mint.to_bytes();
    pda_address(
        "bonding_curve",
        &[SEED_BONDING_CURVE, mint_bytes.as_slice()],
        &PUMP_PROGRAM_ID,
    )
}

/// Associated token account of `owner` for `mint` under `token_program`.
///
/// This is also how the **Associated Bonding Curve (ABC)** is derived: the ATA
/// whose owner is the bonding curve PDA.
///
/// # Errors
/// See [`find_pda`].
pub fn associated_token_address(
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<Pubkey, BuilderError> {
    let owner_bytes = owner.to_bytes();
    let program_bytes = token_program.to_bytes();
    let mint_bytes = mint.to_bytes();
    pda_address(
        "associated_token_account",
        &[
            owner_bytes.as_slice(),
            program_bytes.as_slice(),
            mint_bytes.as_slice(),
        ],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
}

/// Associated Bonding Curve: the bonding curve's token account holding the
/// tokens still for sale.
///
/// # Errors
/// See [`find_pda`].
pub fn associated_bonding_curve(
    bonding_curve: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<Pubkey, BuilderError> {
    associated_token_address(bonding_curve, mint, token_program)
}

/// Metaplex Token Metadata PDA of `mint` (`["metadata", mpl_program, mint]`).
///
/// # Errors
/// See [`find_pda`].
pub fn metadata(mint: &Pubkey) -> Result<Pubkey, BuilderError> {
    let program_bytes = MPL_TOKEN_METADATA_PROGRAM_ID.to_bytes();
    let mint_bytes = mint.to_bytes();
    pda_address(
        "metadata",
        &[
            SEED_METADATA,
            program_bytes.as_slice(),
            mint_bytes.as_slice(),
        ],
        &MPL_TOKEN_METADATA_PROGRAM_ID,
    )
}

/// Creator fee vault (`["creator-vault", creator]`).
///
/// # Errors
/// See [`find_pda`].
pub fn creator_vault(creator: &Pubkey) -> Result<Pubkey, BuilderError> {
    let creator_bytes = creator.to_bytes();
    pda_address(
        "creator_vault",
        &[SEED_CREATOR_VAULT, creator_bytes.as_slice()],
        &PUMP_PROGRAM_ID,
    )
}

/// Per-user volume accumulator (`["user_volume_accumulator", user]`).
///
/// # Errors
/// See [`find_pda`].
pub fn user_volume_accumulator(user: &Pubkey) -> Result<Pubkey, BuilderError> {
    let user_bytes = user.to_bytes();
    pda_address(
        "user_volume_accumulator",
        &[SEED_USER_VOLUME_ACCUMULATOR, user_bytes.as_slice()],
        &PUMP_PROGRAM_ID,
    )
}

/// Global volume accumulator.
///
/// # Errors
/// See [`find_pda`].
pub fn global_volume_accumulator() -> Result<Pubkey, BuilderError> {
    pda_address(
        "global_volume_accumulator",
        &[SEED_GLOBAL_VOLUME_ACCUMULATOR],
        &PUMP_PROGRAM_ID,
    )
}

/// Fee config PDA, owned by the Pump fee program (`["fee_config", pump_program]`).
///
/// # Errors
/// See [`find_pda`].
pub fn fee_config() -> Result<Pubkey, BuilderError> {
    let pump_bytes = PUMP_PROGRAM_ID.to_bytes();
    pda_address(
        "fee_config",
        &[SEED_FEE_CONFIG, pump_bytes.as_slice()],
        &PUMP_FEES_PROGRAM_ID,
    )
}

/// Mayhem program global params PDA.
///
/// # Errors
/// See [`find_pda`].
pub fn mayhem_global_params() -> Result<Pubkey, BuilderError> {
    pda_address(
        "mayhem_global_params",
        &[SEED_MAYHEM_GLOBAL_PARAMS],
        &MAYHEM_PROGRAM_ID,
    )
}

/// Mayhem program SOL vault PDA.
///
/// # Errors
/// See [`find_pda`].
pub fn mayhem_sol_vault() -> Result<Pubkey, BuilderError> {
    pda_address(
        "mayhem_sol_vault",
        &[SEED_MAYHEM_SOL_VAULT],
        &MAYHEM_PROGRAM_ID,
    )
}

/// Mayhem state PDA of `mint` (`["mayhem-state", mint]`).
///
/// # Errors
/// See [`find_pda`].
pub fn mayhem_state(mint: &Pubkey) -> Result<Pubkey, BuilderError> {
    let mint_bytes = mint.to_bytes();
    pda_address(
        "mayhem_state",
        &[SEED_MAYHEM_STATE, mint_bytes.as_slice()],
        &MAYHEM_PROGRAM_ID,
    )
}

/// Every address a Create + Buy launch touches, derived once.
///
/// All fields are public, read-only data: plain addresses, no secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchAddresses {
    /// Creation instruction variant these addresses were derived for.
    pub variant: CreateVariant,
    /// Token program owning the mint (`Token-2022` or legacy SPL Token).
    pub token_program: Pubkey,
    /// The new mint (a fresh keypair, not a PDA).
    pub mint: Pubkey,
    /// Fee payer, creator and buyer.
    pub user: Pubkey,
    /// Program-wide mint authority PDA.
    pub mint_authority: Pubkey,
    /// `Global` configuration PDA.
    pub global: Pubkey,
    /// Anchor event authority PDA.
    pub event_authority: Pubkey,
    /// Bonding curve PDA of the mint.
    pub bonding_curve: Pubkey,
    /// Associated Bonding Curve (token account of the curve for the mint).
    pub associated_bonding_curve: Pubkey,
    /// Metaplex metadata PDA (used by the legacy `create` only).
    pub metadata: Pubkey,
    /// Mayhem global params PDA (`create_v2` only).
    pub mayhem_global_params: Pubkey,
    /// Mayhem SOL vault PDA (`create_v2` only).
    pub mayhem_sol_vault: Pubkey,
    /// Mayhem state PDA of the mint (`create_v2` only).
    pub mayhem_state: Pubkey,
    /// Mayhem token vault: ATA of the mint owned by the mayhem SOL vault.
    pub mayhem_token_vault: Pubkey,
    /// The buyer's token account for the mint.
    pub associated_user: Pubkey,
    /// Bonding curve's wSOL token account (`buy_v3`; derived but unused for SOL).
    pub associated_quote_bonding_curve: Pubkey,
    /// Buyer's wSOL token account (`buy_v3`; derived but unused for SOL).
    pub associated_quote_user: Pubkey,
    /// Buyer's volume accumulator PDA.
    pub user_volume_accumulator: Pubkey,
    /// Fee config PDA (fee program).
    pub fee_config: Pubkey,
}

impl LaunchAddresses {
    /// Derives every address for `mint` created and bought by `user`.
    ///
    /// # Errors
    /// [`BuilderError::PdaDerivationFailed`] if any derivation fails.
    pub fn derive(
        mint: &Pubkey,
        user: &Pubkey,
        variant: CreateVariant,
    ) -> Result<Self, BuilderError> {
        let token_program = variant.token_program();
        let curve = bonding_curve(mint)?;
        let sol_vault = mayhem_sol_vault()?;
        Ok(Self {
            variant,
            token_program,
            mint: *mint,
            user: *user,
            mint_authority: mint_authority()?,
            global: global()?,
            event_authority: event_authority()?,
            bonding_curve: curve,
            associated_bonding_curve: associated_bonding_curve(&curve, mint, &token_program)?,
            metadata: metadata(mint)?,
            mayhem_global_params: mayhem_global_params()?,
            mayhem_sol_vault: sol_vault,
            mayhem_state: mayhem_state(mint)?,
            mayhem_token_vault: associated_token_address(&sol_vault, mint, &TOKEN_2022_PROGRAM_ID)?,
            associated_user: associated_token_address(user, mint, &token_program)?,
            associated_quote_bonding_curve: associated_token_address(
                &curve,
                &WSOL_MINT,
                &TOKEN_PROGRAM_ID,
            )?,
            associated_quote_user: associated_token_address(user, &WSOL_MINT, &TOKEN_PROGRAM_ID)?,
            user_volume_accumulator: user_volume_accumulator(user)?,
            fee_config: fee_config()?,
        })
    }
}

/// Stable, launch-independent accounts worth storing in an Address Lookup Table.
///
/// Loading these through an ALT instead of the static account list saves 31
/// bytes per account, which is what makes room for the Jito tip (Phase 4) and
/// for longer metadata URIs. Programs invoked directly by the transaction
/// (Pump, ATA, Compute Budget) cannot be loaded from a table and are therefore
/// not included.
///
/// # Errors
/// [`BuilderError::PdaDerivationFailed`] if any derivation fails.
pub fn lookup_table_addresses() -> Result<Vec<Pubkey>, BuilderError> {
    Ok(vec![
        global()?,
        mint_authority()?,
        event_authority()?,
        fee_config()?,
        mayhem_global_params()?,
        mayhem_sol_vault()?,
        super::constants::SYSTEM_PROGRAM_ID,
        TOKEN_PROGRAM_ID,
        TOKEN_2022_PROGRAM_ID,
        WSOL_MINT,
        MAYHEM_PROGRAM_ID,
        PUMP_FEES_PROGRAM_ID,
        super::constants::RENT_SYSVAR_ID,
        MPL_TOKEN_METADATA_PROGRAM_ID,
    ])
}
