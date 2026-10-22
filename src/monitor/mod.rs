//! Real-time state monitor and exit manager (Phase 5).
//!
//! - [`geyser`]: resilient stream supervisor (reconnection with exponential
//!   backoff, idle watchdog, health reporting) over a pluggable
//!   [`geyser::StreamTransport`];
//! - [`curve`]: zero-copy `BondingCurve` decoding and curve arithmetic;
//! - [`exit`]: the micro-tranche exit engine, its async manager and the order
//!   executor;
//! - [`submit`]: the Jito-backed order submitter.
//!
//! The pieces talk only through bounded [`tokio::sync::mpsc`] channels and a
//! [`tokio::sync::watch`]-based [`ShutdownSignal`]:
//!
//! ```text
//!  transport ──▶ GeyserMonitor ──StreamUpdate──▶ ExitManager ──SellOrder──▶ executor ──▶ Jito
//!                      │                              ▲                         │
//!                      └── HealthStatus (watch)       └───────SellReport────────┘
//! ```
//!
//! No task ever awaits another task while holding a resource the other needs:
//! the manager only awaits inside a `select!` that includes the shutdown signal,
//! and it dispatches orders with `try_send` into a channel sized so that it can
//! never be full (see [`exit::order_channel`]).

pub mod curve;
pub mod exit;
pub mod geyser;
pub mod submit;

use tokio::sync::watch;

/// The sending half of a shutdown channel. Cloning is not needed: call
/// [`ShutdownTrigger::trigger`] once. **Dropping the trigger also signals
/// shutdown**, so no task is orphaned if the owner exits unexpectedly.
#[derive(Debug)]
pub struct ShutdownTrigger(watch::Sender<bool>);

/// The receiving half of a shutdown channel. Cheap to clone; every task owns one.
#[derive(Debug, Clone)]
pub struct ShutdownSignal(watch::Receiver<bool>);

/// Creates a linked [`ShutdownTrigger`] / [`ShutdownSignal`] pair.
#[must_use]
pub fn shutdown_channel() -> (ShutdownTrigger, ShutdownSignal) {
    let (sender, receiver) = watch::channel(false);
    (ShutdownTrigger(sender), ShutdownSignal(receiver))
}

impl ShutdownTrigger {
    /// Requests shutdown. Idempotent; never fails, even if every signal is gone.
    pub fn trigger(&self) {
        self.0.send_replace(true);
    }
}

impl ShutdownSignal {
    /// Whether shutdown has been requested (or the trigger was dropped).
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }

    /// Completes when shutdown is requested or the trigger is dropped.
    ///
    /// Cancel safe: it can be used as a `select!` branch.
    pub async fn wait(&mut self) {
        loop {
            if *self.0.borrow_and_update() {
                return;
            }
            if self.0.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Waits for Ctrl-C (SIGINT) and then triggers `trigger`.
///
/// Spawn it as its own task next to the monitor and the manager:
/// `tokio::spawn(trigger_on_ctrl_c(trigger))`.
///
/// # Errors
/// The I/O error if the signal handler cannot be installed.
pub async fn trigger_on_ctrl_c(trigger: ShutdownTrigger) -> std::io::Result<()> {
    tokio::signal::ctrl_c().await?;
    trigger.trigger();
    Ok(())
}
