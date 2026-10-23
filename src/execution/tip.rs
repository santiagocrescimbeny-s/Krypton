//! Dynamic Jito tip management (Phase 7).
//!
//! A bundle that must land in the first block competes on its tip, but a tip
//! that follows every fee spike burns money for nothing. [`TipPolicy`] turns the
//! market's recently landed tips ([`TipFloor`]) into one amount, in this order:
//!
//! 1. **Market.** The percentile that matches the urgency: a launch wants block
//!    zero ([`Urgency::Launch`], p95 by default); an exit order is one of
//!    several ([`Urgency::Exit`], p75).
//! 2. **Spike damping.** The result never exceeds `spike_multiple_bps` of the
//!    EMA of the median landed tip (3x by default), though never below the
//!    operator's fallback tip. A spike then raises the bid, but not unboundedly.
//! 3. **Notional cap.** The tip never exceeds `max_share_bps` of the trade's
//!    value (5 % by default), so a small order cannot overpay.
//! 4. **Hard cap.** `MAX_JITO_TIP_LAMPORTS`, itself bounded by
//!    `ABSOLUTE_MAX_JITO_TIP_LAMPORTS`. This one always wins.
//!
//! With no floor, or one older than [`TipPolicy::max_age`], the quote is the
//! configured `JITO_TIP_LAMPORTS` (clamped the same way): a stale market view is
//! never trusted for a bid.
//!
//! The policy is pure. [`TipBook`] adds the one piece of state, the latest floor
//! and when it was observed; fill it from any source (see [`super::tip_feed`]).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{ABSOLUTE_MAX_JITO_TIP_LAMPORTS, MIN_JITO_TIP_LAMPORTS};
use crate::errors::LiveError;

/// Default age beyond which a floor is not trusted.
pub const DEFAULT_MAX_FLOOR_AGE: Duration = Duration::from_secs(30);
/// Default spike ceiling: 3x the EMA of the median landed tip.
pub const DEFAULT_SPIKE_MULTIPLE_BPS: u32 = 30_000;
/// Default share of the trade's value a tip may take: 5 %.
pub const DEFAULT_MAX_SHARE_BPS: u32 = 500;
const BPS: u128 = 10_000;

/// Recently landed tips, in lamports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TipFloor {
    /// Median landed tip.
    pub p50: u64,
    /// 75th percentile.
    pub p75: u64,
    /// 95th percentile.
    pub p95: u64,
    /// 99th percentile.
    pub p99: u64,
    /// Exponential moving average of the median: the calm-market baseline.
    pub ema_p50: u64,
}

/// A percentile of [`TipFloor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Percentile {
    /// Median.
    P50,
    /// 75th.
    P75,
    /// 95th.
    P95,
    /// 99th.
    P99,
}

impl TipFloor {
    /// The value of `percentile`.
    #[must_use]
    pub fn at(&self, percentile: Percentile) -> u64 {
        match percentile {
            Percentile::P50 => self.p50,
            Percentile::P75 => self.p75,
            Percentile::P95 => self.p95,
            Percentile::P99 => self.p99,
        }
    }
}

/// How badly a bundle needs to land now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// The launch bundle: block zero or nothing.
    Launch,
    /// A tranche or stop-loss sell.
    Exit,
}

/// Which rule decided the amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipBasis {
    /// The market percentile, untouched.
    Market,
    /// The market percentile, cut to the spike ceiling.
    SpikeDamped,
    /// Cut to the share of the trade's value.
    NotionalCapped,
    /// Cut to the hard cap.
    HardCapped,
    /// No usable floor: the configured tip.
    Fallback,
}

/// The amount and the rule behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TipQuote {
    /// The tip, in lamports (within the Jito minimum and the hard cap).
    pub lamports: u64,
    /// The deciding rule.
    pub basis: TipBasis,
}

/// Parameters of the tip calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TipPolicy {
    fallback_lamports: u64,
    cap_lamports: u64,
    spike_multiple_bps: u32,
    max_share_bps: u32,
    max_age: Duration,
    launch_percentile: Percentile,
    exit_percentile: Percentile,
}

fn mul_div(value: u64, numerator: u128, denominator: u128) -> u64 {
    let scaled = u128::from(value).saturating_mul(numerator);
    u64::try_from(scaled.checked_div(denominator).unwrap_or(0)).unwrap_or(u64::MAX)
}

impl TipPolicy {
    /// A policy around the configured tip (`JITO_TIP_LAMPORTS`) and its cap
    /// (`MAX_JITO_TIP_LAMPORTS`).
    ///
    /// # Errors
    /// [`LiveError::InvalidTipPolicy`] when the fallback is below the Jito
    /// minimum, the cap is above [`ABSOLUTE_MAX_JITO_TIP_LAMPORTS`], or the
    /// fallback exceeds the cap.
    pub fn new(fallback_lamports: u64, cap_lamports: u64) -> Result<Self, LiveError> {
        if fallback_lamports < MIN_JITO_TIP_LAMPORTS {
            return Err(LiveError::InvalidTipPolicy {
                reason: "fallback tip below the Jito minimum",
            });
        }
        if cap_lamports > ABSOLUTE_MAX_JITO_TIP_LAMPORTS {
            return Err(LiveError::InvalidTipPolicy {
                reason: "tip cap above the absolute ceiling",
            });
        }
        if fallback_lamports > cap_lamports {
            return Err(LiveError::InvalidTipPolicy {
                reason: "fallback tip above the cap",
            });
        }
        Ok(Self {
            fallback_lamports,
            cap_lamports,
            spike_multiple_bps: DEFAULT_SPIKE_MULTIPLE_BPS,
            max_share_bps: DEFAULT_MAX_SHARE_BPS,
            max_age: DEFAULT_MAX_FLOOR_AGE,
            launch_percentile: Percentile::P95,
            exit_percentile: Percentile::P75,
        })
    }

    /// Sets the spike ceiling, as a multiple of the EMA median in basis points
    /// (`30_000` = 3x).
    ///
    /// # Errors
    /// [`LiveError::InvalidTipPolicy`] below 1x.
    pub fn with_spike_multiple_bps(mut self, bps: u32) -> Result<Self, LiveError> {
        if u128::from(bps) < BPS {
            return Err(LiveError::InvalidTipPolicy {
                reason: "spike multiple below 1x",
            });
        }
        self.spike_multiple_bps = bps;
        Ok(self)
    }

    /// Sets the share of a trade's value a tip may take, in basis points.
    ///
    /// # Errors
    /// [`LiveError::InvalidTipPolicy`] for zero or above 100 %.
    pub fn with_max_share_bps(mut self, bps: u32) -> Result<Self, LiveError> {
        if bps == 0 || u128::from(bps) > BPS {
            return Err(LiveError::InvalidTipPolicy {
                reason: "notional share out of range",
            });
        }
        self.max_share_bps = bps;
        Ok(self)
    }

    /// Sets how long a floor stays trustworthy.
    #[must_use]
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// Sets the percentiles used for launches and exits.
    #[must_use]
    pub fn with_percentiles(mut self, launch: Percentile, exit: Percentile) -> Self {
        self.launch_percentile = launch;
        self.exit_percentile = exit;
        self
    }

    /// How long a floor stays trustworthy.
    #[must_use]
    pub fn max_age(&self) -> Duration {
        self.max_age
    }

    /// The hard cap, in lamports.
    #[must_use]
    pub fn cap_lamports(&self) -> u64 {
        self.cap_lamports
    }

    /// The configured tip used without a usable floor.
    #[must_use]
    pub fn fallback_lamports(&self) -> u64 {
        self.fallback_lamports
    }

    /// Quotes a tip.
    ///
    /// `floor` is the latest floor with its age; `notional_lamports` is the value
    /// of the trade the bundle carries (`0` skips the notional cap). The result
    /// is always within `MIN_JITO_TIP_LAMPORTS..=cap`.
    #[must_use]
    pub fn quote(
        &self,
        floor: Option<(&TipFloor, Duration)>,
        urgency: Urgency,
        notional_lamports: u64,
    ) -> TipQuote {
        let (mut amount, mut basis) = match floor {
            Some((floor, age)) if age <= self.max_age => {
                let percentile = match urgency {
                    Urgency::Launch => self.launch_percentile,
                    Urgency::Exit => self.exit_percentile,
                };
                let mut amount = floor.at(percentile);
                let mut basis = TipBasis::Market;
                if floor.ema_p50 > 0 {
                    let ceiling = mul_div(floor.ema_p50, u128::from(self.spike_multiple_bps), BPS)
                        .max(self.fallback_lamports);
                    if amount > ceiling {
                        amount = ceiling;
                        basis = TipBasis::SpikeDamped;
                    }
                }
                (amount, basis)
            }
            _ => (self.fallback_lamports, TipBasis::Fallback),
        };
        amount = amount.max(MIN_JITO_TIP_LAMPORTS);
        if notional_lamports > 0 {
            let share = mul_div(notional_lamports, u128::from(self.max_share_bps), BPS)
                .max(MIN_JITO_TIP_LAMPORTS);
            if amount > share {
                amount = share;
                basis = TipBasis::NotionalCapped;
            }
        }
        if amount > self.cap_lamports {
            amount = self.cap_lamports;
            basis = TipBasis::HardCapped;
        }
        TipQuote {
            lamports: amount,
            basis,
        }
    }
}

/// The latest [`TipFloor`], when it was seen, and the policy that reads it.
#[derive(Debug)]
pub struct TipBook {
    policy: TipPolicy,
    latest: Mutex<Option<(TipFloor, Instant)>>,
}

impl TipBook {
    /// An empty book: quotes are the fallback until a floor is published.
    #[must_use]
    pub fn new(policy: TipPolicy) -> Self {
        Self {
            policy,
            latest: Mutex::new(None),
        }
    }

    /// The policy.
    #[must_use]
    pub fn policy(&self) -> TipPolicy {
        self.policy
    }

    /// Publishes a floor observed now.
    ///
    /// # Errors
    /// [`LiveError::StatePoisoned`].
    pub fn update(&self, floor: TipFloor) -> Result<(), LiveError> {
        self.update_at(floor, Instant::now())
    }

    /// Publishes a floor observed at `observed_at`.
    ///
    /// # Errors
    /// [`LiveError::StatePoisoned`].
    pub fn update_at(&self, floor: TipFloor, observed_at: Instant) -> Result<(), LiveError> {
        *self.latest.lock().map_err(|_| LiveError::StatePoisoned)? = Some((floor, observed_at));
        Ok(())
    }

    /// Quotes a tip against the latest floor.
    ///
    /// # Errors
    /// [`LiveError::StatePoisoned`].
    pub fn quote(&self, urgency: Urgency, notional_lamports: u64) -> Result<TipQuote, LiveError> {
        let latest = *self.latest.lock().map_err(|_| LiveError::StatePoisoned)?;
        let age_of = |observed_at: Instant| Instant::now().saturating_duration_since(observed_at);
        Ok(match latest {
            Some((floor, observed_at)) => self.policy.quote(
                Some((&floor, age_of(observed_at))),
                urgency,
                notional_lamports,
            ),
            None => self.policy.quote(None, urgency, notional_lamports),
        })
    }
}
