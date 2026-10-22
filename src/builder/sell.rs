//! `sell_v3` transactions for the exit manager (Phase 5).
//!
//! A sell is a small `v0` transaction signed by the wallet only:
//! `[SetComputeUnitLimit, SetComputeUnitPrice, sell_v3]` plus optional trailing
//! instructions (the Jito tip). It needs no lookup table: 17 accounts plus the
//! compute-budget program stay far below the 1232-byte packet limit, and the
//! size is measured anyway.
//!
//! # Guards
//! - the priority fee (`price x compute unit limit`) obeys the same cap as the
//!   launch (audit finding #19);
//! - the slippage floor comes from a [`SellPlan`], which enforces the 10 %
//!   bound (audit finding #20);
//! - the wallet keypair is materialized only inside [`SellBuilder::sign`] and
//!   dropped right after signing.

use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::transaction::VersionedTransaction;

use super::constants::{
    select_buyback_fee_recipient, BASE_SIGNATURE_FEE_LAMPORTS, MAX_TRANSACTION_SIZE,
};
use super::instructions::{sell_v3_instruction, set_compute_unit_limit, set_compute_unit_price};
use super::math::SellPlan;
use super::pda::{CreateVariant, LaunchAddresses};
use super::transaction::estimate_wire_size;
use crate::config::{
    priority_fee_lamports, Config, ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS, MAX_COMPUTE_UNIT_LIMIT,
};
use crate::errors::BuilderError;
use crate::security::SecureWallet;

/// Compute unit limit requested by a sell unless configured lower: `sell_v3`
/// needs far less than a launch, and the priority fee scales with the limit.
pub const DEFAULT_SELL_COMPUTE_UNIT_LIMIT: u32 = 200_000;

/// Compute and fee settings of a sell transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SellSettings {
    /// Compute unit limit (1..=1_400_000).
    pub compute_unit_limit: u32,
    /// Priority fee, micro-lamports per compute unit.
    pub priority_fee_micro_lamports: u64,
    /// Hard cap on the priority fee actually paid, in lamports.
    pub max_priority_fee_lamports: u64,
    /// Buyback recipient override; `None` selects one from the mint.
    pub buyback_fee_recipient: Option<Pubkey>,
}

impl SellSettings {
    /// Settings derived from the runtime [`Config`]. The compute unit limit is
    /// the lower of the configured launch limit and
    /// [`DEFAULT_SELL_COMPUTE_UNIT_LIMIT`].
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            compute_unit_limit: config
                .compute_unit_limit
                .min(DEFAULT_SELL_COMPUTE_UNIT_LIMIT),
            priority_fee_micro_lamports: config.priority_fee_micro_lamports,
            max_priority_fee_lamports: config.max_priority_fee_lamports,
            buyback_fee_recipient: None,
        }
    }
}

/// A compiled, unsigned sell.
#[derive(Debug, Clone)]
pub struct CompiledSell {
    /// The `v0` message, ready to sign.
    pub message: v0::Message,
    /// Exact serialized size of the signed transaction, in bytes.
    pub wire_size: usize,
}

/// Builds and signs `sell_v3` transactions for one coin.
#[derive(Debug, Clone)]
pub struct SellBuilder {
    payer: Pubkey,
    addresses: LaunchAddresses,
    buyback_fee_recipient: Pubkey,
    settings: SellSettings,
}

impl SellBuilder {
    /// Derives the coin's addresses and validates `settings`.
    ///
    /// `payer` is the wallet that holds the tokens, signs and pays.
    ///
    /// # Errors
    /// - [`BuilderError::PdaDerivationFailed`] from address derivation;
    /// - [`BuilderError::InvalidCurve`] for a zero or oversized compute limit;
    /// - [`BuilderError::PriorityFeeTooHigh`] if `price x limit` exceeds the cap.
    pub fn new(
        payer: Pubkey,
        mint: Pubkey,
        variant: CreateVariant,
        settings: SellSettings,
    ) -> Result<Self, BuilderError> {
        if settings.compute_unit_limit == 0 || settings.compute_unit_limit > MAX_COMPUTE_UNIT_LIMIT
        {
            return Err(BuilderError::InvalidCurve {
                reason: "compute unit limit out of range",
            });
        }
        let cap = settings
            .max_priority_fee_lamports
            .min(ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS);
        let fee = priority_fee_lamports(
            settings.priority_fee_micro_lamports,
            settings.compute_unit_limit,
        );
        if fee > u128::from(cap) {
            return Err(BuilderError::PriorityFeeTooHigh {
                fee_lamports: fee,
                max: cap,
            });
        }
        let addresses = LaunchAddresses::derive(&mint, &payer, variant)?;
        let buyback_fee_recipient = settings
            .buyback_fee_recipient
            .unwrap_or_else(|| select_buyback_fee_recipient(&mint));
        Ok(Self {
            payer,
            addresses,
            buyback_fee_recipient,
            settings,
        })
    }

    /// The fee payer (also the seller).
    #[must_use]
    pub fn payer(&self) -> Pubkey {
        self.payer
    }

    /// All derived addresses of the coin.
    #[must_use]
    pub fn addresses(&self) -> &LaunchAddresses {
        &self.addresses
    }

    /// Priority fee each sell pays, in lamports (already checked against the cap).
    #[must_use]
    pub fn priority_fee_lamports(&self) -> u128 {
        priority_fee_lamports(
            self.settings.priority_fee_micro_lamports,
            self.settings.compute_unit_limit,
        )
    }

    /// Worst-case lamports one sell costs the payer besides the trade itself:
    /// the signature fee, the priority fee and `tip_lamports`.
    ///
    /// # Errors
    /// [`BuilderError::ArithmeticOverflow`] if the sum does not fit a `u64`.
    pub fn order_cost_lamports(&self, tip_lamports: u64) -> Result<u64, BuilderError> {
        let overflow = || BuilderError::ArithmeticOverflow {
            context: "sell order cost",
        };
        let priority = u64::try_from(self.priority_fee_lamports()).map_err(|_| overflow())?;
        BASE_SIGNATURE_FEE_LAMPORTS
            .checked_add(priority)
            .and_then(|sum| sum.checked_add(tip_lamports))
            .ok_or_else(overflow)
    }

    /// The ordered instruction list: `SetComputeUnitLimit`,
    /// `SetComputeUnitPrice`, `sell_v3`.
    #[must_use]
    pub fn instructions(&self, plan: &SellPlan) -> Vec<Instruction> {
        vec![
            set_compute_unit_limit(self.settings.compute_unit_limit),
            set_compute_unit_price(self.settings.priority_fee_micro_lamports),
            sell_v3_instruction(&self.addresses, plan, &self.buyback_fee_recipient),
        ]
    }

    /// Compiles the sell into a `v0` message; `trailing` instructions (the Jito
    /// tip) are appended after `sell_v3`.
    ///
    /// # Errors
    /// - [`BuilderError::MessageCompile`] if compilation fails;
    /// - [`BuilderError::TransactionTooLarge`] if it does not fit a packet.
    pub fn compile(
        &self,
        plan: &SellPlan,
        recent_blockhash: Hash,
        trailing: &[Instruction],
    ) -> Result<CompiledSell, BuilderError> {
        let mut instructions = self.instructions(plan);
        instructions.extend_from_slice(trailing);
        let message = v0::Message::try_compile(&self.payer, &instructions, &[], recent_blockhash)
            .map_err(|err| BuilderError::MessageCompile(err.to_string()))?;
        let wire_size = estimate_wire_size(&message);
        if wire_size > MAX_TRANSACTION_SIZE {
            return Err(BuilderError::TransactionTooLarge {
                size: wire_size,
                max: MAX_TRANSACTION_SIZE,
            });
        }
        Ok(CompiledSell { message, wire_size })
    }

    /// Signs a compiled message with the wallet. The keypair is created here and
    /// dropped before returning.
    ///
    /// # Errors
    /// - [`BuilderError::PayerMismatch`] if `wallet` is not the payer;
    /// - [`BuilderError::Signing`] if the signature fails.
    pub fn sign(
        &self,
        message: v0::Message,
        wallet: &SecureWallet,
    ) -> Result<VersionedTransaction, BuilderError> {
        if wallet.pubkey() != self.payer {
            return Err(BuilderError::PayerMismatch);
        }
        let keypair = wallet.keypair();
        VersionedTransaction::try_new(VersionedMessage::V0(message), &[&keypair])
            .map_err(|err| BuilderError::Signing(err.to_string()))
    }

    /// [`SellBuilder::compile`] followed by [`SellBuilder::sign`].
    ///
    /// # Errors
    /// See [`SellBuilder::compile`] and [`SellBuilder::sign`].
    pub fn build_signed(
        &self,
        plan: &SellPlan,
        recent_blockhash: Hash,
        trailing: &[Instruction],
        wallet: &SecureWallet,
    ) -> Result<VersionedTransaction, BuilderError> {
        let compiled = self.compile(plan, recent_blockhash, trailing)?;
        self.sign(compiled.message, wallet)
    }
}
