//! Jito-backed [`SellSubmitter`]: signs each sell with a tip and sends it as a
//! one-transaction bundle.
//!
//! Each order becomes `[SetComputeUnitLimit, SetComputeUnitPrice, sell_v3, tip]`
//! signed by the wallet, using the freshest blockhash published on a
//! [`watch`] channel (the caller keeps it refreshed), submitted with
//! `sendBundle` and then followed with `getInflightBundleStatuses` until it
//! lands, fails or the polling budget runs out. Under `DRY_RUN` the client
//! refuses to submit ([`JitoError::DryRun`]) and the order is reported as
//! [`SellOutcome::Simulated`] after the transaction was fully assembled and
//! signed, so the whole path is rehearsed.
//!
//! # Stale blockhashes (audit finding #29)
//! The channel carries a [`TimedBlockhash`], the hash plus the instant it was
//! observed. A hash older than [`MAX_BLOCKHASH_AGE`] is refused with
//! [`ExitError::StaleBlockhash`] *before* anything is signed or sent: a dead
//! refresher would otherwise produce transactions that expire silently inside the
//! 1-10 s exit window.
//!
//! # Outcomes (audit finding #30)
//! The first status check runs immediately; the polling interval is only slept
//! *between* checks. Once the Block Engine may have accepted the bundle, no
//! error and no exhausted polling budget is reported as a failure: the result is
//! [`SellOutcome::Unknown`], so the exit engine does not re-offer the tokens and
//! pay the tip and priority fee of a duplicate sell. [`SellOutcome::Failed`] is
//! reserved for bundles that the Block Engine reported failed or invalid.
//!
//! # `Invalid` is not final at first (audit finding #37)
//! The Block Engine answers `Invalid` for a bundle id it cannot find, which is
//! also what a bundle that was just accepted looks like while the status API
//! catches up. [`is_definite_failure`] therefore treats `Invalid` as `Pending`
//! for the first [`INVALID_SOFT_RETRIES`] status checks and as a failure only
//! afterwards; `Failed` is final at once. A bundle that never settles inside the
//! polling budget is [`SellOutcome::Unknown`], as before.
//!
//! The public Block Engine allows about one request per second: keep
//! `poll_interval` at one second or more when using it.

use std::sync::Arc;
use std::time::Duration;

use solana_sdk::hash::Hash;
use tokio::sync::watch;
use tokio::time::{sleep, Instant};
use tracing::{debug, warn};

use super::exit::{SellOrder, SellOutcome, SellSubmitter};
use crate::builder::sell::SellBuilder;
use crate::errors::{ExitError, JitoError};
use crate::metrics::PipelineMetrics;
use crate::network::{InflightStatus, JitoClient, JitoTip};
use crate::security::SecureWallet;

/// A blockhash together with the instant the refresher observed it.
pub type TimedBlockhash = (Hash, Instant);

/// Oldest blockhash a sell may be signed with. A blockhash is valid for about
/// 150 slots (roughly 60 s); 40 s leaves the transaction time to land.
pub const MAX_BLOCKHASH_AGE: Duration = Duration::from_secs(40);

/// Stamps `hash` with the current instant. The refresher calls this each time
/// it fetches a new blockhash and publishes the result on the channel.
#[must_use]
pub fn stamp_blockhash(hash: Hash) -> TimedBlockhash {
    (hash, Instant::now())
}

/// Creates the blockhash channel, seeded with `initial` stamped now.
#[must_use]
pub fn blockhash_channel(
    initial: Hash,
) -> (
    watch::Sender<TimedBlockhash>,
    watch::Receiver<TimedBlockhash>,
) {
    watch::channel(stamp_blockhash(initial))
}

/// Status checks (counted from zero) at which `Invalid` is still ignored.
pub const INVALID_SOFT_RETRIES: u32 = 3;

/// Whether `status`, seen at the zero-based status check `attempt`, settles the
/// bundle as definitely not landed.
///
/// `Failed` always does. `Invalid` does only from the check numbered
/// [`INVALID_SOFT_RETRIES`] on: before that the id may simply not be indexed yet.
#[must_use]
pub fn is_definite_failure(status: &InflightStatus, attempt: u32) -> bool {
    match status {
        InflightStatus::Failed => true,
        InflightStatus::Invalid => attempt >= INVALID_SOFT_RETRIES,
        _ => false,
    }
}

/// Default pause between bundle status checks.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Default number of status checks before an order is declared failed.
pub const DEFAULT_MAX_POLLS: u32 = 8;

/// Submits sell orders through the Jito Block Engine.
pub struct JitoSellSubmitter {
    client: Arc<JitoClient>,
    builder: SellBuilder,
    wallet: Arc<SecureWallet>,
    blockhash: watch::Receiver<TimedBlockhash>,
    tip: JitoTip,
    poll_interval: Duration,
    max_polls: u32,
    metrics: Option<Arc<PipelineMetrics>>,
}

impl std::fmt::Debug for JitoSellSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitoSellSubmitter")
            .field("tip", &self.tip)
            .field("poll_interval", &self.poll_interval)
            .field("max_polls", &self.max_polls)
            .finish_non_exhaustive()
    }
}

impl JitoSellSubmitter {
    /// Builds a submitter. `tip` must already respect `MAX_JITO_TIP_LAMPORTS`
    /// ([`JitoTip`] guarantees it); `blockhash` is kept fresh by the caller and
    /// refused once older than [`MAX_BLOCKHASH_AGE`].
    #[must_use]
    pub fn new(
        client: Arc<JitoClient>,
        builder: SellBuilder,
        wallet: Arc<SecureWallet>,
        blockhash: watch::Receiver<TimedBlockhash>,
        tip: JitoTip,
    ) -> Self {
        Self {
            client,
            builder,
            wallet,
            blockhash,
            tip,
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_polls: DEFAULT_MAX_POLLS,
            metrics: None,
        }
    }

    /// Reports the instant each transaction is assembled and signed to
    /// `metrics` (see [`crate::metrics`]).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<PipelineMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Overrides the status polling cadence.
    #[must_use]
    pub fn with_polling(mut self, poll_interval: Duration, max_polls: u32) -> Self {
        self.poll_interval = poll_interval;
        self.max_polls = max_polls;
        self
    }
}

impl JitoSellSubmitter {
    /// The current blockhash, or [`ExitError::StaleBlockhash`] when it is older
    /// than [`MAX_BLOCKHASH_AGE`]. The tuple is copied out so the watch guard is
    /// not held across an await.
    fn fresh_blockhash(&self) -> Result<Hash, ExitError> {
        let (hash, observed_at) = *self.blockhash.borrow();
        if observed_at.elapsed() > MAX_BLOCKHASH_AGE {
            return Err(ExitError::StaleBlockhash);
        }
        Ok(hash)
    }
}

impl SellSubmitter for JitoSellSubmitter {
    async fn submit(&self, order: &SellOrder) -> Result<SellOutcome, ExitError> {
        // Refuse before signing: nothing is sent with a hash about to expire.
        let blockhash = self.fresh_blockhash()?;
        let tip_instruction = self.tip.instruction(&self.builder.payer())?;
        let transaction = self.builder.build_signed(
            &order.plan,
            blockhash,
            std::slice::from_ref(&tip_instruction),
            &self.wallet,
        )?;
        if let Some(metrics) = &self.metrics {
            metrics.mark_assembled(order.sequence);
        }

        let bundle_id = match self
            .client
            .send_bundle(std::slice::from_ref(&transaction))
            .await
        {
            Ok(id) => id,
            Err(JitoError::DryRun) => return Ok(SellOutcome::Simulated),
            // The request may have reached the Block Engine: there is no bundle
            // id to poll, and a repeat could double-sell.
            Err(error) if error.may_have_been_accepted() => {
                warn!(sequence = order.sequence, %error, "sendBundle outcome ambiguous");
                return Ok(SellOutcome::Unknown);
            }
            Err(error) => return Err(error.into()),
        };
        // The bundle id is derived from the transaction signatures: never logged.
        debug!(sequence = order.sequence, "sell bundle submitted");

        for attempt in 0..self.max_polls {
            // The first check is immediate; the interval separates later ones.
            if attempt > 0 {
                sleep(self.poll_interval).await;
            }
            match self.client.inflight_status(&bundle_id).await {
                Ok(InflightStatus::Landed { slot }) => return Ok(SellOutcome::Landed { slot }),
                Ok(status) if is_definite_failure(&status, attempt) => {
                    return Ok(SellOutcome::Failed)
                }
                // Pending, not found, or an `Invalid` still within its soft retries.
                Ok(_) => {}
                Err(error) if error.is_retryable() => {
                    debug!(sequence = order.sequence, %error, "status check failed; retrying");
                }
                Err(error) => {
                    // The bundle was accepted, so this is not a failed sell.
                    warn!(sequence = order.sequence, %error, "status check failed permanently");
                    return Ok(SellOutcome::Unknown);
                }
            }
        }
        // The bundle may still land: selling again could double-sell.
        Ok(SellOutcome::Unknown)
    }
}
