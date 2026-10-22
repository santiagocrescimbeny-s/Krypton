//! On-chain constants used by the atomic launch builder.
//!
//! Every value below was checked against the official Pump.fun public IDL and
//! documentation (`pump-fun/pump-public-docs`, `idl/pump.json`, `docs/`):
//!
//! - program ids, PDA seeds and instruction discriminators come from the IDL;
//! - the buyback fee recipients come from `docs/FEE_RECIPIENTS.md`;
//! - the initial curve reserves come from `docs/PUMP_PROGRAM_README.md`.
//!
//! Instruction discriminators are `sha256("global:<name>")[..8]` (Anchor) and
//! are pinned by tests, so an accidental edit is caught immediately.

use solana_pubkey::Pubkey;

// --- Programs ------------------------------------------------------------------

/// Pump.fun bonding-curve program.
pub const PUMP_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
/// Pump fee program (owner of the `fee_config` account).
pub const PUMP_FEES_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
/// Pump "mayhem mode" program (its accounts are required by `create_v2`).
pub const MAYHEM_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e");
/// Metaplex Token Metadata program (legacy `create` only).
pub const MPL_TOKEN_METADATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");
/// SPL Associated Token Account program.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// Legacy SPL Token program.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022 program (mints created by `create_v2`).
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// System program.
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
/// Rent sysvar (legacy `create` only).
pub const RENT_SYSVAR_ID: Pubkey =
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");
/// Compute Budget program.
pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
/// Wrapped SOL mint: the quote mint of every SOL-paired Pump.fun coin.
pub const WSOL_MINT: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");

// --- PDA seeds -----------------------------------------------------------------

/// Seed of the `Global` configuration account.
pub const SEED_GLOBAL: &[u8] = b"global";
/// Seed of the mint-authority PDA (the authority of every Pump.fun mint).
pub const SEED_MINT_AUTHORITY: &[u8] = b"mint-authority";
/// Seed of a coin's bonding curve (`["bonding-curve", mint]`).
pub const SEED_BONDING_CURVE: &[u8] = b"bonding-curve";
/// Seed of the Anchor `#[event_cpi]` authority.
pub const SEED_EVENT_AUTHORITY: &[u8] = b"__event_authority";
/// Seed of a creator's fee vault (`["creator-vault", creator]`).
pub const SEED_CREATOR_VAULT: &[u8] = b"creator-vault";
/// Seed of a user's volume accumulator (`["user_volume_accumulator", user]`).
pub const SEED_USER_VOLUME_ACCUMULATOR: &[u8] = b"user_volume_accumulator";
/// Seed of the global volume accumulator.
pub const SEED_GLOBAL_VOLUME_ACCUMULATOR: &[u8] = b"global_volume_accumulator";
/// Seed of the fee config PDA (`["fee_config", pump_program_id]`, fee program).
pub const SEED_FEE_CONFIG: &[u8] = b"fee_config";
/// Seed of the Metaplex metadata PDA (`["metadata", mpl_program, mint]`).
pub const SEED_METADATA: &[u8] = b"metadata";
/// Seed of the mayhem program's global params PDA.
pub const SEED_MAYHEM_GLOBAL_PARAMS: &[u8] = b"global-params";
/// Seed of the mayhem program's SOL vault PDA.
pub const SEED_MAYHEM_SOL_VAULT: &[u8] = b"sol-vault";
/// Seed of a coin's mayhem state PDA (`["mayhem-state", mint]`).
pub const SEED_MAYHEM_STATE: &[u8] = b"mayhem-state";

// --- Instruction discriminators ---------------------------------------------------

/// Legacy `create` (SPL Token mint + Metaplex metadata).
pub const DISCRIMINATOR_CREATE: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
/// `create_v2` (Token-2022 mint, metadata pointer, mayhem accounts).
pub const DISCRIMINATOR_CREATE_V2: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];
/// `buy_v3` (17-account buy; fees stay on the curve until swept).
pub const DISCRIMINATOR_BUY_V3: [u8; 8] = [7, 5, 29, 196, 245, 23, 101, 80];
/// `sell_v3` (same 17 accounts as `buy_v3`; arguments `amount`, `min_sol_output`).
pub const DISCRIMINATOR_SELL_V3: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];

/// Anchor account discriminator of `BondingCurve` (`sha256("account:BondingCurve")[..8]`).
pub const DISCRIMINATOR_BONDING_CURVE_ACCOUNT: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

/// Every instruction that buys from the curve (legacy, v2 and v3 families).
pub const DISCRIMINATORS_BUY: [[u8; 8]; 6] = [
    [102, 6, 61, 18, 1, 218, 235, 234],     // buy
    [184, 23, 238, 97, 103, 197, 211, 61],  // buy_v2
    [7, 5, 29, 196, 245, 23, 101, 80],      // buy_v3
    [56, 252, 116, 8, 158, 223, 205, 95],   // buy_exact_sol_in
    [194, 171, 28, 70, 104, 77, 91, 47],    // buy_exact_quote_in_v2
    [225, 247, 80, 30, 213, 179, 132, 136], // buy_exact_quote_in_v3
];
/// Every instruction that sells to the curve (`sell`, `sell_v2`, `sell_v3`).
pub const DISCRIMINATORS_SELL: [[u8; 8]; 3] = [
    [51, 230, 133, 164, 1, 127, 131, 173],
    [93, 246, 130, 60, 231, 233, 64, 178],
    [28, 146, 222, 119, 38, 196, 105, 213],
];

/// Compute Budget `SetComputeUnitLimit` tag.
pub const COMPUTE_BUDGET_SET_UNIT_LIMIT: u8 = 2;
/// Compute Budget `SetComputeUnitPrice` tag.
pub const COMPUTE_BUDGET_SET_UNIT_PRICE: u8 = 3;
/// Associated Token Account `CreateIdempotent` tag.
pub const ATA_CREATE_IDEMPOTENT: u8 = 1;

// --- Bonding curve defaults ----------------------------------------------------------

/// Decimals of every Pump.fun coin.
pub const TOKEN_DECIMALS: u8 = 6;
/// Initial virtual token reserves (1,073,000,000 tokens, base units).
pub const INITIAL_VIRTUAL_TOKEN_RESERVES: u64 = 1_073_000_000_000_000;
/// Initial virtual SOL reserves (30 SOL, lamports).
pub const INITIAL_VIRTUAL_SOL_RESERVES: u64 = 30_000_000_000;
/// Initial real token reserves (793,100,000 tokens, base units).
pub const INITIAL_REAL_TOKEN_RESERVES: u64 = 793_100_000_000_000;
/// Total supply (1,000,000,000 tokens, base units).
pub const TOKEN_TOTAL_SUPPLY: u64 = 1_000_000_000_000_000;

// --- Validation limits ---------------------------------------------------------------

/// Maximum `name` length accepted by the program (bytes; stricter than chars).
pub const MAX_NAME_BYTES: usize = 32;
/// Maximum `symbol` length accepted by `create_v2` (bytes).
pub const MAX_SYMBOL_BYTES: usize = 13;
/// Maximum `uri` length accepted by the program (bytes).
pub const MAX_URI_BYTES: usize = 200;
/// Maximum size of a serialized transaction (Solana packet data size).
pub const MAX_TRANSACTION_SIZE: usize = 1232;

// --- Rent and fee reserves (lamports) ---------------------------------------------
//
// Used by `LaunchParams::required_balance`. Rent-exempt minimum is
// `(data_len + 128) * 6960` lamports.

/// Rent of a legacy SPL token account (165 bytes).
pub const TOKEN_ACCOUNT_RENT_LEGACY_LAMPORTS: u64 = 2_039_280;
/// Rent of a Token-2022 associated token account (170 bytes: 165 + account type
/// + the `ImmutableOwner` extension that the ATA program always adds).
pub const TOKEN_ACCOUNT_RENT_2022_LAMPORTS: u64 = 2_074_080;
/// Rent of the buyer's `user_volume_accumulator` (137 bytes), created by the
/// first trade of a wallet. Conservatively always reserved.
pub const USER_VOLUME_ACCUMULATOR_RENT_LAMPORTS: u64 = 1_844_400;
/// Base network fee per signature.
pub const BASE_SIGNATURE_FEE_LAMPORTS: u64 = 5_000;
/// Signatures of a launch: the fee payer and the new mint.
pub const LAUNCH_SIGNATURE_COUNT: u64 = 2;
/// Rent-exempt minimum of a data-less system account. The fee payer must not
/// end the transaction with a non-zero balance below it.
pub const PAYER_RENT_EXEMPT_RESERVE_LAMPORTS: u64 = 890_880;

// --- Fee recipients ------------------------------------------------------------------

/// The eight official buyback fee recipient wallets (`docs/FEE_RECIPIENTS.md`).
///
/// `buy_v3` on a SOL-paired coin takes any one of them as `buyback_fee_recipient`.
pub const BUYBACK_FEE_RECIPIENTS: [Pubkey; 8] = [
    Pubkey::from_str_const("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
    Pubkey::from_str_const("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7"),
    Pubkey::from_str_const("GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL"),
    Pubkey::from_str_const("3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR"),
    Pubkey::from_str_const("5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6"),
    Pubkey::from_str_const("EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL"),
    Pubkey::from_str_const("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD"),
    Pubkey::from_str_const("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW"),
];

/// Fallback buyback recipient (entry 0 of [`BUYBACK_FEE_RECIPIENTS`]).
pub const DEFAULT_BUYBACK_FEE_RECIPIENT: Pubkey =
    Pubkey::from_str_const("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD");

/// Picks a buyback recipient deterministically from the mint, spreading load
/// across the eight official wallets. Any of them is valid.
#[must_use]
pub fn select_buyback_fee_recipient(mint: &Pubkey) -> Pubkey {
    let first = mint.to_bytes().first().copied().unwrap_or(0);
    let index = usize::from(first) % BUYBACK_FEE_RECIPIENTS.len();
    BUYBACK_FEE_RECIPIENTS
        .get(index)
        .copied()
        .unwrap_or(DEFAULT_BUYBACK_FEE_RECIPIENT)
}
