//! Micro-tranche exit engine, its async manager and the sell-order executor.
//!
//! # Model
//! After a launch the creator holds a position of `P` tokens. The exit plan
//! splits (part of) that position into **tranches**: each has a time offset from
//! genesis inside the strict execution window (1 s to 10 s) and a share of `P`
//! in basis points. At its time a tranche is quoted against the **latest
//! observed bonding curve** and turned into a [`SellOrder`] whose
//! `min_sol_output` obeys the 10 % slippage cap of audit finding #20 (the
//! [`SellPlan`] refuses anything wider).
//!
//! The logic is split in two layers:
//!
//! - [`ExitEngine`] is a synchronous, deterministic state machine. It is fed
//!   curve observations and elapsed time, and returns orders. It has no clock, no
//!   channel and no task, which makes it exhaustively testable.
//! - [`ExitManager`] wires the engine to Tokio: it consumes
//!   [`StreamUpdate`]s, sleeps until the next tranche, dispatches orders with
//!   `try_send` and feeds execution reports back into the engine.
//!
//! # Our own price impact
//! Tranches are seconds apart, but a stream update reflecting tranche `n` may
//! lag behind. Quoting tranche `n + 1` against the *pre-sale* reserves would
//! produce a `min_sol_output` the market can no longer satisfy and the order
//! would fail on chain. The engine therefore quotes against the latest
//! observation **with every still-in-flight sale applied on top**
//! ([`SellPlan::apply_to`]); a sale leaves the projection when it failed, or
//! when an observation at or after its landing slot arrives. A sale that landed
//! before the observation but whose report is still pending is counted twice,
//! which only lowers the floor by our own (small) impact and never raises it.
//!
//! # Failure policy
//! Tokens of a failed order, and the share of a dropped tranche, are carried
//! into the next tranche, so a transient failure delays the exit instead of
//! silently shrinking it. A tranche that comes due more than
//! [`MAX_LATENESS_MS`] late, or after the end of the window, is dropped.
//!
//! # Unknown outcomes
//! A bundle that is still unresolved when the polling budget ends may have
//! landed. Re-offering its tokens would risk a double sell: the duplicate fails
//! on chain but still costs the tip and the priority fee. An order reported as
//! [`SellOutcome::Unknown`] is therefore held apart: not sold, not carried
//! forward. [`EngineSnapshot::tokens_unresolved`] shows it, and
//! [`ExitEngine::resolve_unknown`] settles it once the caller has read the
//! token account. Its price impact stays in the quotes for
//! [`UNRESOLVED_PROJECTION_SLOTS`] slots, after which it can no longer land.

use std::sync::Arc;
use std::time::Duration;

use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{sleep_until, timeout, Instant};
use tracing::{debug, info, warn};

use crate::metrics::PipelineMetrics;

use super::curve::{CurveObservation, PriceReference};
use super::geyser::StreamUpdate;
use super::ShutdownSignal;
use crate::builder::math::{SellPlan, MAX_FEE_BPS};
use crate::config::MAX_SLIPPAGE_BPS;
use crate::errors::ExitError;

/// First instant (milliseconds after genesis) at which a sell may be issued.
pub const EXIT_WINDOW_START_MS: u64 = 1_000;
/// Last instant (milliseconds after genesis) of the execution window.
pub const EXIT_WINDOW_END_MS: u64 = 10_000;
/// Maximum tranches in a plan.
pub const MAX_TRANCHES: usize = 10;
/// How late a tranche may still be issued before it is dropped.
pub const MAX_LATENESS_MS: u64 = 500;
/// Stop-loss orders one run may issue (the first plus retries after failures).
pub const MAX_STOP_LOSS_ORDERS: u8 = 3;
/// Slots after the quoting observation during which an order with an unknown
/// outcome may still land. A transaction is valid for about 150 slots; the
/// margin covers clock skew between the observation and the blockhash.
pub const UNRESOLVED_PROJECTION_SLOTS: u64 = 160;

/// Capacity of [`order_channel`]: strictly more than the most orders one engine
/// can ever issue (every tranche plus every stop-loss order), so the manager's
/// non-blocking `try_send` can never find the channel full.
pub const ORDER_CHANNEL_CAPACITY: usize = MAX_TRANCHES + MAX_STOP_LOSS_ORDERS as usize + 1;
/// Capacity of [`report_channel`].
pub const REPORT_CHANNEL_CAPACITY: usize = ORDER_CHANNEL_CAPACITY * 2;
/// Default time the manager keeps waiting for execution reports after the
/// window has closed.
pub const DEFAULT_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

const FULL_POSITION_BPS: u16 = 10_000;
const BPS_DENOMINATOR: u128 = 10_000;

// --- Plan ----------------------------------------------------------------------------------

/// One scheduled sale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tranche {
    /// Offset from genesis, in milliseconds, inside the execution window.
    pub at_ms: u64,
    /// Share of the initial position to sell, in basis points.
    pub fraction_bps: u16,
}

/// The validated exit schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitPlan {
    tranches: Vec<Tranche>,
    stop_loss_bps: Option<u16>,
}

impl ExitPlan {
    /// Validates and wraps a schedule.
    ///
    /// Rules: `1..=`[`MAX_TRANCHES`] tranches; each `at_ms` inside
    /// `[EXIT_WINDOW_START_MS, EXIT_WINDOW_END_MS]` and strictly increasing; each
    /// fraction positive; fractions sum to at most 100 %; a stop-loss, if any,
    /// strictly between 0 % and 100 %.
    ///
    /// `stop_loss_bps` is the price drop (relative to the position's entry
    /// price) that makes the engine sell everything left at once.
    ///
    /// # Errors
    /// [`ExitError::InvalidPlan`].
    pub fn new(tranches: Vec<Tranche>, stop_loss_bps: Option<u16>) -> Result<Self, ExitError> {
        if tranches.is_empty() {
            return Err(ExitError::InvalidPlan {
                reason: "at least one tranche is required",
            });
        }
        if tranches.len() > MAX_TRANCHES {
            return Err(ExitError::InvalidPlan {
                reason: "too many tranches",
            });
        }
        let mut previous: Option<u64> = None;
        let mut total: u32 = 0;
        for tranche in &tranches {
            if tranche.at_ms < EXIT_WINDOW_START_MS || tranche.at_ms > EXIT_WINDOW_END_MS {
                return Err(ExitError::InvalidPlan {
                    reason: "tranche outside the 1 s to 10 s execution window",
                });
            }
            if previous.is_some_and(|prev| tranche.at_ms <= prev) {
                return Err(ExitError::InvalidPlan {
                    reason: "tranche times must be strictly increasing",
                });
            }
            if tranche.fraction_bps == 0 {
                return Err(ExitError::InvalidPlan {
                    reason: "tranche fraction must be positive",
                });
            }
            previous = Some(tranche.at_ms);
            total = total.saturating_add(u32::from(tranche.fraction_bps));
        }
        if total > u32::from(FULL_POSITION_BPS) {
            return Err(ExitError::InvalidPlan {
                reason: "tranche fractions exceed 100 % of the position",
            });
        }
        if let Some(stop) = stop_loss_bps {
            if stop == 0 || stop >= FULL_POSITION_BPS {
                return Err(ExitError::InvalidPlan {
                    reason: "stop-loss must be between 1 and 9999 basis points",
                });
            }
        }
        Ok(Self {
            tranches,
            stop_loss_bps,
        })
    }

    /// Splits `total_bps` of the position into `count` tranches evenly spaced
    /// across the whole window (first at 1 s, last at 10 s). The division
    /// remainder goes to the last tranche, so the fractions add up exactly.
    ///
    /// # Errors
    /// [`ExitError::InvalidPlan`] for `count` outside `1..=`[`MAX_TRANCHES`], a
    /// `total_bps` outside `1..=10_000`, or a total too small to give every
    /// tranche at least one basis point.
    pub fn evenly_spaced(
        count: u8,
        total_bps: u16,
        stop_loss_bps: Option<u16>,
    ) -> Result<Self, ExitError> {
        let count_usize = usize::from(count);
        if count_usize == 0 || count_usize > MAX_TRANCHES {
            return Err(ExitError::InvalidPlan {
                reason: "tranche count out of range",
            });
        }
        if total_bps == 0 || total_bps > FULL_POSITION_BPS {
            return Err(ExitError::InvalidPlan {
                reason: "total fraction out of range",
            });
        }
        let count_u16 = u16::from(count);
        let share = total_bps.checked_div(count_u16).unwrap_or(0);
        if share == 0 {
            return Err(ExitError::InvalidPlan {
                reason: "total fraction too small for the tranche count",
            });
        }
        let remainder = total_bps.checked_rem(count_u16).unwrap_or(0);
        let span = EXIT_WINDOW_END_MS - EXIT_WINDOW_START_MS;
        let last = count_u16.saturating_sub(1);

        let mut tranches = Vec::with_capacity(count_usize);
        for index in 0..count_u16 {
            let at_ms = if last == 0 {
                EXIT_WINDOW_START_MS
            } else {
                let offset = span
                    .checked_mul(u64::from(index))
                    .and_then(|scaled| scaled.checked_div(u64::from(last)))
                    .ok_or(ExitError::ArithmeticOverflow {
                        context: "tranche time",
                    })?;
                EXIT_WINDOW_START_MS.saturating_add(offset)
            };
            let fraction_bps = if index == last {
                share.saturating_add(remainder)
            } else {
                share
            };
            tranches.push(Tranche {
                at_ms,
                fraction_bps,
            });
        }
        Self::new(tranches, stop_loss_bps)
    }

    /// The schedule, in time order.
    #[must_use]
    pub fn tranches(&self) -> &[Tranche] {
        &self.tranches
    }

    /// The stop-loss threshold, in basis points of price drop.
    #[must_use]
    pub fn stop_loss_bps(&self) -> Option<u16> {
        self.stop_loss_bps
    }

    /// Sum of the tranche fractions, in basis points.
    #[must_use]
    pub fn total_bps(&self) -> u16 {
        let total: u32 = self
            .tranches
            .iter()
            .map(|tranche| u32::from(tranche.fraction_bps))
            .sum();
        u16::try_from(total).unwrap_or(u16::MAX)
    }
}

/// Fee, slippage and spending limits of an exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitSettings {
    /// Assumed total trading fee, in basis points.
    pub fee_bps: u16,
    /// Slippage tolerance of each sell, in basis points
    /// (`0..=`[`MAX_SLIPPAGE_BPS`], the 10 % cap of audit finding #20).
    pub slippage_bps: u16,
    /// Worst-case cost of one order besides the trade: signature fee, priority
    /// fee and tip (see `SellBuilder::order_cost_lamports`).
    pub order_cost_lamports: u64,
    /// Budget for the costs of all orders together.
    pub max_total_cost_lamports: u64,
}

impl ExitSettings {
    /// Checks the bounds.
    ///
    /// # Errors
    /// [`ExitError::InvalidConfig`].
    pub fn validate(&self) -> Result<(), ExitError> {
        if self.slippage_bps > MAX_SLIPPAGE_BPS {
            return Err(ExitError::InvalidConfig {
                reason: "slippage above the 10 % cap",
            });
        }
        if self.fee_bps > MAX_FEE_BPS {
            return Err(ExitError::InvalidConfig {
                reason: "assumed fee above the allowed maximum",
            });
        }
        Ok(())
    }
}

/// The position being liquidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Tokens held, in base units.
    pub tokens: u64,
    /// Price reference for the stop-loss (normally the curve after our own buy,
    /// see [`PriceReference::after_buy`]).
    pub entry: PriceReference,
}

// --- Orders & reports ------------------------------------------------------------------------

/// What caused an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitTrigger {
    /// A scheduled tranche (zero-based index in the plan).
    Tranche {
        /// Index of the tranche.
        index: usize,
    },
    /// The stop-loss fired and the remaining position is sold at once.
    StopLoss,
}

/// A sell ready to be assembled, signed and submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SellOrder {
    /// Unique, increasing number of the order.
    pub sequence: u8,
    /// Why it was issued.
    pub trigger: ExitTrigger,
    /// The quote: amount and slippage-protected `min_sol_output`.
    pub plan: SellPlan,
    /// Milliseconds after genesis at which the order was issued.
    pub issued_at_ms: u64,
    /// Slot of the curve observation the quote is based on.
    pub curve_slot: u64,
    /// Worst-case cost charged against the budget, in lamports.
    pub cost_lamports: u64,
}

/// How an order ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellOutcome {
    /// Landed on chain in `slot`.
    Landed {
        /// Landing slot.
        slot: u64,
    },
    /// Definitely did not land (rejected, errored before submission, reported
    /// failed or invalid by the Block Engine). Its tokens are carried forward.
    Failed,
    /// The submission may or may not have landed: the status polling ran out,
    /// or the send itself timed out after the request left the machine.
    ///
    /// The engine neither counts the tokens as sold nor re-offers them: selling
    /// them again could double-sell and burn tips and priority fees on a
    /// transaction that fails on chain. They stay *unresolved* until the caller
    /// reads the token account and calls [`ExitEngine::resolve_unknown`].
    Unknown,
    /// Dry-run: assembled and signed but deliberately not submitted. Counted as
    /// sold so the schedule can be rehearsed end to end.
    Simulated,
}

/// The executor's verdict on one order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SellReport {
    /// Sequence of the order.
    pub sequence: u8,
    /// Result.
    pub outcome: SellOutcome,
}

/// A tranche (or sweep) that could not be turned into an order.
#[derive(Debug)]
pub struct DroppedOrder {
    /// What was being attempted.
    pub trigger: ExitTrigger,
    /// Why it was dropped.
    pub error: ExitError,
}

/// Result of [`ExitEngine::poll`].
#[derive(Debug, Default)]
pub struct PollResult {
    /// Orders to execute now.
    pub orders: Vec<SellOrder>,
    /// Orders that could not be created.
    pub dropped: Vec<DroppedOrder>,
}

/// Position accounting at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EngineSnapshot {
    /// Orders issued so far.
    pub orders_issued: u32,
    /// Tokens confirmed sold (landed or simulated).
    pub tokens_sold: u64,
    /// Tokens in orders awaiting a report.
    pub tokens_in_flight: u64,
    /// Tokens in orders whose outcome is [`SellOutcome::Unknown`]: neither
    /// counted as sold nor offered again until resolved.
    pub tokens_unresolved: u64,
    /// Tokens neither sold, in flight nor unresolved.
    pub tokens_unsold: u64,
    /// Tranches and sweeps dropped.
    pub dropped: u32,
    /// Whether the stop-loss fired.
    pub stop_loss_fired: bool,
    /// Worst-case cost committed to issued orders, in lamports.
    pub committed_cost_lamports: u64,
}

// --- Engine ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrancheStatus {
    Pending,
    Issued,
    Dropped,
    Superseded,
}

/// Where an issued order stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlightState {
    /// No report yet.
    Awaiting,
    /// Landed in this slot; kept until an observation reflects it.
    Landed(u64),
    /// Outcome unknown; its price impact is kept in the quotes until this slot
    /// (the order can no longer land after it, so observations tell the truth).
    Unresolved { until_slot: u64 },
}

#[derive(Debug, Clone, Copy)]
struct InFlight {
    sequence: u8,
    plan: SellPlan,
    /// Slot of the observation the order was quoted on.
    quoted_slot: u64,
    state: FlightState,
}

/// The synchronous exit state machine. See the module documentation.
#[derive(Debug)]
pub struct ExitEngine {
    plan: ExitPlan,
    settings: ExitSettings,
    position: Position,
    status: Vec<TrancheStatus>,
    unallocated: u64,
    carry: u64,
    sold: u64,
    /// `(sequence, tokens)` of every order reported [`SellOutcome::Unknown`]
    /// and not yet resolved. At most one entry per order issued.
    unresolved: Vec<(u8, u64)>,
    in_flight: Vec<InFlight>,
    latest: Option<CurveObservation>,
    next_sequence: u8,
    committed_cost: u64,
    orders_issued: u32,
    dropped: u32,
    stop_loss_fired: bool,
    stop_loss_orders: u8,
}

impl ExitEngine {
    /// Builds an engine for `position`.
    ///
    /// # Errors
    /// [`ExitError::InvalidConfig`] for invalid settings or an empty position.
    pub fn new(
        plan: ExitPlan,
        settings: ExitSettings,
        position: Position,
    ) -> Result<Self, ExitError> {
        settings.validate()?;
        if position.tokens == 0 {
            return Err(ExitError::InvalidConfig {
                reason: "the position is empty",
            });
        }
        let status = vec![TrancheStatus::Pending; plan.tranches().len()];
        Ok(Self {
            plan,
            settings,
            position,
            status,
            unallocated: position.tokens,
            carry: 0,
            sold: 0,
            unresolved: Vec::new(),
            in_flight: Vec::new(),
            latest: None,
            next_sequence: 0,
            committed_cost: 0,
            orders_issued: 0,
            dropped: 0,
            stop_loss_fired: false,
            stop_loss_orders: 0,
        })
    }

    /// Feeds a curve observation. Returns `false` (and ignores it) when it is
    /// not newer than the one already held, which happens when a reconnection
    /// replays old updates.
    pub fn observe(&mut self, observation: CurveObservation) -> bool {
        if let Some(current) = &self.latest {
            if !observation.is_newer_than(current) {
                return false;
            }
        }
        self.latest = Some(observation);
        self.in_flight.retain(|flight| match flight.state {
            FlightState::Awaiting => true,
            FlightState::Landed(slot) => slot > observation.slot,
            FlightState::Unresolved { until_slot } => until_slot >= observation.slot,
        });
        true
    }

    /// Records the executor's verdict on an order. Unknown sequences are ignored.
    pub fn report(&mut self, report: SellReport) {
        let Some(position) = self
            .in_flight
            .iter()
            .position(|flight| flight.sequence == report.sequence)
        else {
            return;
        };
        let Some(flight) = self.in_flight.get(position).copied() else {
            return;
        };
        if flight.state != FlightState::Awaiting {
            return; // already reported: duplicate reports must not count twice
        }
        let tokens = flight.plan.token_amount;
        match report.outcome {
            SellOutcome::Landed { slot } => {
                self.sold = self.sold.saturating_add(tokens);
                let reflected = self
                    .latest
                    .is_some_and(|observation| observation.slot >= slot);
                if reflected {
                    self.in_flight.remove(position);
                } else if let Some(entry) = self.in_flight.get_mut(position) {
                    entry.state = FlightState::Landed(slot);
                }
            }
            SellOutcome::Simulated => {
                self.sold = self.sold.saturating_add(tokens);
                self.in_flight.remove(position);
            }
            SellOutcome::Failed => {
                self.unallocated = self.unallocated.saturating_add(tokens);
                self.carry = self.carry.saturating_add(tokens);
                self.in_flight.remove(position);
            }
            SellOutcome::Unknown => {
                // Not sold, not re-offered: wait for `resolve_unknown`.
                self.unresolved.push((report.sequence, tokens));
                let until_slot = flight
                    .quoted_slot
                    .saturating_add(UNRESOLVED_PROJECTION_SLOTS);
                if let Some(entry) = self.in_flight.get_mut(position) {
                    entry.state = FlightState::Unresolved { until_slot };
                }
            }
        }
    }

    /// Settles an order that was reported [`SellOutcome::Unknown`], once the
    /// caller has read the token account. `sold` says whether the tokens left
    /// the wallet. Returns `false` if `sequence` is not an unresolved order.
    ///
    /// Tokens that were not sold are returned to the position and offered to
    /// the next pending tranche (or to the stop-loss sweep), like a failure.
    pub fn resolve_unknown(&mut self, sequence: u8, sold: bool) -> bool {
        let Some(position) = self
            .unresolved
            .iter()
            .position(|(unresolved, _)| *unresolved == sequence)
        else {
            return false;
        };
        let (_, tokens) = self.unresolved.remove(position);
        if sold {
            self.sold = self.sold.saturating_add(tokens);
        } else {
            self.unallocated = self.unallocated.saturating_add(tokens);
            self.carry = self.carry.saturating_add(tokens);
            self.in_flight.retain(|flight| flight.sequence != sequence);
        }
        true
    }

    /// Issues every order that is due at `elapsed_ms` after genesis.
    ///
    /// Call it after every observation, every report and every wake-up.
    pub fn poll(&mut self, elapsed_ms: u64) -> PollResult {
        let mut result = PollResult::default();
        if elapsed_ms < EXIT_WINDOW_START_MS {
            return result;
        }
        self.check_stop_loss(elapsed_ms, &mut result);
        if self.stop_loss_fired {
            return result;
        }

        for index in 0..self.plan.tranches().len() {
            if self.status.get(index) != Some(&TrancheStatus::Pending) {
                continue;
            }
            let Some(tranche) = self.plan.tranches().get(index).copied() else {
                continue;
            };
            if elapsed_ms < tranche.at_ms {
                break; // sorted: nothing later is due either
            }
            let target = match self.tranche_target(tranche) {
                Ok(target) => target,
                Err(error) => {
                    self.drop_tranche(index, 0, error, &mut result);
                    continue;
                }
            };
            // `>=` so that waking exactly at the limit drops the tranche instead
            // of re-arming a zero-length timer. Every tranche time is inside the
            // window, so this also bounds the whole window plus the lateness.
            if elapsed_ms >= tranche.at_ms.saturating_add(MAX_LATENESS_MS) {
                let tranche_index = u8::try_from(index).unwrap_or(u8::MAX);
                let error = ExitError::WindowMissed {
                    tranche: tranche_index,
                };
                self.drop_tranche(index, target, error, &mut result);
                continue;
            }
            if self.latest.is_none() {
                continue; // wait for the first observation, until the lateness limit
            }
            let tokens = self.tokens_for(index, target);
            match self.issue(ExitTrigger::Tranche { index }, tokens, elapsed_ms) {
                Ok(order) => {
                    if let Some(slot) = self.status.get_mut(index) {
                        *slot = TrancheStatus::Issued;
                    }
                    self.carry = 0;
                    result.orders.push(order);
                }
                Err(error) => self.drop_tranche(index, target, error, &mut result),
            }
        }
        result
    }

    /// Milliseconds after genesis at which the engine next needs to run even if
    /// no update arrives, or `None` when no tranche is pending.
    #[must_use]
    pub fn next_wakeup_ms(&self, elapsed_ms: u64) -> Option<u64> {
        let index = self
            .status
            .iter()
            .position(|status| *status == TrancheStatus::Pending)?;
        let tranche = self.plan.tranches().get(index)?;
        Some(if elapsed_ms < tranche.at_ms {
            tranche.at_ms
        } else {
            tranche.at_ms.saturating_add(MAX_LATENESS_MS)
        })
    }

    /// Whether any tranche is still waiting for its time.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.status.contains(&TrancheStatus::Pending)
    }

    /// Whether any order still awaits its execution report.
    #[must_use]
    pub fn has_unreported(&self) -> bool {
        self.in_flight
            .iter()
            .any(|flight| flight.state == FlightState::Awaiting)
    }

    /// Position accounting.
    #[must_use]
    pub fn snapshot(&self) -> EngineSnapshot {
        let in_flight_tokens = self
            .in_flight
            .iter()
            .filter(|flight| flight.state == FlightState::Awaiting)
            .fold(0u64, |sum, flight| {
                sum.saturating_add(flight.plan.token_amount)
            });
        let unresolved_tokens = self
            .unresolved
            .iter()
            .fold(0u64, |sum, (_, tokens)| sum.saturating_add(*tokens));
        EngineSnapshot {
            orders_issued: self.orders_issued,
            tokens_sold: self.sold,
            tokens_in_flight: in_flight_tokens,
            tokens_unresolved: unresolved_tokens,
            tokens_unsold: self.unallocated,
            dropped: self.dropped,
            stop_loss_fired: self.stop_loss_fired,
            committed_cost_lamports: self.committed_cost,
        }
    }

    // --- internals ---

    /// Fires the stop-loss, or keeps sweeping after it fired.
    ///
    /// Once the price falls below the stop, every pending tranche is superseded
    /// and all unallocated tokens go out in one order. If that order fails, its
    /// tokens come back and are swept again (at most
    /// [`MAX_STOP_LOSS_ORDERS`] times), whatever the price does meanwhile.
    fn check_stop_loss(&mut self, elapsed_ms: u64, result: &mut PollResult) {
        if self.unallocated == 0
            || self.stop_loss_orders >= MAX_STOP_LOSS_ORDERS
            || elapsed_ms >= EXIT_WINDOW_END_MS.saturating_add(MAX_LATENESS_MS)
        {
            return;
        }
        let Some(observation) = self.latest else {
            return;
        };
        if observation.state.complete {
            return;
        }
        if !self.stop_loss_fired {
            let Some(stop) = self.plan.stop_loss_bps() else {
                return;
            };
            // An undefined ratio (zero reserves) is not a trigger: the quote
            // below would fail anyway.
            let Ok(ratio) = observation.state.price_ratio_bps(&self.position.entry) else {
                return;
            };
            let floor = BPS_DENOMINATOR.saturating_sub(u128::from(stop));
            if ratio >= floor {
                return;
            }
            self.stop_loss_fired = true;
            for status in &mut self.status {
                if *status == TrancheStatus::Pending {
                    *status = TrancheStatus::Superseded;
                }
            }
        }

        let tokens = self.unallocated;
        match self.issue(ExitTrigger::StopLoss, tokens, elapsed_ms) {
            Ok(order) => {
                self.stop_loss_orders = self.stop_loss_orders.saturating_add(1);
                self.carry = 0;
                result.orders.push(order);
            }
            Err(error) => {
                // Count the attempt so a persistent failure cannot loop forever.
                self.stop_loss_orders = self.stop_loss_orders.saturating_add(1);
                self.dropped = self.dropped.saturating_add(1);
                result.dropped.push(DroppedOrder {
                    trigger: ExitTrigger::StopLoss,
                    error,
                });
            }
        }
    }

    /// `floor(position * fraction / 10_000)` for one tranche.
    fn tranche_target(&self, tranche: Tranche) -> Result<u64, ExitError> {
        let overflow = ExitError::ArithmeticOverflow {
            context: "tranche amount",
        };
        let scaled = u128::from(self.position.tokens)
            .checked_mul(u128::from(tranche.fraction_bps))
            .and_then(|value| value.checked_div(BPS_DENOMINATOR))
            .ok_or(overflow)?;
        u64::try_from(scaled).map_err(|_| ExitError::ArithmeticOverflow {
            context: "tranche amount",
        })
    }

    /// Tokens an order for tranche `index` sells: its own share plus whatever
    /// earlier failures carried over, capped at what is unallocated. The last
    /// pending tranche of a full-exit plan sweeps the rounding dust as well.
    fn tokens_for(&self, index: usize, target: u64) -> u64 {
        let later_pending = self
            .status
            .get(index.saturating_add(1)..)
            .is_some_and(|rest| rest.contains(&TrancheStatus::Pending));
        let full_exit = self.plan.total_bps() == FULL_POSITION_BPS;
        if full_exit && !later_pending {
            self.unallocated
        } else {
            target.saturating_add(self.carry).min(self.unallocated)
        }
    }

    fn drop_tranche(
        &mut self,
        index: usize,
        target: u64,
        error: ExitError,
        result: &mut PollResult,
    ) {
        if let Some(status) = self.status.get_mut(index) {
            *status = TrancheStatus::Dropped;
        }
        // The share was never allocated; offer it to the next tranche.
        self.carry = self.carry.saturating_add(target);
        self.dropped = self.dropped.saturating_add(1);
        result.dropped.push(DroppedOrder {
            trigger: ExitTrigger::Tranche { index },
            error,
        });
    }

    /// Reserves `tokens` and builds the order. Nothing is mutated unless every
    /// check passes.
    fn issue(
        &mut self,
        trigger: ExitTrigger,
        tokens: u64,
        elapsed_ms: u64,
    ) -> Result<SellOrder, ExitError> {
        let observation = self.latest.ok_or(ExitError::NoCurveState)?;
        if observation.state.complete {
            return Err(ExitError::CurveComplete);
        }
        if observation.state.is_cashback_coin {
            return Err(ExitError::CashbackCoinUnsupported);
        }

        // Quote against the observation plus the impact of unresolved sales.
        let mut curve = observation.state.curve_params();
        for flight in &self.in_flight {
            curve = flight.plan.apply_to(&curve)?;
        }
        let plan = SellPlan::from_token_amount(
            &curve,
            tokens,
            self.settings.fee_bps,
            self.settings.slippage_bps,
        )?;

        let cost = self.settings.order_cost_lamports;
        let remaining = self
            .settings
            .max_total_cost_lamports
            .saturating_sub(self.committed_cost);
        if cost > remaining {
            return Err(ExitError::BudgetExceeded { cost, remaining });
        }
        let overflow = || ExitError::ArithmeticOverflow {
            context: "order bookkeeping",
        };
        let committed = self.committed_cost.checked_add(cost).ok_or_else(overflow)?;
        let unallocated = self.unallocated.checked_sub(tokens).ok_or_else(overflow)?;
        let sequence = self.next_sequence;
        let next_sequence = sequence.checked_add(1).ok_or_else(overflow)?;
        let issued = self.orders_issued.checked_add(1).ok_or_else(overflow)?;

        self.committed_cost = committed;
        self.unallocated = unallocated;
        self.next_sequence = next_sequence;
        self.orders_issued = issued;
        self.in_flight.push(InFlight {
            sequence,
            plan,
            quoted_slot: observation.slot,
            state: FlightState::Awaiting,
        });
        Ok(SellOrder {
            sequence,
            trigger,
            plan,
            issued_at_ms: elapsed_ms,
            curve_slot: observation.slot,
            cost_lamports: cost,
        })
    }
}

// --- Channels ------------------------------------------------------------------------------------

/// The channel carrying orders from the manager to the executor. Its capacity
/// is [`ORDER_CHANNEL_CAPACITY`], above the most orders one engine can issue, so
/// the manager's non-blocking `try_send` cannot hit a full queue.
#[must_use]
pub fn order_channel() -> (mpsc::Sender<SellOrder>, mpsc::Receiver<SellOrder>) {
    mpsc::channel(ORDER_CHANNEL_CAPACITY)
}

/// The channel carrying execution reports back to the manager.
#[must_use]
pub fn report_channel() -> (mpsc::Sender<SellReport>, mpsc::Receiver<SellReport>) {
    mpsc::channel(REPORT_CHANNEL_CAPACITY)
}

// --- Manager ---------------------------------------------------------------------------------------

/// Final accounting of an exit run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitSummary {
    /// Position accounting at the end.
    pub position: EngineSnapshot,
    /// Curve observations applied.
    pub observations: u64,
    /// Account updates for the curve that failed to decode.
    pub decode_errors: u64,
    /// The run ended because of a shutdown request, not because it was done.
    pub interrupted: bool,
    /// The run ended with orders still unreported (settle timeout, or the
    /// executor went away).
    pub unsettled: bool,
}

/// Drives an [`ExitEngine`] from a stream of updates.
#[derive(Debug)]
pub struct ExitManager {
    engine: ExitEngine,
    curve: Pubkey,
    genesis: Instant,
    settle_timeout: Duration,
    metrics: Option<Arc<PipelineMetrics>>,
    /// When the update that has not been acted on yet was received.
    pending_ingest: Option<std::time::Instant>,
}

impl ExitManager {
    /// `curve` is the bonding curve account whose updates drive the engine;
    /// `genesis` is the instant the launch transaction was submitted (the zero
    /// of the execution window).
    #[must_use]
    pub fn new(engine: ExitEngine, curve: Pubkey, genesis: Instant) -> Self {
        Self {
            engine,
            curve,
            genesis,
            settle_timeout: DEFAULT_SETTLE_TIMEOUT,
            metrics: None,
            pending_ingest: None,
        }
    }

    /// Records pipeline latencies into `metrics` (see [`crate::metrics`]): the
    /// instant each curve update is received, and the instant each order is
    /// issued. The submitter completes the picture with
    /// [`PipelineMetrics::mark_assembled`].
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<PipelineMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Overrides how long to wait for reports after the window closes.
    #[must_use]
    pub fn with_settle_timeout(mut self, settle_timeout: Duration) -> Self {
        self.settle_timeout = settle_timeout;
        self
    }

    fn elapsed_ms(&self) -> u64 {
        let elapsed = Instant::now().saturating_duration_since(self.genesis);
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    }

    /// Runs until every tranche is resolved and every order reported, the
    /// settle timeout expires, or shutdown is requested.
    ///
    /// Shutdown stops new orders; it does **not** liquidate the remainder (a
    /// panic-sell on Ctrl-C would be a surprise). The unsold amount is in the
    /// returned summary and logged.
    ///
    /// # Errors
    /// [`ExitError::OrderChannelClosed`] when the executor is gone while orders
    /// are due; everything else is absorbed into the summary.
    pub async fn run(
        mut self,
        mut updates: mpsc::Receiver<StreamUpdate>,
        orders: mpsc::Sender<SellOrder>,
        mut reports: mpsc::Receiver<SellReport>,
        mut shutdown: ShutdownSignal,
    ) -> Result<ExitSummary, ExitError> {
        let hard_deadline = self.genesis
            + Duration::from_millis(EXIT_WINDOW_END_MS.saturating_add(MAX_LATENESS_MS))
            + self.settle_timeout;
        let mut updates_open = true;
        let mut reports_open = true;
        let mut observations: u64 = 0;
        let mut decode_errors: u64 = 0;
        let mut interrupted = false;
        let mut unsettled = false;

        loop {
            let elapsed = self.elapsed_ms();
            self.dispatch(elapsed, &orders)?;

            let waiting_for_reports = self.engine.has_unreported() && reports_open;
            if !self.engine.has_pending() && !waiting_for_reports {
                unsettled = self.engine.has_unreported();
                break;
            }

            let wake = self
                .engine
                .next_wakeup_ms(elapsed)
                .map(|ms| self.genesis + Duration::from_millis(ms))
                .map_or(hard_deadline, |at| at.min(hard_deadline));

            tokio::select! {
                biased;
                () = shutdown.wait() => {
                    interrupted = true;
                    break;
                }
                update = updates.recv(), if updates_open => match update {
                    Some(update) => {
                        self.handle_update(&update, &mut observations, &mut decode_errors);
                    }
                    None => {
                        updates_open = false;
                        warn!("update stream closed; continuing on the last known curve state");
                    }
                },
                report = reports.recv(), if reports_open => match report {
                    Some(report) => self.engine.report(report),
                    None => reports_open = false,
                },
                () = sleep_until(wake) => {
                    if Instant::now() >= hard_deadline {
                        unsettled = self.engine.has_unreported();
                        break;
                    }
                }
            }
        }

        // Counts and flags only: amounts and balances never reach the logs.
        let snapshot = self.engine.snapshot();
        if snapshot.tokens_unsold > 0
            || snapshot.tokens_in_flight > 0
            || snapshot.tokens_unresolved > 0
        {
            warn!(
                orders = snapshot.orders_issued,
                dropped = snapshot.dropped,
                unresolved = snapshot.tokens_unresolved > 0,
                interrupted,
                "exit finished with part of the position not confirmed sold"
            );
        } else {
            info!(orders = snapshot.orders_issued, "exit finished");
        }
        Ok(ExitSummary {
            position: snapshot,
            observations,
            decode_errors,
            interrupted,
            unsettled,
        })
    }

    fn handle_update(
        &mut self,
        update: &StreamUpdate,
        observations: &mut u64,
        decode_errors: &mut u64,
    ) {
        let StreamUpdate::Account(account) = update else {
            return;
        };
        if account.pubkey != self.curve {
            return;
        }
        // Taken before decoding, so decode and decision time are measured too.
        let received = std::time::Instant::now();
        match CurveObservation::from_account(account) {
            Ok(observation) => {
                if self.engine.observe(observation) {
                    *observations = observations.saturating_add(1);
                    self.pending_ingest = Some(received);
                    if let Some(metrics) = &self.metrics {
                        metrics.count_observation();
                    }
                }
            }
            Err(error) => {
                *decode_errors = decode_errors.saturating_add(1);
                warn!(%error, "ignoring undecodable bonding curve update");
            }
        }
    }

    /// Polls the engine and hands the orders to the executor without blocking.
    fn dispatch(
        &mut self,
        elapsed_ms: u64,
        orders: &mpsc::Sender<SellOrder>,
    ) -> Result<(), ExitError> {
        let result = self.engine.poll(elapsed_ms);
        // Orders issued right after an update are its reaction; the others were
        // fired by the schedule's timer (or by a report).
        let ingest = self.pending_ingest.take();
        for dropped in &result.dropped {
            warn!(
                trigger = ?dropped.trigger,
                kind = dropped.error.label(),
                "exit order dropped"
            );
        }
        let mut closed = false;
        for order in result.orders {
            debug!(sequence = order.sequence, "dispatching sell order");
            if let Some(metrics) = &self.metrics {
                metrics.mark_order(order.sequence, ingest);
            }
            match orders.try_send(order) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(order)) => {
                    warn!(
                        sequence = order.sequence,
                        "order channel full; order failed"
                    );
                    self.engine.report(SellReport {
                        sequence: order.sequence,
                        outcome: SellOutcome::Failed,
                    });
                }
                Err(mpsc::error::TrySendError::Closed(order)) => {
                    self.engine.report(SellReport {
                        sequence: order.sequence,
                        outcome: SellOutcome::Failed,
                    });
                    closed = true;
                }
            }
        }
        if closed {
            return Err(ExitError::OrderChannelClosed);
        }
        Ok(())
    }
}

// --- Executor ------------------------------------------------------------------------------------------

/// Turns a [`SellOrder`] into a submitted transaction and reports how it ended.
pub trait SellSubmitter: Send + Sync + 'static {
    /// Assembles, signs and submits the order, then waits for its outcome.
    ///
    /// Errors are mapped to [`SellOutcome::Failed`] by the executor, so an
    /// implementation must return `Ok(SellOutcome::Unknown)` (not an error) for
    /// every case in which the order may already have been accepted.
    fn submit(
        &self,
        order: &SellOrder,
    ) -> impl std::future::Future<Output = Result<SellOutcome, ExitError>> + Send;
}

/// Receives orders and submits each one on its own task, so a slow submission
/// never delays the next tranche. Runs until the order channel closes or
/// shutdown is requested, then waits up to `drain_timeout` for submissions in
/// progress. Orders still queued at shutdown are discarded.
pub async fn run_sell_executor<S: SellSubmitter>(
    submitter: Arc<S>,
    mut orders: mpsc::Receiver<SellOrder>,
    reports: mpsc::Sender<SellReport>,
    mut shutdown: ShutdownSignal,
    drain_timeout: Duration,
) {
    let mut tasks: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = shutdown.wait() => break,
            order = orders.recv() => match order {
                None => break,
                Some(order) => {
                    let submitter = Arc::clone(&submitter);
                    let reports = reports.clone();
                    tasks.spawn(async move {
                        let outcome = match submitter.submit(&order).await {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                // The label never carries amounts; the Block
                                // Engine text is sanitized by `JitoError`.
                                match &error {
                                    ExitError::Jito(detail) => warn!(
                                        sequence = order.sequence,
                                        kind = error.label(),
                                        %detail,
                                        "sell submission failed"
                                    ),
                                    _ => warn!(
                                        sequence = order.sequence,
                                        kind = error.label(),
                                        "sell submission failed"
                                    ),
                                }
                                SellOutcome::Failed
                            }
                        };
                        if outcome == SellOutcome::Unknown {
                            warn!(
                                sequence = order.sequence,
                                "sell outcome unknown; not resubmitting until the balance is checked"
                            );
                        }
                        // The manager may already be gone; nothing else to do then.
                        let _ = reports
                            .send(SellReport { sequence: order.sequence, outcome })
                            .await;
                    });
                }
            },
            // Reap finished submissions so the set does not grow.
            _joined = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    let drain = async { while tasks.join_next().await.is_some() {} };
    if timeout(drain_timeout, drain).await.is_err() {
        warn!("sell executor drain timed out; aborting submissions in progress");
        tasks.abort_all();
    }
}
