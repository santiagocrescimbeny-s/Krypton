//! Live trading and the mainnet boundary (Phase 7).
//!
//! - [`settings`]: the explicit opt-in flags, capital limits and keypair path;
//! - [`gate`]: the dry-to-live barrier ([`gate::arm`]);
//! - [`guards`]: [`guards::SpendingGuards`], per-transaction cap, daily drawdown
//!   tripwire and the 10 % slippage ceiling;
//! - [`tip`]: dynamic Jito tip calculation ([`tip::TipPolicy`], [`tip::TipBook`]);
//! - [`live`]: [`live::LiveExecutor`], which sends launches and runs live exits
//!   through the Jito Block Engine behind all of the above.
//!
//! Live trading is off unless `LIVE_TRADING=true`, `DRY_RUN=false`,
//! `LIVE_CONFIRM` carries the confirmation phrase, both capital limits are set
//! and `LIVE_KEYPAIR_PATH` points to the configured wallet's keypair (mode
//! `0600`). Anything less is a [`crate::errors::LiveError`] at startup.

pub mod gate;
pub mod guards;
pub mod live;
pub mod settings;
pub mod tip;
