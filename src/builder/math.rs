//! Constant-product bonding-curve math for the creator's initial buy.
//!
//! All arithmetic is exact integer math in `u128` with checked operations, so
//! there is no floating point, no silent wrap-around and no panic. Rounding
//! always favours safety:
//!
//! - tokens received for a budget are rounded **down**;
//! - the SOL cost of a token amount and the slippage cap are rounded **up**.
//!
//! # Model
//! With virtual reserves `vs` (SOL) and `vt` (tokens), `k = vs * vt` is
//! constant, so buying `t` tokens costs `ceil(vs * t / (vt - t))` lamports
//! before fees. Fees are charged on top: `gross = net * (10_000 + fee_bps) /
//! 10_000`. The program's `max_sol_cost` bounds the **gross** amount.
//!
//! The fee schedule of the live program is tiered and owned by the fee program,
//! so `fee_bps` is an explicit, caller-controlled assumption
//! ([`DEFAULT_TOTAL_FEE_BPS`]); the slippage margin absorbs any difference and a
//! pre-execution simulation (Phase 4) validates the final numbers.

use super::constants::{
    INITIAL_REAL_TOKEN_RESERVES, INITIAL_VIRTUAL_SOL_RESERVES, INITIAL_VIRTUAL_TOKEN_RESERVES,
    TOKEN_TOTAL_SUPPLY,
};
use crate::errors::BuilderError;

/// Basis-point denominator.
pub const BPS_DENOMINATOR: u128 = 10_000;
/// Default assumed total trading fee: 0.95 % protocol + 0.30 % creator.
pub const DEFAULT_TOTAL_FEE_BPS: u16 = 125;
/// Highest fee assumption accepted (10 %); a guard against typos.
pub const MAX_FEE_BPS: u16 = 1_000;
/// Highest slippage tolerance accepted. Single source of truth: the config
/// layer's bound (10 %), so the two can never drift apart (audit finding #20).
pub const MAX_SLIPPAGE_BPS: u16 = crate::config::MAX_SLIPPAGE_BPS;

/// Reserves of a freshly created bonding curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveParams {
    /// Virtual token reserves, in base units.
    pub virtual_token_reserves: u64,
    /// Virtual SOL reserves, in lamports.
    pub virtual_sol_reserves: u64,
    /// Tokens actually for sale, in base units.
    pub real_token_reserves: u64,
    /// Total token supply, in base units.
    pub token_total_supply: u64,
}

impl Default for CurveParams {
    /// The program's launch parameters (`Global` account defaults): 30 SOL /
    /// 1,073,000,000 virtual tokens, 793,100,000 tokens for sale, 1B supply.
    fn default() -> Self {
        Self {
            virtual_token_reserves: INITIAL_VIRTUAL_TOKEN_RESERVES,
            virtual_sol_reserves: INITIAL_VIRTUAL_SOL_RESERVES,
            real_token_reserves: INITIAL_REAL_TOKEN_RESERVES,
            token_total_supply: TOKEN_TOTAL_SUPPLY,
        }
    }
}

impl CurveParams {
    /// Checks internal consistency.
    ///
    /// # Errors
    /// [`BuilderError::InvalidCurve`] for zero reserves or a real reserve that
    /// exceeds the virtual reserve (the curve could then never be solvent).
    pub fn validate(&self) -> Result<(), BuilderError> {
        if self.virtual_token_reserves == 0 || self.virtual_sol_reserves == 0 {
            return Err(BuilderError::InvalidCurve {
                reason: "virtual reserves must be non-zero",
            });
        }
        if self.token_total_supply == 0 {
            return Err(BuilderError::InvalidCurve {
                reason: "token supply must be non-zero",
            });
        }
        if self.real_token_reserves > self.virtual_token_reserves {
            return Err(BuilderError::InvalidCurve {
                reason: "real token reserves exceed virtual token reserves",
            });
        }
        Ok(())
    }
}

/// A fully resolved initial buy: how many tokens, and the SOL bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyPlan {
    /// Tokens to buy (`amount` argument of `buy_v3`), in base units.
    pub token_amount: u64,
    /// Expected cost excluding fees, in lamports.
    pub net_cost_lamports: u64,
    /// Expected cost including the assumed fees, in lamports.
    pub expected_cost_lamports: u64,
    /// Slippage-protected cap (`max_sol_cost` argument), in lamports.
    pub max_sol_cost_lamports: u64,
    /// Assumed fee in basis points used for this plan.
    pub fee_bps: u16,
    /// Slippage tolerance in basis points used for this plan.
    pub slippage_bps: u16,
}

impl BuyPlan {
    /// Plans a buy that spends **at most** `budget_lamports` (fees included)
    /// before slippage, rounding the token amount down.
    ///
    /// # Errors
    /// - [`BuilderError::InvalidFee`] / [`BuilderError::InvalidSlippage`] for
    ///   out-of-range basis points;
    /// - [`BuilderError::ZeroBuyAmount`] when the budget buys no token;
    /// - [`BuilderError::BuyExceedsReserves`] is *not* returned here: the amount
    ///   is capped to the tokens for sale instead (the cost is then recomputed).
    pub fn from_budget(
        curve: &CurveParams,
        budget_lamports: u64,
        fee_bps: u16,
        slippage_bps: u16,
    ) -> Result<Self, BuilderError> {
        curve.validate()?;
        check_bps(fee_bps, slippage_bps)?;

        // net = budget / (1 + fee), rounded down.
        let net = mul_div_floor(
            u128::from(budget_lamports),
            BPS_DENOMINATOR,
            BPS_DENOMINATOR.saturating_add(u128::from(fee_bps)),
            "net budget",
        )?;
        // tokens = vt * net / (vs + net), rounded down.
        let vs = u128::from(curve.virtual_sol_reserves);
        let vt = u128::from(curve.virtual_token_reserves);
        let denominator = vs
            .checked_add(net)
            .ok_or(BuilderError::ArithmeticOverflow {
                context: "curve denominator",
            })?;
        let tokens = mul_div_floor(vt, net, denominator, "tokens for budget")?;
        let capped = tokens.min(u128::from(curve.real_token_reserves));
        let token_amount = u64::try_from(capped).map_err(|_| BuilderError::ArithmeticOverflow {
            context: "token amount",
        })?;
        Self::from_token_amount(curve, token_amount, fee_bps, slippage_bps)
    }

    /// Plans a buy of exactly `token_amount` base units.
    ///
    /// # Errors
    /// - [`BuilderError::ZeroBuyAmount`] for `0`;
    /// - [`BuilderError::BuyExceedsReserves`] above the tokens for sale;
    /// - the same validation errors as [`BuyPlan::from_budget`].
    pub fn from_token_amount(
        curve: &CurveParams,
        token_amount: u64,
        fee_bps: u16,
        slippage_bps: u16,
    ) -> Result<Self, BuilderError> {
        curve.validate()?;
        check_bps(fee_bps, slippage_bps)?;
        if token_amount == 0 {
            return Err(BuilderError::ZeroBuyAmount);
        }
        if token_amount > curve.real_token_reserves {
            return Err(BuilderError::BuyExceedsReserves {
                requested: token_amount,
                available: curve.real_token_reserves,
            });
        }

        // net = ceil(vs * t / (vt - t)); `t <= real <= vt` and `t != vt` is
        // guaranteed by the check below.
        let vt = u128::from(curve.virtual_token_reserves);
        let remaining =
            vt.checked_sub(u128::from(token_amount))
                .ok_or(BuilderError::ArithmeticOverflow {
                    context: "remaining virtual tokens",
                })?;
        if remaining == 0 {
            return Err(BuilderError::InvalidCurve {
                reason: "purchase would drain the virtual token reserve",
            });
        }
        let net = mul_div_ceil(
            u128::from(curve.virtual_sol_reserves),
            u128::from(token_amount),
            remaining,
            "net cost",
        )?;
        let gross = mul_div_ceil(
            net,
            BPS_DENOMINATOR.saturating_add(u128::from(fee_bps)),
            BPS_DENOMINATOR,
            "gross cost",
        )?;
        let max_cost = mul_div_ceil(
            gross,
            BPS_DENOMINATOR.saturating_add(u128::from(slippage_bps)),
            BPS_DENOMINATOR,
            "slippage cap",
        )?;

        Ok(Self {
            token_amount,
            net_cost_lamports: to_u64(net, "net cost")?,
            expected_cost_lamports: to_u64(gross, "gross cost")?,
            max_sol_cost_lamports: to_u64(max_cost, "slippage cap")?,
            fee_bps,
            slippage_bps,
        })
    }

    /// Plans a buy of a target share of the **total supply**, in basis points
    /// (`975` = 9.75 %), rounding the token amount down.
    ///
    /// # Errors
    /// [`BuilderError::InvalidCurve`] if `supply_bps` is above `10_000`;
    /// otherwise see [`BuyPlan::from_token_amount`].
    pub fn from_supply_bps(
        curve: &CurveParams,
        supply_bps: u16,
        fee_bps: u16,
        slippage_bps: u16,
    ) -> Result<Self, BuilderError> {
        curve.validate()?;
        if u128::from(supply_bps) > BPS_DENOMINATOR {
            return Err(BuilderError::InvalidCurve {
                reason: "supply share above 100 %",
            });
        }
        let tokens = mul_div_floor(
            u128::from(curve.token_total_supply),
            u128::from(supply_bps),
            BPS_DENOMINATOR,
            "supply share",
        )?;
        let token_amount = to_u64(tokens, "supply share")?;
        Self::from_token_amount(curve, token_amount, fee_bps, slippage_bps)
    }

    /// Share of the total supply bought, in basis points (rounded down).
    ///
    /// # Errors
    /// [`BuilderError::InvalidCurve`] for a zero supply.
    pub fn supply_share_bps(&self, curve: &CurveParams) -> Result<u64, BuilderError> {
        if curve.token_total_supply == 0 {
            return Err(BuilderError::InvalidCurve {
                reason: "token supply must be non-zero",
            });
        }
        let share = mul_div_floor(
            u128::from(self.token_amount),
            BPS_DENOMINATOR,
            u128::from(curve.token_total_supply),
            "supply share",
        )?;
        to_u64(share, "supply share")
    }
}

fn check_bps(fee_bps: u16, slippage_bps: u16) -> Result<(), BuilderError> {
    if fee_bps > MAX_FEE_BPS {
        return Err(BuilderError::InvalidFee {
            bps: fee_bps,
            max: MAX_FEE_BPS,
        });
    }
    if slippage_bps > MAX_SLIPPAGE_BPS {
        return Err(BuilderError::InvalidSlippage {
            bps: slippage_bps,
            max: MAX_SLIPPAGE_BPS,
        });
    }
    Ok(())
}

fn to_u64(value: u128, context: &'static str) -> Result<u64, BuilderError> {
    u64::try_from(value).map_err(|_| BuilderError::ArithmeticOverflow { context })
}

/// `floor(a * b / c)` with checked arithmetic.
fn mul_div_floor(a: u128, b: u128, c: u128, context: &'static str) -> Result<u128, BuilderError> {
    let product = a
        .checked_mul(b)
        .ok_or(BuilderError::ArithmeticOverflow { context })?;
    product
        .checked_div(c)
        .ok_or(BuilderError::ArithmeticOverflow { context })
}

/// `ceil(a * b / c)` with checked arithmetic.
fn mul_div_ceil(a: u128, b: u128, c: u128, context: &'static str) -> Result<u128, BuilderError> {
    let product = a
        .checked_mul(b)
        .ok_or(BuilderError::ArithmeticOverflow { context })?;
    let quotient = product
        .checked_div(c)
        .ok_or(BuilderError::ArithmeticOverflow { context })?;
    let remainder = product
        .checked_rem(c)
        .ok_or(BuilderError::ArithmeticOverflow { context })?;
    if remainder == 0 {
        Ok(quotient)
    } else {
        quotient
            .checked_add(1)
            .ok_or(BuilderError::ArithmeticOverflow { context })
    }
}
