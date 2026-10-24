//! Live trading and the mainnet boundary (Phase 7).
//!
//! - [`settings`]: the explicit opt-in flags, capital limits and keypair path;
//! - [`gate`]: the dry-to-live barrier ([`gate::arm`]);
//! - [`guards`]: [`guards::SpendingGuards`], per-transaction cap, daily drawdown
//!   tripwire and the 10 % slippage ceiling;
//! - [`persist`] (audit #35): the spending state on disk (atomic `0600` writes
//!   from a background thread, an exclusive lock, fail-closed reads);
//! - [`tip`]: dynamic Jito tip calculation ([`tip::TipPolicy`], [`tip::TipBook`]);
//! - [`tip_feed`] / `tip_http` (Phase 8): the Jito tip-floor feed that keeps the
//!   [`tip::TipBook`] fresh (pure polling loop, and the HTTP source and parser);
//! - [`live`]: [`live::LiveExecutor`], which sends launches and runs live exits
//!   through the Jito Block Engine behind all of the above.
//!
//! Live trading is off unless `LIVE_TRADING=true`, `DRY_RUN=false`,
//! `LIVE_CONFIRM` carries the confirmation phrase, both capital limits are set
//! and `LIVE_KEYPAIR_PATH` points to the configured wallet's keypair (mode
//! `0600`) and `GUARD_STATE_PATH` points to a valid state file created once
//! with `krypton init-state`. Anything less is a [`crate::errors::LiveError`]
//! at startup.

pub mod gate;
pub mod guards;
pub mod live;
pub mod persist;
pub mod settings;
pub mod tip;
pub mod tip_feed;
pub mod tip_http;
