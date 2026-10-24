//! Live-trading settings: the explicit opt-in flags, the capital limits and the
//! keypair file, parsed from the environment (Phase 7).
//!
//! Parsing is pure and never reads the filesystem: files are only touched when
//! the barrier is crossed (see [`super::gate`]). Every value is checked and
//! nothing is clamped silently. Errors never echo the offending value.

use std::path::PathBuf;

use crate::errors::ConfigError;
use crate::parsing::{parse_bool, parse_sol_to_lamports};

/// Explicit live opt-in (`true` / `false`). Absent means `false`.
pub const ENV_LIVE_TRADING: &str = "LIVE_TRADING";
/// Must equal [`LIVE_CONFIRM_PHRASE`] for live trading to arm.
pub const ENV_LIVE_CONFIRM: &str = "LIVE_CONFIRM";
/// Path of the keypair file live trading signs with.
pub const ENV_LIVE_KEYPAIR_PATH: &str = "LIVE_KEYPAIR_PATH";
/// Path of the persisted spending state (see [`super::persist`]). Required to
/// arm live trading.
pub const ENV_GUARD_STATE_PATH: &str = "GUARD_STATE_PATH";
/// Per-transaction spending cap, in SOL.
pub const ENV_MAX_SOL_PER_TX: &str = "MAX_SOL_PER_TX";
/// Daily net-spend (drawdown) limit, in SOL.
pub const ENV_DAILY_DRAWDOWN_SOL: &str = "DAILY_DRAWDOWN_SOL";

/// The exact text `LIVE_CONFIRM` must carry. Deliberately long: it cannot be
/// typed by accident or left over from a template.
pub const LIVE_CONFIRM_PHRASE: &str = "I-UNDERSTAND-REAL-FUNDS-WILL-BE-SPENT";

/// No configuration can raise the per-transaction cap above 10 SOL.
pub const ABSOLUTE_MAX_SPEND_PER_TX_LAMPORTS: u64 = 10_000_000_000;
/// No configuration can raise the daily drawdown limit above 50 SOL.
pub const ABSOLUTE_MAX_DAILY_DRAWDOWN_LAMPORTS: u64 = 50_000_000_000;

/// Unvalidated live variables, straight from the environment.
#[derive(Debug, Default, Clone)]
pub struct RawLiveSettings {
    /// `LIVE_TRADING`.
    pub live_trading: Option<String>,
    /// `LIVE_CONFIRM`.
    pub live_confirm: Option<String>,
    /// `LIVE_KEYPAIR_PATH`.
    pub live_keypair_path: Option<String>,
    /// `GUARD_STATE_PATH`.
    pub guard_state_path: Option<String>,
    /// `MAX_SOL_PER_TX`.
    pub max_sol_per_tx: Option<String>,
    /// `DAILY_DRAWDOWN_SOL`.
    pub daily_drawdown_sol: Option<String>,
}

/// Validated live settings. The default is "not armed, nothing allowed".
#[derive(Clone, PartialEq, Eq, Default)]
pub struct LiveSettings {
    /// `LIVE_TRADING=true` was set.
    pub live_trading: bool,
    /// `LIVE_CONFIRM` carried [`LIVE_CONFIRM_PHRASE`].
    pub confirmed: bool,
    /// The keypair file live trading signs with.
    pub keypair_path: Option<PathBuf>,
    /// The persisted spending state file.
    pub guard_state_path: Option<PathBuf>,
    /// Per-transaction cap, in lamports.
    pub max_spend_per_tx_lamports: Option<u64>,
    /// Daily drawdown limit, in lamports.
    pub daily_drawdown_lamports: Option<u64>,
}

impl std::fmt::Debug for LiveSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The path is not printed: it can reveal the account layout of the host.
        f.debug_struct("LiveSettings")
            .field("live_trading", &self.live_trading)
            .field("confirmed", &self.confirmed)
            .field("keypair_path", &self.keypair_path.is_some())
            .field("guard_state_path", &self.guard_state_path.is_some())
            .field("max_spend_per_tx_lamports", &self.max_spend_per_tx_lamports)
            .field("daily_drawdown_lamports", &self.daily_drawdown_lamports)
            .finish()
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn invalid(name: &'static str, reason: &str) -> ConfigError {
    ConfigError::InvalidVar {
        name,
        reason: reason.to_owned(),
    }
}

fn parse_limit(
    name: &'static str,
    text: Option<String>,
    ceiling: u64,
    ceiling_text: &str,
) -> Result<Option<u64>, ConfigError> {
    let Some(text) = clean(text) else {
        return Ok(None);
    };
    let lamports = parse_sol_to_lamports(&text).map_err(|reason| invalid(name, reason))?;
    if lamports == 0 {
        return Err(invalid(name, "must be greater than zero"));
    }
    if lamports > ceiling {
        return Err(invalid(
            name,
            &format!("must be at most {ceiling_text} SOL"),
        ));
    }
    Ok(Some(lamports))
}

impl LiveSettings {
    /// Validates the raw variables.
    ///
    /// Everything is optional here: a configuration without live variables is
    /// valid and simply cannot arm. What is present must be well formed.
    ///
    /// # Errors
    /// [`ConfigError::InvalidVar`] for a malformed or out-of-range value, or for
    /// a `LIVE_CONFIRM` that is not [`LIVE_CONFIRM_PHRASE`].
    pub fn from_raw(raw: RawLiveSettings) -> Result<Self, ConfigError> {
        let live_trading = match clean(raw.live_trading) {
            Some(text) => parse_bool(&text).ok_or_else(|| {
                invalid(
                    ENV_LIVE_TRADING,
                    "expected true/false, 1/0, yes/no or on/off",
                )
            })?,
            None => false,
        };
        let confirmed = match clean(raw.live_confirm) {
            Some(text) if text == LIVE_CONFIRM_PHRASE => true,
            Some(_) => {
                return Err(invalid(
                    ENV_LIVE_CONFIRM,
                    "does not match the required confirmation phrase",
                ))
            }
            None => false,
        };
        Ok(Self {
            live_trading,
            confirmed,
            keypair_path: clean(raw.live_keypair_path).map(PathBuf::from),
            guard_state_path: clean(raw.guard_state_path).map(PathBuf::from),
            max_spend_per_tx_lamports: parse_limit(
                ENV_MAX_SOL_PER_TX,
                raw.max_sol_per_tx,
                ABSOLUTE_MAX_SPEND_PER_TX_LAMPORTS,
                "10",
            )?,
            daily_drawdown_lamports: parse_limit(
                ENV_DAILY_DRAWDOWN_SOL,
                raw.daily_drawdown_sol,
                ABSOLUTE_MAX_DAILY_DRAWDOWN_LAMPORTS,
                "50",
            )?,
        })
    }
}
