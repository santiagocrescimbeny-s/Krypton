//! Atomic launch transaction: `[compute budget, ATA, Create, Buy]` packed into a
//! single `v0` [`VersionedTransaction`].
//!
//! Because `Create` and `Buy` share one transaction, the creator's buy lands in
//! the very block that creates the coin: nobody can buy in between.
//!
//! # Size budget
//! A Solana packet holds 1232 bytes. The base launch (about 25 static
//! accounts) takes roughly 1200 bytes with a typical IPFS URI, so there is very
//! little room left. [`LaunchBuilder::compile`] measures the exact wire size
//! and fails with [`BuilderError::TransactionTooLarge`] instead of producing a
//! transaction the network would drop. To make room (a long URI, or the Jito tip
//! added in Phase 4) pass an Address Lookup Table that holds the addresses
//! returned by [`super::pda::lookup_table_addresses`]; that shrinks the
//! transaction to about 900 bytes.
//!
//! # Key hygiene
//! The wallet keypair and the mint keypair are materialized only inside
//! [`LaunchBuilder::sign`] and dropped right after signing. The mint's seed is
//! stored in [`MintSigner`] as `Zeroizing<[u8; 32]>`, and every intermediate
//! secret buffer created here is wrapped in `Zeroizing`.

use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_sdk::transaction::VersionedTransaction;
use zeroize::Zeroizing;

use super::constants::{
    select_buyback_fee_recipient, BASE_SIGNATURE_FEE_LAMPORTS, LAUNCH_SIGNATURE_COUNT,
    MAX_TRANSACTION_SIZE, PAYER_RENT_EXEMPT_RESERVE_LAMPORTS, TOKEN_ACCOUNT_RENT_2022_LAMPORTS,
    TOKEN_ACCOUNT_RENT_LEGACY_LAMPORTS, USER_VOLUME_ACCUMULATOR_RENT_LAMPORTS,
};
use super::instructions::{
    buy_v3_instruction, create_associated_token_account_idempotent, create_instruction,
    set_compute_unit_limit, set_compute_unit_price, CreateOptions, LaunchMetadata,
};
use super::math::{BuyPlan, CurveParams, DEFAULT_TOTAL_FEE_BPS, MAX_SLIPPAGE_BPS};
use super::pda::{CreateVariant, LaunchAddresses};
use crate::config::{priority_fee_lamports, Config, ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS};
use crate::errors::BuilderError;
use crate::security::SecureWallet;

/// Length of an Ed25519 signature on the wire.
const SIGNATURE_BYTES: usize = 64;
/// Length of a public key on the wire.
const PUBKEY_BYTES: usize = 32;
/// Length of the recent blockhash on the wire.
const BLOCKHASH_BYTES: usize = 32;
/// Version prefix byte of a `v0` message.
const VERSION_PREFIX_BYTES: usize = 1;
/// Message header length (three counters).
const HEADER_BYTES: usize = 3;

/// The ephemeral keypair of the new mint.
///
/// Only the 32-byte seed is kept, in a `Zeroizing` buffer that is wiped on
/// drop. The public address is public data and stored plainly.
pub struct MintSigner {
    seed: Zeroizing<[u8; 32]>,
    pubkey: Pubkey,
}

impl MintSigner {
    /// Generates a fresh random mint keypair.
    #[must_use]
    pub fn generate() -> Self {
        let keypair = Keypair::new();
        let bytes = Zeroizing::new(keypair.to_bytes());
        Self::from_keypair_bytes(&bytes)
    }

    /// Builds a signer from a fixed 32-byte seed (deterministic; used by tests
    /// and by callers that pre-generate vanity mints).
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let seed = Zeroizing::new(seed);
        let keypair = Keypair::new_from_array(*seed);
        let bytes = Zeroizing::new(keypair.to_bytes());
        Self::from_keypair_bytes(&bytes)
    }

    /// Splits the 64 keypair bytes into the seed (kept, zeroizing) and the
    /// public key (kept, plain).
    fn from_keypair_bytes(bytes: &Zeroizing<[u8; 64]>) -> Self {
        let raw: &[u8; 64] = bytes;
        let mut seed = Zeroizing::new([0u8; 32]);
        for (dst, src) in seed.iter_mut().zip(raw.iter()) {
            *dst = *src;
        }
        let mut public = [0u8; 32];
        for (dst, src) in public.iter_mut().zip(raw.iter().skip(32)) {
            *dst = *src;
        }
        Self {
            seed,
            pubkey: Pubkey::new_from_array(public),
        }
    }

    /// Address of the mint.
    #[must_use]
    pub fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    /// Short-lived keypair for signing. Drop it immediately after use.
    fn keypair(&self) -> Keypair {
        Keypair::new_from_array(*self.seed)
    }
}

impl std::fmt::Debug for MintSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintSigner")
            .field("seed", &"<redacted>")
            .finish()
    }
}

/// Everything needed to describe one atomic launch.
#[derive(Debug, Clone)]
pub struct LaunchParams {
    /// Name, symbol and metadata URI.
    pub metadata: LaunchMetadata,
    /// `create_v2` (default) or the legacy `create`.
    pub variant: CreateVariant,
    /// Optional `create_v2` flags.
    pub options: CreateOptions,
    /// Creator recorded on chain; defaults to the fee payer when `None`.
    pub creator: Option<Pubkey>,
    /// The resolved initial buy.
    pub buy: BuyPlan,
    /// Compute unit limit.
    pub compute_unit_limit: u32,
    /// Priority fee, in micro-lamports per compute unit.
    pub priority_fee_micro_lamports: u64,
    /// Hard cap on the priority fee actually paid (`price x compute unit
    /// limit`), in lamports. [`LaunchBuilder::new`] refuses anything above it,
    /// and never accepts more than [`ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS`].
    pub max_priority_fee_lamports: u64,
    /// Buyback fee recipient; derived from the mint when `None`.
    pub buyback_fee_recipient: Option<Pubkey>,
}

/// What the payer holds and what the launch needs beyond the curve itself.
///
/// Both extras are supplied by the caller on purpose: the rent of the accounts
/// created by `Create` is best measured with the Phase 4 simulation (never
/// hard-coded), and the Jito tip is only known once the bundle is assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FundingSnapshot {
    /// Current lamport balance of the payer (from `getBalance`).
    pub available_lamports: u64,
    /// Rent of the accounts `Create` allocates (mint, bonding curve, ...).
    pub create_rent_lamports: u64,
    /// Jito tip that will be appended to the bundle (0 when none).
    pub tip_lamports: u64,
}

impl LaunchParams {
    /// Builds parameters from the runtime [`Config`]: the creator buy budget
    /// (`CREATOR_SOL_AMOUNT`), slippage and compute settings come from it.
    ///
    /// `curve` and `fee_bps` default to [`CurveParams::default`] and
    /// [`DEFAULT_TOTAL_FEE_BPS`] when the caller has nothing better.
    ///
    /// # Errors
    /// See [`BuyPlan::from_budget`].
    pub fn from_config(
        config: &Config,
        metadata: LaunchMetadata,
        curve: &CurveParams,
        fee_bps: u16,
    ) -> Result<Self, BuilderError> {
        let buy = BuyPlan::from_budget(
            curve,
            config.creator_buy_lamports,
            fee_bps,
            config.slippage_bps,
        )?;
        Ok(Self {
            metadata,
            variant: CreateVariant::default(),
            options: CreateOptions::default(),
            creator: None,
            buy,
            compute_unit_limit: config.compute_unit_limit,
            priority_fee_micro_lamports: config.priority_fee_micro_lamports,
            max_priority_fee_lamports: config.max_priority_fee_lamports,
            buyback_fee_recipient: None,
        })
    }

    /// Priority fee this launch pays, in lamports (`price x CU limit`, rounded up).
    #[must_use]
    pub fn priority_fee_lamports(&self) -> u128 {
        priority_fee_lamports(self.priority_fee_micro_lamports, self.compute_unit_limit)
    }

    /// Worst-case lamports the payer must hold before the transaction is sent.
    ///
    /// The sum of: the buy's `max_sol_cost` (slippage included), the rent of the
    /// buyer's token account and `user_volume_accumulator`, the base signature
    /// fees, the priority fee, `create_rent_lamports`, `tip_lamports`, and the
    /// rent-exempt minimum the payer account must keep afterwards.
    ///
    /// # Errors
    /// [`BuilderError::ArithmeticOverflow`] if any term or the sum overflows.
    pub fn required_balance(
        &self,
        create_rent_lamports: u64,
        tip_lamports: u64,
    ) -> Result<u64, BuilderError> {
        let priority = u64::try_from(self.priority_fee_lamports()).map_err(|_| {
            BuilderError::ArithmeticOverflow {
                context: "priority fee",
            }
        })?;
        let signature_fees = BASE_SIGNATURE_FEE_LAMPORTS
            .checked_mul(LAUNCH_SIGNATURE_COUNT)
            .ok_or(BuilderError::ArithmeticOverflow {
                context: "signature fees",
            })?;
        let token_account_rent = match self.variant {
            CreateVariant::CreateV2 => TOKEN_ACCOUNT_RENT_2022_LAMPORTS,
            CreateVariant::LegacyCreate => TOKEN_ACCOUNT_RENT_LEGACY_LAMPORTS,
        };
        [
            self.buy.max_sol_cost_lamports,
            token_account_rent,
            USER_VOLUME_ACCUMULATOR_RENT_LAMPORTS,
            signature_fees,
            priority,
            create_rent_lamports,
            tip_lamports,
            PAYER_RENT_EXEMPT_RESERVE_LAMPORTS,
        ]
        .into_iter()
        .try_fold(0u64, u64::checked_add)
        .ok_or(BuilderError::ArithmeticOverflow {
            context: "required balance",
        })
    }

    /// Same as [`LaunchParams::from_config`] with the default curve and fee.
    ///
    /// # Errors
    /// See [`BuyPlan::from_budget`].
    pub fn from_config_defaults(
        config: &Config,
        metadata: LaunchMetadata,
    ) -> Result<Self, BuilderError> {
        Self::from_config(
            config,
            metadata,
            &CurveParams::default(),
            DEFAULT_TOTAL_FEE_BPS,
        )
    }
}

/// A compiled (unsigned) launch message plus its measured wire size.
#[derive(Debug, Clone)]
pub struct CompiledLaunch {
    /// The `v0` message, ready to sign.
    pub message: v0::Message,
    /// Exact serialized size of the signed transaction, in bytes.
    pub wire_size: usize,
}

/// Builds and signs the atomic launch transaction.
#[derive(Debug, Clone)]
pub struct LaunchBuilder {
    payer: Pubkey,
    creator: Pubkey,
    params: LaunchParams,
    addresses: LaunchAddresses,
    buyback_fee_recipient: Pubkey,
}

impl LaunchBuilder {
    /// Derives all addresses and validates the parameters.
    ///
    /// `payer` is the wallet that pays, signs and buys; `mint` is the public
    /// address of the new [`MintSigner`].
    ///
    /// # Errors
    /// - [`BuilderError::PdaDerivationFailed`] from address derivation;
    /// - [`BuilderError::UnsupportedOption`] if the legacy variant is combined
    ///   with `create_v2`-only options.
    pub fn new(payer: Pubkey, mint: Pubkey, params: LaunchParams) -> Result<Self, BuilderError> {
        // Safety guards that hold no matter how `LaunchParams` was assembled.
        if params.buy.slippage_bps > MAX_SLIPPAGE_BPS {
            return Err(BuilderError::InvalidSlippage {
                bps: params.buy.slippage_bps,
                max: MAX_SLIPPAGE_BPS,
            });
        }
        let fee_cap = params
            .max_priority_fee_lamports
            .min(ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS);
        let fee = params.priority_fee_lamports();
        if fee > u128::from(fee_cap) {
            return Err(BuilderError::PriorityFeeTooHigh {
                fee_lamports: fee,
                max: fee_cap,
            });
        }
        if params.variant == CreateVariant::LegacyCreate
            && params.options != CreateOptions::default()
        {
            return Err(BuilderError::UnsupportedOption {
                reason: "mayhem and holder-reward modes require create_v2",
            });
        }
        let addresses = LaunchAddresses::derive(&mint, &payer, params.variant)?;
        let creator = params.creator.unwrap_or(payer);
        let buyback_fee_recipient = params
            .buyback_fee_recipient
            .unwrap_or_else(|| select_buyback_fee_recipient(&mint));
        Ok(Self {
            payer,
            creator,
            params,
            addresses,
            buyback_fee_recipient,
        })
    }

    /// All derived addresses of this launch.
    #[must_use]
    pub fn addresses(&self) -> &LaunchAddresses {
        &self.addresses
    }

    /// The launch parameters.
    #[must_use]
    pub fn params(&self) -> &LaunchParams {
        &self.params
    }

    /// Pre-flight balance check: the payer must hold the worst-case cost of the
    /// launch (see [`LaunchParams::required_balance`]). Returns the required
    /// amount on success so callers can log it.
    ///
    /// # Errors
    /// - [`BuilderError::InsufficientBalance`] when `available_lamports` is lower;
    /// - [`BuilderError::ArithmeticOverflow`] from the cost computation.
    pub fn ensure_funded(&self, funding: &FundingSnapshot) -> Result<u64, BuilderError> {
        let required = self
            .params
            .required_balance(funding.create_rent_lamports, funding.tip_lamports)?;
        if funding.available_lamports < required {
            return Err(BuilderError::InsufficientBalance {
                required,
                available: funding.available_lamports,
            });
        }
        Ok(required)
    }

    /// The ordered instruction list:
    /// `SetComputeUnitLimit`, `SetComputeUnitPrice`, `CreateIdempotent` (buyer
    /// token account), `Create`/`create_v2`, `buy_v3`.
    ///
    /// # Errors
    /// See [`create_instruction`].
    pub fn instructions(&self) -> Result<Vec<Instruction>, BuilderError> {
        let a = &self.addresses;
        Ok(vec![
            set_compute_unit_limit(self.params.compute_unit_limit),
            set_compute_unit_price(self.params.priority_fee_micro_lamports),
            create_associated_token_account_idempotent(
                &self.payer,
                &a.associated_user,
                &self.payer,
                &a.mint,
                &a.token_program,
            ),
            create_instruction(a, &self.creator, &self.params.metadata, self.params.options)?,
            buy_v3_instruction(a, &self.params.buy, &self.buyback_fee_recipient),
        ])
    }

    /// Compiles the instructions into a `v0` message.
    ///
    /// `trailing` instructions (for example the Jito tip, Phase 4) are appended
    /// after `Buy`. Fails if the signed transaction would exceed
    /// [`MAX_TRANSACTION_SIZE`].
    ///
    /// # Errors
    /// - [`BuilderError::MessageCompile`] if compilation fails;
    /// - [`BuilderError::TransactionTooLarge`] if it does not fit a packet.
    pub fn compile(
        &self,
        recent_blockhash: Hash,
        lookup_tables: &[AddressLookupTableAccount],
        trailing: &[Instruction],
    ) -> Result<CompiledLaunch, BuilderError> {
        let mut instructions = self.instructions()?;
        instructions.extend_from_slice(trailing);
        let message =
            v0::Message::try_compile(&self.payer, &instructions, lookup_tables, recent_blockhash)
                .map_err(|err| BuilderError::MessageCompile(err.to_string()))?;
        let wire_size = estimate_wire_size(&message);
        if wire_size > MAX_TRANSACTION_SIZE {
            return Err(BuilderError::TransactionTooLarge {
                size: wire_size,
                max: MAX_TRANSACTION_SIZE,
            });
        }
        Ok(CompiledLaunch { message, wire_size })
    }

    /// Signs a compiled message with the wallet and the mint keypair.
    ///
    /// Both keypairs are created here and dropped before returning.
    ///
    /// # Errors
    /// - [`BuilderError::PayerMismatch`] / [`BuilderError::MintSignerMismatch`]
    ///   if the signers do not belong to this launch;
    /// - [`BuilderError::Signing`] if the signature fails.
    pub fn sign(
        &self,
        message: v0::Message,
        wallet: &SecureWallet,
        mint: &MintSigner,
    ) -> Result<VersionedTransaction, BuilderError> {
        if wallet.pubkey() != self.payer {
            return Err(BuilderError::PayerMismatch);
        }
        if mint.pubkey() != self.addresses.mint {
            return Err(BuilderError::MintSignerMismatch);
        }
        let wallet_keypair = wallet.keypair();
        let mint_keypair = mint.keypair();
        VersionedTransaction::try_new(
            VersionedMessage::V0(message),
            &[&wallet_keypair, &mint_keypair],
        )
        .map_err(|err| BuilderError::Signing(err.to_string()))
    }

    /// Like [`LaunchBuilder::build_signed`], but runs
    /// [`LaunchBuilder::ensure_funded`] first: no message is assembled and no key
    /// is touched unless the payer can afford the launch.
    ///
    /// # Errors
    /// See [`LaunchBuilder::ensure_funded`], [`LaunchBuilder::compile`] and
    /// [`LaunchBuilder::sign`].
    pub fn build_signed_checked(
        &self,
        funding: &FundingSnapshot,
        recent_blockhash: Hash,
        lookup_tables: &[AddressLookupTableAccount],
        trailing: &[Instruction],
        wallet: &SecureWallet,
        mint: &MintSigner,
    ) -> Result<VersionedTransaction, BuilderError> {
        self.ensure_funded(funding)?;
        self.build_signed(recent_blockhash, lookup_tables, trailing, wallet, mint)
    }

    /// Convenience: [`LaunchBuilder::compile`] followed by [`LaunchBuilder::sign`].
    ///
    /// # Errors
    /// See [`LaunchBuilder::compile`] and [`LaunchBuilder::sign`].
    pub fn build_signed(
        &self,
        recent_blockhash: Hash,
        lookup_tables: &[AddressLookupTableAccount],
        trailing: &[Instruction],
        wallet: &SecureWallet,
        mint: &MintSigner,
    ) -> Result<VersionedTransaction, BuilderError> {
        let compiled = self.compile(recent_blockhash, lookup_tables, trailing)?;
        self.sign(compiled.message, wallet, mint)
    }
}

/// Number of bytes of Solana's `short_vec` length prefix for `len` elements.
fn short_vec_len(len: usize) -> usize {
    if len < 0x80 {
        1
    } else if len < 0x4000 {
        2
    } else {
        3
    }
}

/// Exact serialized size of the signed transaction carrying `message`.
///
/// Computed from the compiled structure (no serializer needed):
/// signatures (`short_vec` + 64 B each), version byte, header, account keys,
/// blockhash, compiled instructions and address-table lookups.
#[must_use]
pub fn estimate_wire_size(message: &v0::Message) -> usize {
    let signatures = usize::from(message.header.num_required_signatures);
    let mut size =
        short_vec_len(signatures).saturating_add(signatures.saturating_mul(SIGNATURE_BYTES));
    size = size
        .saturating_add(VERSION_PREFIX_BYTES)
        .saturating_add(HEADER_BYTES)
        .saturating_add(short_vec_len(message.account_keys.len()))
        .saturating_add(message.account_keys.len().saturating_mul(PUBKEY_BYTES))
        .saturating_add(BLOCKHASH_BYTES)
        .saturating_add(short_vec_len(message.instructions.len()));
    for instruction in &message.instructions {
        size = size
            .saturating_add(1) // program id index
            .saturating_add(short_vec_len(instruction.accounts.len()))
            .saturating_add(instruction.accounts.len())
            .saturating_add(short_vec_len(instruction.data.len()))
            .saturating_add(instruction.data.len());
    }
    size = size.saturating_add(short_vec_len(message.address_table_lookups.len()));
    for lookup in &message.address_table_lookups {
        size = size
            .saturating_add(PUBKEY_BYTES)
            .saturating_add(short_vec_len(lookup.writable_indexes.len()))
            .saturating_add(lookup.writable_indexes.len())
            .saturating_add(short_vec_len(lookup.readonly_indexes.len()))
            .saturating_add(lookup.readonly_indexes.len());
    }
    size
}
