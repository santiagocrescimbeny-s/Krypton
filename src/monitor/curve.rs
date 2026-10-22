//! Zero-copy decoding of the Pump.fun `BondingCurve` account and the curve
//! arithmetic the exit engine needs (progression and price movement).
//!
//! # Layout
//! `BondingCurve` is an Anchor account: an 8-byte discriminator followed by
//! Borsh fields (little-endian, no padding), per `idl/pump.json`:
//!
//! | offset | field                      | type |
//! |--------|----------------------------|------|
//! | 8      | `virtual_token_reserves`   | u64  |
//! | 16     | `virtual_quote_reserves`   | u64  |
//! | 24     | `real_token_reserves`      | u64  |
//! | 32     | `real_quote_reserves`      | u64  |
//! | 40     | `token_total_supply`       | u64  |
//! | 48     | `complete`                 | bool |
//! | 49     | `creator`                  | pubkey |
//! | 81     | `is_mayhem_mode`           | bool |
//! | 82     | `is_cashback_coin`         | bool |
//!
//! Older accounts are shorter than the current layout, so only the first 49
//! bytes are mandatory; the flags after `complete` default to `false` when the
//! account predates them. The decoder reads straight from the borrowed bytes
//! (no allocation, no intermediate struct per field) and never indexes: every
//! read goes through [`slice::get`].

use super::geyser::AccountUpdate;
use crate::builder::constants::{DISCRIMINATOR_BONDING_CURVE_ACCOUNT, INITIAL_REAL_TOKEN_RESERVES};
use crate::builder::math::{BuyPlan, CurveParams, BPS_DENOMINATOR};
use crate::errors::{BuilderError, MonitorError};

const OFFSET_VIRTUAL_TOKEN: usize = 8;
const OFFSET_VIRTUAL_QUOTE: usize = 16;
const OFFSET_REAL_TOKEN: usize = 24;
const OFFSET_REAL_QUOTE: usize = 32;
const OFFSET_SUPPLY: usize = 40;
const OFFSET_COMPLETE: usize = 48;
const OFFSET_MAYHEM: usize = 81;
const OFFSET_CASHBACK: usize = 82;

/// Bytes every decodable account must have (through `complete`).
pub const MIN_BONDING_CURVE_BYTES: usize = OFFSET_COMPLETE + 1;

/// Reads a little-endian `u64` at `offset`.
fn read_u64(data: &[u8], offset: usize) -> Result<u64, MonitorError> {
    let end = offset.checked_add(8).ok_or(MonitorError::AccountTooShort {
        actual: data.len(),
        min: MIN_BONDING_CURVE_BYTES,
    })?;
    let bytes = data
        .get(offset..end)
        .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
        .ok_or(MonitorError::AccountTooShort {
            actual: data.len(),
            min: end,
        })?;
    Ok(u64::from_le_bytes(bytes))
}

/// Reads a Borsh `bool` at `offset`; `None` when the account is too short.
fn read_bool(data: &[u8], offset: usize) -> Result<Option<bool>, MonitorError> {
    match data.get(offset) {
        None => Ok(None),
        Some(0) => Ok(Some(false)),
        Some(1) => Ok(Some(true)),
        Some(_) => Err(MonitorError::InvalidAccountData {
            reason: "boolean field is neither 0 nor 1",
        }),
    }
}

/// The reserves and flags of a bonding curve at one point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurveState {
    /// Virtual token reserves, in base units.
    pub virtual_token_reserves: u64,
    /// Virtual SOL reserves, in lamports.
    pub virtual_sol_reserves: u64,
    /// Tokens still for sale, in base units.
    pub real_token_reserves: u64,
    /// SOL actually held by the curve, in lamports.
    pub real_sol_reserves: u64,
    /// Total token supply, in base units.
    pub token_total_supply: u64,
    /// The curve completed (graduated): no more trades on the curve.
    pub complete: bool,
    /// The coin runs in mayhem mode.
    pub is_mayhem_mode: bool,
    /// The coin is a cashback coin (`sell_v3` is rejected on chain).
    pub is_cashback_coin: bool,
}

impl BondingCurveState {
    /// Decodes an account payload.
    ///
    /// # Errors
    /// - [`MonitorError::AccountTooShort`] below [`MIN_BONDING_CURVE_BYTES`];
    /// - [`MonitorError::UnexpectedDiscriminator`] for another account type;
    /// - [`MonitorError::InvalidAccountData`] for a non-boolean flag or zero
    ///   virtual reserves on a live curve.
    pub fn decode(data: &[u8]) -> Result<Self, MonitorError> {
        if data.len() < MIN_BONDING_CURVE_BYTES {
            return Err(MonitorError::AccountTooShort {
                actual: data.len(),
                min: MIN_BONDING_CURVE_BYTES,
            });
        }
        if data.get(..8) != Some(DISCRIMINATOR_BONDING_CURVE_ACCOUNT.as_slice()) {
            return Err(MonitorError::UnexpectedDiscriminator);
        }
        let complete = read_bool(data, OFFSET_COMPLETE)?.ok_or(MonitorError::AccountTooShort {
            actual: data.len(),
            min: MIN_BONDING_CURVE_BYTES,
        })?;
        let state = Self {
            virtual_token_reserves: read_u64(data, OFFSET_VIRTUAL_TOKEN)?,
            virtual_sol_reserves: read_u64(data, OFFSET_VIRTUAL_QUOTE)?,
            real_token_reserves: read_u64(data, OFFSET_REAL_TOKEN)?,
            real_sol_reserves: read_u64(data, OFFSET_REAL_QUOTE)?,
            token_total_supply: read_u64(data, OFFSET_SUPPLY)?,
            complete,
            is_mayhem_mode: read_bool(data, OFFSET_MAYHEM)?.unwrap_or(false),
            is_cashback_coin: read_bool(data, OFFSET_CASHBACK)?.unwrap_or(false),
        };
        if !state.complete && (state.virtual_token_reserves == 0 || state.virtual_sol_reserves == 0)
        {
            return Err(MonitorError::InvalidAccountData {
                reason: "live curve with zero virtual reserves",
            });
        }
        Ok(state)
    }

    /// The reserves as the math layer's [`CurveParams`].
    #[must_use]
    pub fn curve_params(&self) -> CurveParams {
        CurveParams {
            virtual_token_reserves: self.virtual_token_reserves,
            virtual_sol_reserves: self.virtual_sol_reserves,
            real_token_reserves: self.real_token_reserves,
            token_total_supply: self.token_total_supply,
        }
    }

    /// Replaces the reserves with `params` (flags unchanged). Used to project
    /// the effect of sells that are still in flight.
    #[must_use]
    pub fn with_params(&self, params: &CurveParams) -> Self {
        Self {
            virtual_token_reserves: params.virtual_token_reserves,
            virtual_sol_reserves: params.virtual_sol_reserves,
            real_token_reserves: params.real_token_reserves,
            ..*self
        }
    }

    /// Share of the tokens for sale that have been bought, in basis points
    /// (`0` = fresh curve, `10_000` = sold out), measured against
    /// `initial_real_token_reserves` (normally [`INITIAL_REAL_TOKEN_RESERVES`]).
    ///
    /// # Errors
    /// [`MonitorError::InvalidAccountData`] if `initial_real_token_reserves` is
    /// zero or the arithmetic overflows.
    pub fn progress_bps_against(
        &self,
        initial_real_token_reserves: u64,
    ) -> Result<u16, MonitorError> {
        if initial_real_token_reserves == 0 {
            return Err(MonitorError::InvalidAccountData {
                reason: "initial real reserves are zero",
            });
        }
        let sold = initial_real_token_reserves.saturating_sub(self.real_token_reserves);
        let bps = u128::from(sold)
            .checked_mul(BPS_DENOMINATOR)
            .and_then(|scaled| scaled.checked_div(u128::from(initial_real_token_reserves)))
            .ok_or(MonitorError::InvalidAccountData {
                reason: "progress overflow",
            })?;
        // `sold <= initial`, so `bps <= 10_000`.
        u16::try_from(bps).map_err(|_| MonitorError::InvalidAccountData {
            reason: "progress above 100 %",
        })
    }

    /// [`BondingCurveState::progress_bps_against`] with the program's default
    /// launch reserves.
    ///
    /// # Errors
    /// See [`BondingCurveState::progress_bps_against`].
    pub fn progress_bps(&self) -> Result<u16, MonitorError> {
        self.progress_bps_against(INITIAL_REAL_TOKEN_RESERVES)
    }

    /// Spot price now relative to `reference`, in basis points (`10_000` =
    /// unchanged, `5_000` = halved).
    ///
    /// The price is `virtual_sol / virtual_token`; the ratio is evaluated by
    /// cross-multiplication in `u128` so no precision is lost to an early
    /// division: `(vs_now * vt_ref * 10_000) / (vt_now * vs_ref)`.
    ///
    /// # Errors
    /// [`MonitorError::InvalidAccountData`] when a reserve is zero or the
    /// arithmetic overflows.
    pub fn price_ratio_bps(&self, reference: &PriceReference) -> Result<u128, MonitorError> {
        let invalid = || MonitorError::InvalidAccountData {
            reason: "price ratio is undefined",
        };
        let numerator = u128::from(self.virtual_sol_reserves)
            .checked_mul(u128::from(reference.virtual_token_reserves))
            .and_then(|value| value.checked_mul(BPS_DENOMINATOR))
            .ok_or_else(invalid)?;
        let denominator = u128::from(self.virtual_token_reserves)
            .checked_mul(u128::from(reference.virtual_sol_reserves))
            .filter(|value| *value != 0)
            .ok_or_else(invalid)?;
        numerator.checked_div(denominator).ok_or_else(invalid)
    }
}

/// The two reserves that define a spot price, kept as the entry reference of a
/// position (typically the curve right after the creator's own buy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceReference {
    /// Virtual SOL reserves, in lamports.
    pub virtual_sol_reserves: u64,
    /// Virtual token reserves, in base units.
    pub virtual_token_reserves: u64,
}

impl PriceReference {
    /// The curve as it stands right after `plan` executes against `curve`:
    /// the net cost enters the SOL reserve and the tokens leave the token reserve.
    ///
    /// # Errors
    /// [`BuilderError::ArithmeticOverflow`] if a reserve under- or overflows.
    pub fn after_buy(curve: &CurveParams, plan: &BuyPlan) -> Result<Self, BuilderError> {
        let overflow = |context| BuilderError::ArithmeticOverflow { context };
        Ok(Self {
            virtual_sol_reserves: curve
                .virtual_sol_reserves
                .checked_add(plan.net_cost_lamports)
                .ok_or_else(|| overflow("post-buy virtual SOL"))?,
            virtual_token_reserves: curve
                .virtual_token_reserves
                .checked_sub(plan.token_amount)
                .ok_or_else(|| overflow("post-buy virtual tokens"))?,
        })
    }
}

impl From<&BondingCurveState> for PriceReference {
    fn from(state: &BondingCurveState) -> Self {
        Self {
            virtual_sol_reserves: state.virtual_sol_reserves,
            virtual_token_reserves: state.virtual_token_reserves,
        }
    }
}

/// A decoded curve together with the stream position it was observed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveObservation {
    /// Slot of the update.
    pub slot: u64,
    /// Geyser write version (orders updates within a slot).
    pub write_version: u64,
    /// The decoded state.
    pub state: BondingCurveState,
}

impl CurveObservation {
    /// Decodes the payload of a stream account update.
    ///
    /// # Errors
    /// See [`BondingCurveState::decode`].
    pub fn from_account(update: &AccountUpdate) -> Result<Self, MonitorError> {
        Ok(Self {
            slot: update.slot,
            write_version: update.write_version,
            state: BondingCurveState::decode(&update.data)?,
        })
    }

    /// Whether `self` is strictly newer than `other` (by slot, then write
    /// version). Used to drop out-of-order updates after a reconnection.
    #[must_use]
    pub fn is_newer_than(&self, other: &Self) -> bool {
        (self.slot, self.write_version) > (other.slot, other.write_version)
    }
}
