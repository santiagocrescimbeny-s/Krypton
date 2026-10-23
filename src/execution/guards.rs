//! Strict capital limits for live trading (Phase 7).
//!
//! [`SpendingGuards`] sits between the strategy and the Block Engine. Nothing
//! that spends money is sent without first obtaining a [`Reservation`]:
//!
//! - **Per-transaction cap.** One order never commits more than
//!   [`GuardLimits::max_per_tx_lamports`], whatever its kind.
//! - **Daily drawdown.** Entries (launches and buys) are refused when the day's
//!   *net* spend, committed plus reserved minus what sells returned, would pass
//!   [`GuardLimits::daily_drawdown_lamports`]. When realised net spend reaches
//!   the limit the guards **latch** ([`GuardError::Halted`]) and stay halted
//!   across day rollovers until [`SpendingGuards::rearm`] is called on purpose.
//! - **Exits are never blocked** by the halt or the drawdown: refusing a sell
//!   would trap the position. They still count towards spend and obey the
//!   per-transaction cap (a sell costs only fees and a tip).
//! - **Slippage** is capped at 10 % ([`MAX_SLIPPAGE_BPS`]).
//!
//! A reservation is taken *before* the order is built and settled afterwards:
//! [`SpendingGuards::commit`] when it landed or may have landed,
//! [`SpendingGuards::release`] when it definitely did not. Reservations count
//! as spent while open, so concurrent orders cannot jointly overshoot.
//!
//! Time is an argument (`now_secs`, seconds since the Unix epoch), never read
//! here, which keeps the guards deterministic and testable.

use std::sync::Mutex;

use crate::config::MAX_SLIPPAGE_BPS;
use crate::errors::GuardError;

use super::settings::{ABSOLUTE_MAX_DAILY_DRAWDOWN_LAMPORTS, ABSOLUTE_MAX_SPEND_PER_TX_LAMPORTS};

/// Seconds in the accounting day (UTC).
pub const SECONDS_PER_DAY: u64 = 86_400;

/// The configured limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardLimits {
    max_per_tx_lamports: u64,
    daily_drawdown_lamports: u64,
    max_slippage_bps: u16,
}

impl GuardLimits {
    /// Validates the limits.
    ///
    /// # Errors
    /// [`GuardError::InvalidLimits`] when a limit is zero, above its absolute
    /// ceiling, or when the daily limit is below the per-transaction cap (a
    /// single allowed order would already break the day);
    /// [`GuardError::SlippageTooHigh`] when `max_slippage_bps` exceeds 10 %.
    pub fn new(
        max_per_tx_lamports: u64,
        daily_drawdown_lamports: u64,
        max_slippage_bps: u16,
    ) -> Result<Self, GuardError> {
        if max_per_tx_lamports == 0 || daily_drawdown_lamports == 0 {
            return Err(GuardError::InvalidLimits {
                reason: "limits must be greater than zero",
            });
        }
        if max_per_tx_lamports > ABSOLUTE_MAX_SPEND_PER_TX_LAMPORTS {
            return Err(GuardError::InvalidLimits {
                reason: "per-transaction cap above its absolute ceiling",
            });
        }
        if daily_drawdown_lamports > ABSOLUTE_MAX_DAILY_DRAWDOWN_LAMPORTS {
            return Err(GuardError::InvalidLimits {
                reason: "daily drawdown above its absolute ceiling",
            });
        }
        if daily_drawdown_lamports < max_per_tx_lamports {
            return Err(GuardError::InvalidLimits {
                reason: "daily drawdown below the per-transaction cap",
            });
        }
        if max_slippage_bps > MAX_SLIPPAGE_BPS {
            return Err(GuardError::SlippageTooHigh);
        }
        Ok(Self {
            max_per_tx_lamports,
            daily_drawdown_lamports,
            max_slippage_bps,
        })
    }

    /// Per-transaction cap, in lamports.
    #[must_use]
    pub fn max_per_tx_lamports(&self) -> u64 {
        self.max_per_tx_lamports
    }

    /// Daily drawdown limit, in lamports.
    #[must_use]
    pub fn daily_drawdown_lamports(&self) -> u64 {
        self.daily_drawdown_lamports
    }

    /// Slippage ceiling, in basis points.
    #[must_use]
    pub fn max_slippage_bps(&self) -> u16 {
        self.max_slippage_bps
    }
}

/// What an order is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendKind {
    /// Opens or grows a position: subject to the halt and the drawdown.
    Entry,
    /// Reduces a position: only the per-transaction cap applies.
    Exit,
}

/// Money set aside for one order. Consumed by [`SpendingGuards::commit`] or
/// [`SpendingGuards::release`]; it cannot be copied, so it cannot be settled
/// twice.
#[derive(Debug)]
#[must_use = "a reservation must be committed or released"]
pub struct Reservation {
    lamports: u64,
    kind: SpendKind,
}

impl Reservation {
    /// Lamports set aside.
    #[must_use]
    pub fn lamports(&self) -> u64 {
        self.lamports
    }

    /// What the order is for.
    #[must_use]
    pub fn kind(&self) -> SpendKind {
        self.kind
    }
}

/// A point-in-time copy of the accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuardSnapshot {
    /// Day index (`now / 86_400`) the counters belong to.
    pub day: u64,
    /// Spend of orders that landed or may have landed, this day.
    pub committed_lamports: u64,
    /// Spend of orders in flight.
    pub reserved_lamports: u64,
    /// Proceeds of sells that landed, this day.
    pub returned_lamports: u64,
    /// Net spend: committed, minus the part of the proceeds that offsets it.
    pub realised_drawdown_lamports: u64,
    /// The drawdown tripwire has fired.
    pub halted: bool,
}

#[derive(Debug, Default)]
struct State {
    day: Option<u64>,
    committed: u64,
    reserved: u64,
    returned: u64,
    halted: bool,
}

impl State {
    fn roll(&mut self, now_secs: u64) {
        let day = now_secs / SECONDS_PER_DAY;
        if self.day != Some(day) {
            self.day = Some(day);
            self.committed = 0;
            self.returned = 0;
            // `reserved` (orders still in flight) and `halted` survive.
        }
    }

    /// Committed spend not offset by returns (returns never go below zero).
    fn realised(&self) -> u64 {
        self.committed
            .saturating_sub(self.returned.min(self.committed))
    }

    fn snapshot(&self) -> GuardSnapshot {
        GuardSnapshot {
            day: self.day.unwrap_or(0),
            committed_lamports: self.committed,
            reserved_lamports: self.reserved,
            returned_lamports: self.returned,
            realised_drawdown_lamports: self.realised(),
            halted: self.halted,
        }
    }
}

/// Enforces [`GuardLimits`]. Share it behind an `Arc`.
#[derive(Debug)]
pub struct SpendingGuards {
    limits: GuardLimits,
    state: Mutex<State>,
}

impl SpendingGuards {
    /// Guards with empty books.
    #[must_use]
    pub fn new(limits: GuardLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(State::default()),
        }
    }

    /// The configured limits.
    #[must_use]
    pub fn limits(&self) -> GuardLimits {
        self.limits
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>, GuardError> {
        self.state.lock().map_err(|_| GuardError::StatePoisoned)
    }

    /// Checks a slippage tolerance against the 10 % cap.
    ///
    /// # Errors
    /// [`GuardError::SlippageTooHigh`].
    pub fn check_slippage(&self, slippage_bps: u16) -> Result<(), GuardError> {
        if slippage_bps > self.limits.max_slippage_bps {
            Err(GuardError::SlippageTooHigh)
        } else {
            Ok(())
        }
    }

    /// Sets `lamports` aside for one order.
    ///
    /// # Errors
    /// - [`GuardError::PerTransactionCap`] above the per-transaction cap;
    /// - [`GuardError::Halted`] for an entry after the tripwire fired;
    /// - [`GuardError::DailyDrawdown`] for an entry that would take the net spend
    ///   past the daily limit;
    /// - [`GuardError::StatePoisoned`].
    pub fn reserve(
        &self,
        kind: SpendKind,
        lamports: u64,
        now_secs: u64,
    ) -> Result<Reservation, GuardError> {
        if lamports > self.limits.max_per_tx_lamports {
            return Err(GuardError::PerTransactionCap);
        }
        let mut state = self.lock()?;
        state.roll(now_secs);
        if kind == SpendKind::Entry {
            if state.halted {
                return Err(GuardError::Halted);
            }
            let projected = state
                .realised()
                .saturating_add(state.reserved)
                .saturating_add(lamports);
            if projected > self.limits.daily_drawdown_lamports {
                return Err(GuardError::DailyDrawdown);
            }
        }
        state.reserved = state.reserved.saturating_add(lamports);
        Ok(Reservation { lamports, kind })
    }

    /// The order landed or may have landed: the money counts as spent. Fires the
    /// tripwire when the realised net spend reaches the daily limit.
    ///
    /// # Errors
    /// [`GuardError::StatePoisoned`] (the reservation is then lost; the books
    /// are unusable anyway).
    pub fn commit(&self, reservation: Reservation, now_secs: u64) -> Result<(), GuardError> {
        let mut state = self.lock()?;
        state.roll(now_secs);
        state.reserved = state.reserved.saturating_sub(reservation.lamports);
        state.committed = state.committed.saturating_add(reservation.lamports);
        if state.realised() >= self.limits.daily_drawdown_lamports {
            state.halted = true;
        }
        Ok(())
    }

    /// The order definitely did not happen: the money is free again.
    ///
    /// # Errors
    /// [`GuardError::StatePoisoned`].
    pub fn release(&self, reservation: Reservation) -> Result<(), GuardError> {
        let mut state = self.lock()?;
        state.reserved = state.reserved.saturating_sub(reservation.lamports);
        Ok(())
    }

    /// Records the net proceeds of a sell that landed. They offset the day's
    /// spend but never lift a latched halt.
    ///
    /// # Errors
    /// [`GuardError::StatePoisoned`].
    pub fn record_return(&self, lamports: u64, now_secs: u64) -> Result<(), GuardError> {
        let mut state = self.lock()?;
        state.roll(now_secs);
        state.returned = state.returned.saturating_add(lamports);
        Ok(())
    }

    /// Clears the halt latch. Call it only after a human has looked at why the
    /// tripwire fired.
    ///
    /// # Errors
    /// [`GuardError::StatePoisoned`].
    pub fn rearm(&self) -> Result<(), GuardError> {
        self.lock()?.halted = false;
        Ok(())
    }

    /// Whether new entries are blocked by the tripwire.
    ///
    /// # Errors
    /// [`GuardError::StatePoisoned`].
    pub fn is_halted(&self) -> Result<bool, GuardError> {
        Ok(self.lock()?.halted)
    }

    /// A copy of the books.
    ///
    /// # Errors
    /// [`GuardError::StatePoisoned`].
    pub fn snapshot(&self) -> Result<GuardSnapshot, GuardError> {
        Ok(self.lock()?.snapshot())
    }
}
