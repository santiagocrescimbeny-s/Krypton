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
//! The public Block Engine allows about one request per second: keep
//! `poll_interval` at one second or more when using it.

use std::sync::Arc;
use std::time::Duration;

use solana_sdk::hash::Hash;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::debug;

use super::exit::{SellOrder, SellOutcome, SellSubmitter};
use crate::builder::sell::SellBuilder;
use crate::errors::{ExitError, JitoError};
use crate::network::{InflightStatus, JitoClient, JitoTip};
use crate::security::SecureWallet;

/// Default pause between bundle status checks.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Default number of status checks before an order is declared failed.
pub const DEFAULT_MAX_POLLS: u32 = 8;

/// Submits sell orders through the Jito Block Engine.
pub struct JitoSellSubmitter {
    client: Arc<JitoClient>,
    builder: SellBuilder,
    wallet: Arc<SecureWallet>,
    blockhash: watch::Receiver<Hash>,
    tip: JitoTip,
    poll_interval: Duration,
    max_polls: u32,
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
    /// ([`JitoTip`] guarantees it); `blockhash` is kept fresh by the caller.
    #[must_use]
    pub fn new(
        client: Arc<JitoClient>,
        builder: SellBuilder,
        wallet: Arc<SecureWallet>,
        blockhash: watch::Receiver<Hash>,
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
        }
    }

    /// Overrides the status polling cadence.
    #[must_use]
    pub fn with_polling(mut self, poll_interval: Duration, max_polls: u32) -> Self {
        self.poll_interval = poll_interval;
        self.max_polls = max_polls;
        self
    }
}

impl SellSubmitter for JitoSellSubmitter {
    async fn submit(&self, order: &SellOrder) -> Result<SellOutcome, ExitError> {
        // Copy the hash out so the watch guard is not held across an await.
        let blockhash = *self.blockhash.borrow();
        let tip_instruction = self.tip.instruction(&self.builder.payer())?;
        let transaction = self.builder.build_signed(
            &order.plan,
            blockhash,
            std::slice::from_ref(&tip_instruction),
            &self.wallet,
        )?;

        let bundle_id = match self
            .client
            .send_bundle(std::slice::from_ref(&transaction))
            .await
        {
            Ok(id) => id,
            Err(JitoError::DryRun) => return Ok(SellOutcome::Simulated),
            Err(error) => return Err(error.into()),
        };
        debug!(sequence = order.sequence, bundle = %bundle_id, "sell bundle submitted");

        for _ in 0..self.max_polls {
            sleep(self.poll_interval).await;
            match self.client.inflight_status(&bundle_id).await {
                Ok(InflightStatus::Landed { slot }) => return Ok(SellOutcome::Landed { slot }),
                Ok(InflightStatus::Failed | InflightStatus::Invalid) => {
                    return Ok(SellOutcome::Failed)
                }
                Ok(InflightStatus::Pending | InflightStatus::NotFound) => {}
                Err(error) if error.is_retryable() => {
                    debug!(sequence = order.sequence, %error, "status check failed; retrying");
                }
                Err(error) => return Err(error.into()),
            }
        }
        // Unknown outcome: treated as failed. Selling again is safe, because a
        // duplicate fails on chain once the tokens are gone.
        Ok(SellOutcome::Failed)
    }
}
