//! Deterministic bonding-curve market model for the simulation harness.
//!
//! Everything here is pure and seeded: the same seed produces the same trade
//! flow, the same curve states and the same account bytes, so a failing
//! simulation can be replayed exactly.
//!
//! The model reuses the production curve arithmetic ([`BuyPlan`], [`SellPlan`]):
//! the simulation exercises the code that will run live, not a copy of it.
//! External actors buy with a lamport budget and sell a share of what external
//! actors hold, so the market can never sell the creator's position.

use bytes::Bytes;
use solana_pubkey::Pubkey;

use crate::builder::constants::{DISCRIMINATOR_BONDING_CURVE_ACCOUNT, PUMP_PROGRAM_ID};
use crate::builder::math::{BuyPlan, CurveParams, SellPlan, BPS_DENOMINATOR};
use crate::errors::{BuilderError, SimulationError};
use crate::monitor::geyser::{AccountUpdate, StreamUpdate};

/// Milliseconds per slot used to derive slot numbers from simulated time.
pub const SLOT_MS: u64 = 400;

/// Slot of the simulated genesis.
pub const BASE_SLOT: u64 = 280_000_000;

/// Size of a full `BondingCurve` account (see `idl/pump.json`).
pub const CURVE_ACCOUNT_BYTES: usize = 166;

/// Upper bound on the trades one generated flow may contain, so a bad profile
/// cannot allocate without limit.
pub const MAX_GENERATED_TRADES: usize = 10_000;

// --- Deterministic randomness -------------------------------------------------------------------

/// SplitMix64: a tiny, fast, well-distributed generator. Not cryptographic, and
/// not meant to be: it only makes simulations reproducible.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// A generator seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`0` when `bound` is zero).
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64().checked_rem(bound).unwrap_or(0)
    }

    /// A value in `lo..=hi` (the bounds may be given in either order).
    pub fn range_inclusive(&mut self, lo: u64, hi: u64) -> u64 {
        let (low, high) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let span = high.saturating_sub(low);
        if span == u64::MAX {
            return self.next_u64();
        }
        low.saturating_add(self.below(span.saturating_add(1)))
    }

    /// `true` with a probability of `bps` basis points.
    pub fn chance_bps(&mut self, bps: u16) -> bool {
        self.below(10_000) < u64::from(bps)
    }
}

/// 32 bytes derived from `seed` and a `domain` label, for deterministic
/// simulation-only keys. These keys are public by construction and must never
/// hold funds.
#[must_use]
pub fn derive_seed_bytes(seed: u64, domain: u64) -> [u8; 32] {
    let mut rng = SplitMix64::new(seed ^ domain.rotate_left(32) ^ 0xA5A5_5A5A_C3C3_3C3C);
    let mut out = [0u8; 32];
    for chunk in out.chunks_mut(8) {
        let word = rng.next_u64().to_le_bytes();
        for (target, byte) in chunk.iter_mut().zip(word.iter()) {
            *target = *byte;
        }
    }
    out
}

// --- Trade flow ---------------------------------------------------------------------------------

/// What an external actor does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeFlow {
    /// Buys with a budget of `lamports`.
    Buy {
        /// Budget, in lamports.
        lamports: u64,
    },
    /// Sells `bps` basis points of the tokens external actors hold.
    SellHoldings {
        /// Share of the external holdings, in basis points.
        bps: u16,
    },
}

/// A trade at a point in simulated time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledTrade {
    /// Milliseconds after genesis.
    pub at_ms: u64,
    /// What happens.
    pub flow: TradeFlow,
}

/// Parameters of the background trade flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowProfile {
    /// Average trades per second (`0` = silence).
    pub trades_per_second: u16,
    /// Probability that a trade is a buy, in basis points.
    pub buy_bias_bps: u16,
    /// Inclusive range of a buy's budget, in lamports.
    pub buy_lamports: (u64, u64),
    /// Inclusive range of a sell's share of the external holdings, in basis points.
    pub sell_bps: (u16, u16),
}

/// Generates the trades between `start_ms` and `horizon_ms`, ordered by time.
/// Gaps are drawn uniformly between half and one and a half average intervals.
#[must_use]
pub fn generate_flow(
    seed: u64,
    profile: &FlowProfile,
    start_ms: u64,
    horizon_ms: u64,
) -> Vec<ScheduledTrade> {
    let mut trades = Vec::new();
    let Some(interval) = 1_000u64.checked_div(u64::from(profile.trades_per_second)) else {
        return trades;
    };
    let interval = interval.max(1);
    let mut rng = SplitMix64::new(seed ^ 0xF10F_F10F_F10F_F10F);
    let low = (interval / 2).max(1);
    let high = interval.saturating_add(interval / 2).max(low);
    let mut at_ms = start_ms;
    while trades.len() < MAX_GENERATED_TRADES {
        at_ms = at_ms.saturating_add(rng.range_inclusive(low, high));
        if at_ms > horizon_ms {
            break;
        }
        let flow = if rng.chance_bps(profile.buy_bias_bps) {
            TradeFlow::Buy {
                lamports: rng.range_inclusive(profile.buy_lamports.0, profile.buy_lamports.1),
            }
        } else {
            let bps =
                rng.range_inclusive(u64::from(profile.sell_bps.0), u64::from(profile.sell_bps.1));
            TradeFlow::SellHoldings {
                bps: u16::try_from(bps.min(10_000)).unwrap_or(10_000),
            }
        };
        trades.push(ScheduledTrade { at_ms, flow });
    }
    trades
}

// --- Account encoding ---------------------------------------------------------------------------

/// Encodes a full-length `BondingCurve` account, as the stream would deliver it.
#[must_use]
pub fn encode_curve_account(
    curve: &CurveParams,
    real_sol_reserves: u64,
    complete: bool,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(CURVE_ACCOUNT_BYTES);
    data.extend_from_slice(&DISCRIMINATOR_BONDING_CURVE_ACCOUNT);
    for value in [
        curve.virtual_token_reserves,
        curve.virtual_sol_reserves,
        curve.real_token_reserves,
        real_sol_reserves,
        curve.token_total_supply,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data.push(u8::from(complete));
    data.extend_from_slice(&[0u8; 32]); // creator
    data.push(0); // is_mayhem_mode
    data.push(0); // is_cashback_coin
    data.resize(CURVE_ACCOUNT_BYTES, 0);
    data
}

// --- Market state -------------------------------------------------------------------------------

/// What happened to our own sells, from the market's point of view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarketLedger {
    /// Trades by external buyers that changed the curve.
    pub external_buys: u32,
    /// Trades by external sellers that changed the curve.
    pub external_sells: u32,
    /// Our sells that executed.
    pub our_sells_landed: u32,
    /// Our sells rejected because the price moved past `min_sol_output`.
    pub our_sells_rejected: u32,
    /// Tokens our executed sells removed from the position.
    pub our_tokens_sold: u64,
    /// Net lamports our executed sells received.
    pub our_lamports_received: u64,
}

/// How one of our sells fared against the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellExecution {
    /// Executed.
    Landed {
        /// Net lamports received (after fees).
        net_lamports: u64,
        /// Slot of the execution.
        slot: u64,
    },
    /// The price moved: the proceeds would have been below `min_sol_output`.
    SlippageExceeded,
    /// The curve completed; selling on it is no longer possible.
    CurveComplete,
}

/// The simulated bonding curve and the actors around it.
#[derive(Debug, Clone)]
pub struct MarketState {
    curve: CurveParams,
    real_sol_reserves: u64,
    fee_bps: u16,
    slot: u64,
    write_version: u64,
    complete: bool,
    external_holdings: u64,
    ledger: MarketLedger,
}

impl MarketState {
    /// A fresh market on `curve`, charging `fee_bps` on every trade.
    #[must_use]
    pub fn new(curve: CurveParams, fee_bps: u16) -> Self {
        Self {
            curve,
            real_sol_reserves: 0,
            fee_bps,
            slot: BASE_SLOT,
            write_version: 0,
            complete: false,
            external_holdings: 0,
            ledger: MarketLedger::default(),
        }
    }

    /// The current reserves.
    #[must_use]
    pub fn curve(&self) -> CurveParams {
        self.curve
    }

    /// Real SOL reserves, in lamports.
    #[must_use]
    pub fn real_sol_reserves(&self) -> u64 {
        self.real_sol_reserves
    }

    /// Slot of the latest change.
    #[must_use]
    pub fn slot(&self) -> u64 {
        self.slot
    }

    /// Whether the curve completed (graduated).
    #[must_use]
    pub fn complete(&self) -> bool {
        self.complete
    }

    /// Tokens external actors currently hold.
    #[must_use]
    pub fn external_holdings(&self) -> u64 {
        self.external_holdings
    }

    /// The running tally.
    #[must_use]
    pub fn ledger(&self) -> MarketLedger {
        self.ledger
    }

    /// Moves the slot forward for a change at `elapsed_ms`: never backwards, and
    /// by at least one, so every change has its own `(slot)` ordering.
    fn advance_slot(&mut self, elapsed_ms: u64) -> u64 {
        let by_time = BASE_SLOT.saturating_add(elapsed_ms / SLOT_MS);
        self.slot = by_time.max(self.slot.saturating_add(1));
        self.slot
    }

    fn mark_complete_if_drained(&mut self) {
        if self.curve.real_token_reserves == 0 {
            self.complete = true;
        }
    }

    fn arithmetic(context: &'static str) -> SimulationError {
        SimulationError::Builder(BuilderError::ArithmeticOverflow { context })
    }

    /// Applies the creator's atomic buy at genesis.
    ///
    /// # Errors
    /// [`SimulationError::Builder`] if a reserve under- or overflows.
    pub fn apply_creator_buy(&mut self, plan: &BuyPlan) -> Result<(), SimulationError> {
        self.apply_buy(plan, "creator buy")?;
        self.advance_slot(0);
        Ok(())
    }

    fn apply_buy(&mut self, plan: &BuyPlan, context: &'static str) -> Result<(), SimulationError> {
        let virtual_tokens = self
            .curve
            .virtual_token_reserves
            .checked_sub(plan.token_amount)
            .ok_or_else(|| Self::arithmetic(context))?;
        let real_tokens = self
            .curve
            .real_token_reserves
            .checked_sub(plan.token_amount)
            .ok_or_else(|| Self::arithmetic(context))?;
        let virtual_sol = self
            .curve
            .virtual_sol_reserves
            .checked_add(plan.net_cost_lamports)
            .ok_or_else(|| Self::arithmetic(context))?;
        let real_sol = self
            .real_sol_reserves
            .checked_add(plan.net_cost_lamports)
            .ok_or_else(|| Self::arithmetic(context))?;
        self.curve.virtual_token_reserves = virtual_tokens;
        self.curve.real_token_reserves = real_tokens;
        self.curve.virtual_sol_reserves = virtual_sol;
        self.real_sol_reserves = real_sol;
        self.mark_complete_if_drained();
        Ok(())
    }

    /// An external buy with a budget of `lamports`. Returns whether the curve
    /// changed (a completed curve, or a budget too small to buy a token, leaves
    /// it untouched).
    ///
    /// # Errors
    /// [`SimulationError::Builder`] on arithmetic failure.
    pub fn external_buy(
        &mut self,
        lamports: u64,
        elapsed_ms: u64,
    ) -> Result<bool, SimulationError> {
        if self.complete {
            return Ok(false);
        }
        let plan = match BuyPlan::from_budget(&self.curve, lamports, self.fee_bps, 0) {
            Ok(plan) => plan,
            Err(BuilderError::ZeroBuyAmount) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        self.apply_buy(&plan, "external buy")?;
        self.external_holdings = self.external_holdings.saturating_add(plan.token_amount);
        self.ledger.external_buys = self.ledger.external_buys.saturating_add(1);
        self.advance_slot(elapsed_ms);
        Ok(true)
    }

    /// An external sale of `bps` basis points of the external holdings. Returns
    /// whether the curve changed.
    ///
    /// # Errors
    /// [`SimulationError::Builder`] on arithmetic failure.
    pub fn external_sell(&mut self, bps: u16, elapsed_ms: u64) -> Result<bool, SimulationError> {
        if self.complete {
            return Ok(false);
        }
        let scaled = u128::from(self.external_holdings)
            .saturating_mul(u128::from(bps.min(10_000)))
            .checked_div(BPS_DENOMINATOR)
            .unwrap_or(0);
        let tokens = u64::try_from(scaled)
            .unwrap_or(0)
            .min(self.external_holdings);
        if tokens == 0 {
            return Ok(false);
        }
        let plan = match SellPlan::from_token_amount(&self.curve, tokens, self.fee_bps, 0) {
            Ok(plan) => plan,
            // Proceeds round down to nothing: too small to matter.
            Err(BuilderError::InvalidCurve { .. }) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        self.curve = plan.apply_to(&self.curve)?;
        self.real_sol_reserves = self
            .real_sol_reserves
            .saturating_sub(plan.gross_proceeds_lamports);
        self.external_holdings = self.external_holdings.saturating_sub(tokens);
        self.ledger.external_sells = self.ledger.external_sells.saturating_add(1);
        self.advance_slot(elapsed_ms);
        Ok(true)
    }

    /// Executes one of our sells against the *current* curve. The order was
    /// quoted earlier, so the price may have moved since: the sell fails, as it
    /// would on chain, when the proceeds fall below `order.min_sol_output_lamports`.
    ///
    /// # Errors
    /// [`SimulationError::Builder`] on arithmetic failure.
    pub fn execute_sell(
        &mut self,
        order: &SellPlan,
        elapsed_ms: u64,
    ) -> Result<SellExecution, SimulationError> {
        if self.complete {
            return Ok(SellExecution::CurveComplete);
        }
        let now =
            match SellPlan::from_token_amount(&self.curve, order.token_amount, order.fee_bps, 0) {
                Ok(plan) => plan,
                Err(BuilderError::InvalidCurve { .. }) => {
                    self.ledger.our_sells_rejected =
                        self.ledger.our_sells_rejected.saturating_add(1);
                    return Ok(SellExecution::SlippageExceeded);
                }
                Err(error) => return Err(error.into()),
            };
        if now.net_proceeds_lamports < order.min_sol_output_lamports {
            self.ledger.our_sells_rejected = self.ledger.our_sells_rejected.saturating_add(1);
            return Ok(SellExecution::SlippageExceeded);
        }
        self.curve = now.apply_to(&self.curve)?;
        self.real_sol_reserves = self
            .real_sol_reserves
            .saturating_sub(now.gross_proceeds_lamports);
        self.ledger.our_sells_landed = self.ledger.our_sells_landed.saturating_add(1);
        self.ledger.our_tokens_sold = self
            .ledger
            .our_tokens_sold
            .saturating_add(order.token_amount);
        self.ledger.our_lamports_received = self
            .ledger
            .our_lamports_received
            .saturating_add(now.net_proceeds_lamports);
        let slot = self.advance_slot(elapsed_ms);
        Ok(SellExecution::Landed {
            net_lamports: now.net_proceeds_lamports,
            slot,
        })
    }

    /// The account update the stream would publish for the current state.
    pub fn account_update(&mut self, curve_account: Pubkey) -> StreamUpdate {
        self.write_version = self.write_version.saturating_add(1);
        StreamUpdate::Account(AccountUpdate {
            slot: self.slot,
            write_version: self.write_version,
            pubkey: curve_account,
            owner: PUMP_PROGRAM_ID,
            lamports: self.real_sol_reserves,
            data: Bytes::from(encode_curve_account(
                &self.curve,
                self.real_sol_reserves,
                self.complete,
            )),
        })
    }
}
