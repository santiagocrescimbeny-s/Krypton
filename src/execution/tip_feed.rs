//! The Jito tip-floor feed (Phase 8): fills the [`TipBook`] so the dynamic tip
//! actually follows the market (audit finding #38).
//!
//! Without a feed every quote is [`TipBasis::Fallback`](super::tip::TipBasis):
//! the p95/p75 logic of [`TipPolicy`](super::tip::TipPolicy) never runs. This
//! module is the missing producer, split in two so that almost all of it is
//! compiled and tested without a network:
//!
//! - **here** (pure): the [`TipSource`] trait, the unit conversion and sanity
//!   checks ([`floor_from_sol`]) and the polling loop ([`run_tip_feed`]);
//! - [`super::tip_http`]: the HTTP source and the JSON parser.
//!
//! # Failure semantics
//! The feed is an optimisation, never a dependency. A failed or malformed fetch
//! is logged by label, backs off exponentially up to a ceiling, and leaves the
//! last good floor in place; once that floor is older than the policy's maximum
//! age the quote falls back to the configured `JITO_TIP_LAMPORTS`. The loop only
//! stops on shutdown, or if the tip book itself is unusable (poisoned lock).
//!
//! # Rate limit
//! The public Block Engine allows roughly one request per second per IP, and the
//! sell-status polling shares it: the default interval is 10 s and the minimum
//! 1 s.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use super::tip::{TipBook, TipFloor};
use crate::errors::TipFeedError;
use crate::monitor::ShutdownSignal;

/// Lamports in one SOL, as the float the feed's numbers are scaled by.
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Largest value accepted from the feed, in lamports (1 SOL). A landed tip that
/// size is a data error, not a market: the whole sample is rejected rather than
/// clamped.
pub const MAX_PLAUSIBLE_TIP_LAMPORTS: u64 = 1_000_000_000;

/// Default time between fetches.
pub const DEFAULT_FEED_INTERVAL: Duration = Duration::from_secs(10);

/// Default deadline of one fetch.
pub const DEFAULT_FEED_TIMEOUT: Duration = Duration::from_secs(3);

/// Default ceiling of the failure backoff.
pub const DEFAULT_FEED_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Shortest polling interval allowed (the public engine's rate limit).
pub const MIN_FEED_INTERVAL: Duration = Duration::from_secs(1);

/// The tip floor as the feed publishes it: amounts in SOL.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SolTipFloor {
    /// Median landed tip.
    pub p50: f64,
    /// 75th percentile.
    pub p75: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Exponential moving average of the median.
    pub ema_p50: f64,
}

/// Converts an amount in SOL to lamports, rounding to the nearest lamport.
///
/// # Errors
/// [`TipFeedError::Malformed`] for a NaN, an infinity, a negative amount or one
/// above [`MAX_PLAUSIBLE_TIP_LAMPORTS`].
pub fn sol_to_lamports(sol: f64) -> Result<u64, TipFeedError> {
    if !sol.is_finite() || sol < 0.0 {
        return Err(TipFeedError::Malformed {
            reason: "a tip is not a finite non-negative number",
        });
    }
    let lamports = (sol * LAMPORTS_PER_SOL).round();
    // 1e9 is exactly representable, so the comparison and the cast are exact.
    if lamports > MAX_PLAUSIBLE_TIP_LAMPORTS as f64 {
        return Err(TipFeedError::Malformed {
            reason: "a tip is implausibly large",
        });
    }
    Ok(lamports as u64)
}

/// Converts and sanity-checks a published floor.
///
/// # Errors
/// [`TipFeedError::Malformed`] when an amount is invalid (see [`sol_to_lamports`])
/// or the percentiles are not ordered `p50 <= p75 <= p95 <= p99`.
pub fn floor_from_sol(floor: &SolTipFloor) -> Result<TipFloor, TipFeedError> {
    let converted = TipFloor {
        p50: sol_to_lamports(floor.p50)?,
        p75: sol_to_lamports(floor.p75)?,
        p95: sol_to_lamports(floor.p95)?,
        p99: sol_to_lamports(floor.p99)?,
        ema_p50: sol_to_lamports(floor.ema_p50)?,
    };
    if converted.p50 > converted.p75
        || converted.p75 > converted.p95
        || converted.p95 > converted.p99
    {
        return Err(TipFeedError::Malformed {
            reason: "percentiles are not ordered",
        });
    }
    Ok(converted)
}

/// Where tip floors come from.
pub trait TipSource: Send {
    /// Fetches the current floor. The loop applies the deadline, so an
    /// implementation may simply await; it must be cancel safe (it is dropped on
    /// shutdown).
    fn fetch(&mut self) -> impl Future<Output = Result<TipFloor, TipFeedError>> + Send;
}

/// Polling parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TipFeedSettings {
    /// Time between fetches after a success.
    pub interval: Duration,
    /// Deadline of one fetch.
    pub request_timeout: Duration,
    /// Ceiling of the delay after consecutive failures.
    pub max_backoff: Duration,
}

impl Default for TipFeedSettings {
    fn default() -> Self {
        Self {
            interval: DEFAULT_FEED_INTERVAL,
            request_timeout: DEFAULT_FEED_TIMEOUT,
            max_backoff: DEFAULT_FEED_MAX_BACKOFF,
        }
    }
}

impl TipFeedSettings {
    /// Checks the settings are consistent.
    ///
    /// # Errors
    /// [`TipFeedError::InvalidConfig`] for an interval under [`MIN_FEED_INTERVAL`],
    /// a zero deadline or a backoff ceiling below the interval.
    pub fn validate(&self) -> Result<(), TipFeedError> {
        if self.interval < MIN_FEED_INTERVAL {
            return Err(TipFeedError::InvalidConfig {
                reason: "interval must be at least one second",
            });
        }
        if self.request_timeout.is_zero() {
            return Err(TipFeedError::InvalidConfig {
                reason: "request timeout must be positive",
            });
        }
        if self.max_backoff < self.interval {
            return Err(TipFeedError::InvalidConfig {
                reason: "backoff ceiling must not be below the interval",
            });
        }
        Ok(())
    }

    /// The delay before the next fetch after `consecutive_failures` failures in
    /// a row (`0` = the last fetch succeeded): `interval` doubled per failure,
    /// capped at `max_backoff`.
    #[must_use]
    pub fn delay_after(&self, consecutive_failures: u32) -> Duration {
        // 2^10 is already far beyond any sensible ceiling; the cap keeps the shift small.
        let factor = 1_u32 << consecutive_failures.min(10);
        self.interval.saturating_mul(factor).min(self.max_backoff)
    }
}

/// What the loop did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TipFeedStats {
    /// Floors published to the tip book.
    pub updates: u64,
    /// Failed fetches (including timeouts and malformed documents).
    pub failures: u64,
}

/// Polls `source` until `shutdown`, publishing every good floor to `book`.
///
/// The first fetch happens immediately. Returns the counters on a clean stop.
///
/// # Errors
/// [`TipFeedError::InvalidConfig`] for bad `settings` (nothing is fetched);
/// [`TipFeedError::BookUnavailable`] if the tip book cannot be updated.
pub async fn run_tip_feed<S: TipSource>(
    mut source: S,
    book: Arc<TipBook>,
    settings: TipFeedSettings,
    mut shutdown: ShutdownSignal,
) -> Result<TipFeedStats, TipFeedError> {
    settings.validate()?;
    let mut stats = TipFeedStats::default();
    let mut consecutive_failures: u32 = 0;
    loop {
        if shutdown.is_triggered() {
            break;
        }
        let fetched = tokio::select! {
            biased;
            () = shutdown.wait() => break,
            fetched = timeout(settings.request_timeout, source.fetch()) => fetched,
        };
        let outcome = fetched.unwrap_or(Err(TipFeedError::Timeout));
        match outcome {
            Ok(floor) => {
                book.update(floor)
                    .map_err(|_| TipFeedError::BookUnavailable)?;
                stats.updates = stats.updates.saturating_add(1);
                consecutive_failures = 0;
                debug!(updates = stats.updates, "tip floor updated");
            }
            Err(error) => {
                stats.failures = stats.failures.saturating_add(1);
                consecutive_failures = consecutive_failures.saturating_add(1);
                warn!(
                    error = error.label(),
                    consecutive_failures, "tip floor fetch failed; keeping the last floor"
                );
            }
        }
        let delay = settings.delay_after(consecutive_failures);
        tokio::select! {
            biased;
            () = shutdown.wait() => break,
            () = sleep(delay) => {}
        }
    }
    Ok(stats)
}
