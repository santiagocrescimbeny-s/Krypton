//! Strongly-typed configuration loaded from environment variables.
//!
//! Flow: `.env` (optional, via `dotenvy`) -> process environment -> a
//! `serde`-deserialized [`RawConfig`] of strings -> strict validation into
//! [`Config`]. Every value is range/format checked; nothing is silently
//! clamped, and no error message ever contains a secret or a URL.
//!
//! # Secrets
//! The wallet key is parsed once, during validation, into a zeroizing
//! [`SecureWallet`]; the original string ([`SecretString`]) is wiped as soon as
//! parsing finishes, so [`Config`] never retains the textual key. In addition,
//! [`Config::from_env`] removes `PRIVATE_KEY` and `YELLOWSTONE_X_TOKEN` from the
//! process environment right after reading them.
//!
//! # `.env` discovery
//! Only an explicit path is ever read ([`DEFAULT_DOTENV_PATH`] for
//! [`Config::from_env`], or the path given to [`Config::from_env_file`]). Parent
//! directories are never searched, so a stray `.env` in an untrusted ancestor
//! directory cannot inject configuration.
//!
//! Parsing is separated from the process environment: [`Config::from_vars`]
//! takes an explicit map, which keeps it deterministic and testable without
//! mutating global state. [`Config::from_env`] is the thin production wrapper.

use std::collections::HashMap;
use std::env::VarError;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use reqwest::Url;
use serde::Deserialize;
use solana_commitment_config::CommitmentConfig;

use crate::errors::{ConfigError, WalletError};
use crate::execution::settings::{
    LiveSettings, RawLiveSettings, ENV_DAILY_DRAWDOWN_SOL, ENV_GUARD_STATE_PATH, ENV_LIVE_CONFIRM,
    ENV_LIVE_KEYPAIR_PATH, ENV_LIVE_TRADING, ENV_MAX_SOL_PER_TX,
};
use crate::parsing::{endpoint_scheme_allowed, parse_bool, parse_sol_to_lamports};
use crate::production::{
    merge_alias, ProductionSettings, RawProductionSettings, ENV_JITO_BLOCK_ENGINE_URL,
    ENV_JITO_TIP_STREAM_URL, ENV_PAYER_KEYPAIR_PATH, ENV_YELLOWSTONE_ENDPOINT,
    ENV_YELLOWSTONE_X_TOKEN,
};
use crate::security::{SecretString, SecureWallet};

// --- Environment variable names ------------------------------------------------

/// JSON-RPC endpoint (required).
pub const ENV_RPC_URL: &str = "RPC_URL";
/// PubSub websocket endpoint (optional).
pub const ENV_RPC_WS_URL: &str = "RPC_WS_URL";
/// Jito Block Engine base URL (required).
pub const ENV_JITO_ENGINE_URL: &str = "JITO_ENGINE_URL";
/// Wallet secret: Base58 keypair or JSON byte array (required).
pub const ENV_PRIVATE_KEY: &str = "PRIVATE_KEY";
/// Confirmation commitment level.
pub const ENV_COMMITMENT: &str = "COMMITMENT";
/// Pre-flight simulation commitment level.
pub const ENV_PREFLIGHT_COMMITMENT: &str = "PREFLIGHT_COMMITMENT";
/// SOL spent by the creator on the initial buy (required).
pub const ENV_CREATOR_SOL_AMOUNT: &str = "CREATOR_SOL_AMOUNT";
/// Slippage tolerance in basis points.
pub const ENV_SLIPPAGE_BPS: &str = "SLIPPAGE_BPS";
/// Compute unit limit for the launch transaction.
pub const ENV_COMPUTE_UNIT_LIMIT: &str = "COMPUTE_UNIT_LIMIT";
/// Priority fee in micro-lamports per compute unit.
pub const ENV_PRIORITY_FEE_MICRO_LAMPORTS: &str = "PRIORITY_FEE_MICRO_LAMPORTS";
/// Hard ceiling on the priority fee actually paid, in lamports (optional).
pub const ENV_MAX_PRIORITY_FEE_LAMPORTS: &str = "MAX_PRIORITY_FEE_LAMPORTS";
/// Jito tip in lamports.
pub const ENV_JITO_TIP_LAMPORTS: &str = "JITO_TIP_LAMPORTS";
/// Hard ceiling on the Jito tip, in lamports (optional).
pub const ENV_MAX_JITO_TIP_LAMPORTS: &str = "MAX_JITO_TIP_LAMPORTS";
/// Dry-run switch.
pub const ENV_DRY_RUN: &str = "DRY_RUN";
/// Directory for log files.
pub const ENV_LOG_DIR: &str = "LOG_DIR";
/// Minimum log level.
pub const ENV_LOG_LEVEL: &str = "LOG_LEVEL";

/// The core variables of the configuration (Phases 1-7). Together with
/// [`PRODUCTION_ENV_KEYS`] they are every variable the configuration reads, and
/// must stay in sync with [`RawConfig`] (its fields are renamed to these exact
/// names). The two lists are kept apart so that the production bridge adds
/// variables without changing this one.
pub const ENV_KEYS: [&str; 21] = [
    ENV_RPC_URL,
    ENV_RPC_WS_URL,
    ENV_JITO_ENGINE_URL,
    ENV_PRIVATE_KEY,
    ENV_COMMITMENT,
    ENV_PREFLIGHT_COMMITMENT,
    ENV_CREATOR_SOL_AMOUNT,
    ENV_SLIPPAGE_BPS,
    ENV_COMPUTE_UNIT_LIMIT,
    ENV_PRIORITY_FEE_MICRO_LAMPORTS,
    ENV_MAX_PRIORITY_FEE_LAMPORTS,
    ENV_JITO_TIP_LAMPORTS,
    ENV_MAX_JITO_TIP_LAMPORTS,
    ENV_DRY_RUN,
    ENV_LOG_DIR,
    ENV_LOG_LEVEL,
    ENV_LIVE_TRADING,
    ENV_LIVE_CONFIRM,
    ENV_LIVE_KEYPAIR_PATH,
    ENV_MAX_SOL_PER_TX,
    ENV_DAILY_DRAWDOWN_SOL,
];

/// The production-bridge variables (see [`crate::production`]): all optional.
/// `PAYER_KEYPAIR_PATH` and `JITO_BLOCK_ENGINE_URL` are aliases of
/// `LIVE_KEYPAIR_PATH` and `JITO_ENGINE_URL`. `GUARD_STATE_PATH` (audit
/// finding #35) locates the persisted spending state.
pub const PRODUCTION_ENV_KEYS: [&str; 6] = [
    ENV_YELLOWSTONE_ENDPOINT,
    ENV_YELLOWSTONE_X_TOKEN,
    ENV_JITO_BLOCK_ENGINE_URL,
    ENV_JITO_TIP_STREAM_URL,
    ENV_PAYER_KEYPAIR_PATH,
    ENV_GUARD_STATE_PATH,
];

// --- Defaults and limits -------------------------------------------------------

/// Default slippage: 5%.
pub const DEFAULT_SLIPPAGE_BPS: u16 = 500;
/// Slippage ceiling: 10 %.
///
/// A launch is one atomic `Create` + `Buy` transaction, so nobody can trade in
/// between: slippage only has to absorb a mismatch between the assumed and the
/// live fee/curve parameters. A wider tolerance would turn `max_sol_cost` into
/// a blank cheque (audit finding #20), hence the hard bound.
pub const MAX_SLIPPAGE_BPS: u16 = 1_000;
/// Default compute unit limit (enough for create + buy with headroom).
pub const DEFAULT_COMPUTE_UNIT_LIMIT: u32 = 300_000;
/// Solana's per-transaction compute unit ceiling.
pub const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
/// Default priority fee in micro-lamports per compute unit.
pub const DEFAULT_PRIORITY_FEE_MICRO_LAMPORTS: u64 = 100_000;
/// Default cap on the priority fee actually paid: 0.005 SOL.
pub const DEFAULT_MAX_PRIORITY_FEE_LAMPORTS: u64 = 5_000_000;
/// Absolute ceiling for the configurable priority-fee cap: 0.05 SOL. No value of
/// `MAX_PRIORITY_FEE_LAMPORTS` can lift the cap above this.
pub const ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS: u64 = 50_000_000;
/// Default Jito tip in lamports.
pub const DEFAULT_JITO_TIP_LAMPORTS: u64 = 100_000;
/// Default cap on the Jito tip: 0.001 SOL.
pub const DEFAULT_MAX_JITO_TIP_LAMPORTS: u64 = 1_000_000;
/// Absolute ceiling for the configurable tip cap: 0.01 SOL. No value of
/// `MAX_JITO_TIP_LAMPORTS` can lift the cap above this.
pub const ABSOLUTE_MAX_JITO_TIP_LAMPORTS: u64 = 10_000_000;
/// Smallest tip the Jito Block Engine accepts for a bundle.
pub const MIN_JITO_TIP_LAMPORTS: u64 = 1_000;
/// Default log directory.
pub const DEFAULT_LOG_DIR: &str = "logs";
/// Default `.env` file: a fixed path relative to the working directory (the
/// repository root when launched from it). Never searched for in parent
/// directories.
pub const DEFAULT_DOTENV_PATH: &str = ".env";
/// Default log level.
pub const DEFAULT_LOG_LEVEL: tracing::Level = tracing::Level::INFO;

// --- Raw (string) schema -------------------------------------------------------

/// Unvalidated environment snapshot. Field names map 1:1 to the `ENV_*` keys.
#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
struct RawConfig {
    rpc_url: Option<String>,
    rpc_ws_url: Option<String>,
    jito_engine_url: Option<String>,
    private_key: Option<SecretString>,
    commitment: Option<String>,
    preflight_commitment: Option<String>,
    creator_sol_amount: Option<String>,
    slippage_bps: Option<String>,
    compute_unit_limit: Option<String>,
    priority_fee_micro_lamports: Option<String>,
    max_priority_fee_lamports: Option<String>,
    jito_tip_lamports: Option<String>,
    max_jito_tip_lamports: Option<String>,
    dry_run: Option<String>,
    log_dir: Option<String>,
    log_level: Option<String>,
    live_trading: Option<String>,
    live_confirm: Option<String>,
    live_keypair_path: Option<String>,
    max_sol_per_tx: Option<String>,
    daily_drawdown_sol: Option<String>,
    yellowstone_endpoint: Option<String>,
    yellowstone_x_token: Option<SecretString>,
    jito_block_engine_url: Option<String>,
    jito_tip_stream_url: Option<String>,
    payer_keypair_path: Option<String>,
    guard_state_path: Option<String>,
}

// --- Validated configuration ---------------------------------------------------

/// Validated runtime configuration.
///
/// Intentionally not `Clone` (it owns the parsed wallet); share it behind an
/// `Arc`. `Debug` redacts the secret and strips paths/queries from URLs.
pub struct Config {
    /// Solana JSON-RPC endpoint (`http`/`https`).
    pub rpc_url: Url,
    /// Solana PubSub endpoint (`ws`/`wss`), if configured.
    pub rpc_ws_url: Option<Url>,
    /// Jito Block Engine base URL (`http`/`https`).
    pub jito_engine_url: Url,
    /// Commitment used to confirm transactions and read state.
    pub commitment: CommitmentConfig,
    /// Commitment used for pre-flight simulation.
    pub preflight_commitment: CommitmentConfig,
    /// Creator's initial buy, in lamports (parsed exactly from `CREATOR_SOL_AMOUNT`).
    pub creator_buy_lamports: u64,
    /// Slippage tolerance in basis points (`0..=`[`MAX_SLIPPAGE_BPS`]).
    pub slippage_bps: u16,
    /// Compute unit limit (1..=1_400_000).
    pub compute_unit_limit: u32,
    /// Priority fee, micro-lamports per compute unit.
    pub priority_fee_micro_lamports: u64,
    /// Hard cap on the priority fee actually paid (`price x CU limit`), in
    /// lamports. Validated against [`ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS`].
    pub max_priority_fee_lamports: u64,
    /// Jito tip in lamports (`MIN_JITO_TIP_LAMPORTS..=max_jito_tip_lamports`).
    pub jito_tip_lamports: u64,
    /// Hard cap on the Jito tip, in lamports. Validated against
    /// [`ABSOLUTE_MAX_JITO_TIP_LAMPORTS`].
    pub max_jito_tip_lamports: u64,
    /// When `true` nothing is ever submitted to the network.
    pub dry_run: bool,
    /// Directory where log files are written.
    pub log_dir: PathBuf,
    /// Minimum level to log.
    pub log_level: tracing::Level,
    /// Live-trading opt-in and capital limits (Phase 7). Inert unless
    /// [`crate::execution::live::LiveExecutor::arm`] accepts it.
    pub live: LiveSettings,
    /// Yellowstone endpoint and tip-feed URL (production bridge). All optional.
    pub production: ProductionSettings,
    /// The parsed wallet. The textual key is not kept (see the module docs).
    wallet: SecureWallet,
}

impl Config {
    /// Loads [`DEFAULT_DOTENV_PATH`] (if present) and the process environment,
    /// then validates.
    ///
    /// Equivalent to [`Config::from_env_file`] with the default path. Call it
    /// once at startup, **before spawning threads** (see the note there).
    ///
    /// # Errors
    /// Any [`ConfigError`].
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_file(Path::new(DEFAULT_DOTENV_PATH))
    }

    /// Loads the `.env` file at exactly `dotenv_path` (if it exists) and the
    /// process environment, then validates.
    ///
    /// - A missing file is fine; a malformed one is an error (reported without
    ///   echoing its contents).
    /// - No directory search is performed: only `dotenv_path` is read. Values
    ///   already present in the process environment win over the file.
    /// - `PRIVATE_KEY` is removed from the process environment as soon as it has
    ///   been read, whether or not validation succeeds, so child processes and
    ///   later `std::env::vars()` calls cannot see it.
    ///
    /// # Limits
    /// `std::env::remove_var` is only sound while no other thread reads or
    /// writes the environment, so call this during single-threaded startup.
    /// It cannot wipe the OS-level copy of the initial environment block (for
    /// example `/proc/<pid>/environ` on Linux), nor the `String` returned by
    /// `std::env::var`, which is moved into the config and zeroized there.
    /// Prefer a secret store or a restricted-permission `.env` over exporting
    /// the key in the shell.
    ///
    /// # Errors
    /// Any [`ConfigError`].
    pub fn from_env_file(dotenv_path: &Path) -> Result<Self, ConfigError> {
        // `dotenvy` applies a file line by line, so a syntax error half-way
        // through can leave `PRIVATE_KEY` already exported. Scrub regardless of
        // the outcome, before any validation can fail or log.
        let snapshot = load_dotenv(dotenv_path).and_then(|()| collect_env_vars());
        scrub_secret_env();
        Self::from_vars(snapshot?)
    }

    /// Builds a validated configuration from an explicit variable map.
    ///
    /// Unknown keys are ignored. Blank values count as "not set". The map is
    /// consumed so secret values are *moved* (not copied) into the config.
    ///
    /// # Errors
    /// [`ConfigError::MissingVar`] for absent required keys,
    /// [`ConfigError::InvalidVar`] for malformed or out-of-range values.
    pub fn from_vars(mut vars: HashMap<String, String>) -> Result<Self, ConfigError> {
        let mut object = serde_json::Map::with_capacity(ENV_KEYS.len() + PRODUCTION_ENV_KEYS.len());
        for key in ENV_KEYS.into_iter().chain(PRODUCTION_ENV_KEYS) {
            if let Some(value) = vars.remove(key) {
                object.insert(key.to_owned(), serde_json::Value::String(value));
            }
        }
        let raw: RawConfig = serde_json::from_value(serde_json::Value::Object(object))
            .map_err(|err| ConfigError::Deserialize(err.to_string()))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, ConfigError> {
        // Required values first, in a fixed order, so errors are deterministic.
        let rpc_url = require(ENV_RPC_URL, clean(raw.rpc_url))?;
        let jito_engine_url = require(
            ENV_JITO_ENGINE_URL,
            merge_alias(
                ENV_JITO_ENGINE_URL,
                ENV_JITO_BLOCK_ENGINE_URL,
                raw.jito_engine_url,
                raw.jito_block_engine_url,
            )?,
        )?;
        let private_key =
            raw.private_key
                .filter(|secret| !secret.is_blank())
                .ok_or(ConfigError::MissingVar {
                    name: ENV_PRIVATE_KEY,
                })?;
        let creator_sol = require(ENV_CREATOR_SOL_AMOUNT, clean(raw.creator_sol_amount))?;

        // Fail fast on a malformed key instead of at first signature. The wallet
        // built here is kept (it zeroizes itself); `WalletError` messages never
        // contain key material, so forwarding them is safe.
        let wallet = SecureWallet::from_secret_str(private_key.expose_secret())
            .map_err(|err| invalid(ENV_PRIVATE_KEY, &err.to_string()))?;
        // The textual key is no longer needed: wipe it now instead of keeping it
        // for the lifetime of the config (the parsed `wallet` zeroizes itself).
        drop(private_key);

        let creator_buy_lamports = parse_sol_to_lamports(&creator_sol)
            .map_err(|reason| invalid(ENV_CREATOR_SOL_AMOUNT, reason))?;
        if creator_buy_lamports == 0 {
            return Err(invalid(ENV_CREATOR_SOL_AMOUNT, "must be greater than zero"));
        }

        let slippage_bps = match clean(raw.slippage_bps) {
            Some(text) => parse_number::<u16>(ENV_SLIPPAGE_BPS, &text)?,
            None => DEFAULT_SLIPPAGE_BPS,
        };
        if slippage_bps > MAX_SLIPPAGE_BPS {
            return Err(invalid(
                ENV_SLIPPAGE_BPS,
                "must be at most 1000 basis points (10%)",
            ));
        }

        let compute_unit_limit = match clean(raw.compute_unit_limit) {
            Some(text) => parse_number::<u32>(ENV_COMPUTE_UNIT_LIMIT, &text)?,
            None => DEFAULT_COMPUTE_UNIT_LIMIT,
        };
        if compute_unit_limit == 0 || compute_unit_limit > MAX_COMPUTE_UNIT_LIMIT {
            return Err(invalid(
                ENV_COMPUTE_UNIT_LIMIT,
                "must be between 1 and 1400000",
            ));
        }

        let priority_fee_micro_lamports = match clean(raw.priority_fee_micro_lamports) {
            Some(text) => parse_number::<u64>(ENV_PRIORITY_FEE_MICRO_LAMPORTS, &text)?,
            None => DEFAULT_PRIORITY_FEE_MICRO_LAMPORTS,
        };

        let max_priority_fee_lamports = match clean(raw.max_priority_fee_lamports) {
            Some(text) => parse_number::<u64>(ENV_MAX_PRIORITY_FEE_LAMPORTS, &text)?,
            None => DEFAULT_MAX_PRIORITY_FEE_LAMPORTS,
        };
        if max_priority_fee_lamports > ABSOLUTE_MAX_PRIORITY_FEE_LAMPORTS {
            return Err(invalid(
                ENV_MAX_PRIORITY_FEE_LAMPORTS,
                "must be at most 50000000 lamports (0.05 SOL)",
            ));
        }
        // The fee actually charged is `price x requested CU limit`, not the CU
        // actually consumed, so a typo in the price can cost a fortune (audit
        // finding #19). Reject it before anything is built.
        if priority_fee_lamports(priority_fee_micro_lamports, compute_unit_limit)
            > u128::from(max_priority_fee_lamports)
        {
            return Err(invalid(
                ENV_PRIORITY_FEE_MICRO_LAMPORTS,
                "price x COMPUTE_UNIT_LIMIT exceeds MAX_PRIORITY_FEE_LAMPORTS",
            ));
        }

        let jito_tip_lamports = match clean(raw.jito_tip_lamports) {
            Some(text) => parse_number::<u64>(ENV_JITO_TIP_LAMPORTS, &text)?,
            None => DEFAULT_JITO_TIP_LAMPORTS,
        };
        if jito_tip_lamports < MIN_JITO_TIP_LAMPORTS {
            return Err(invalid(
                ENV_JITO_TIP_LAMPORTS,
                "must be at least 1000 lamports (Jito minimum)",
            ));
        }

        let max_jito_tip_lamports = match clean(raw.max_jito_tip_lamports) {
            Some(text) => parse_number::<u64>(ENV_MAX_JITO_TIP_LAMPORTS, &text)?,
            None => DEFAULT_MAX_JITO_TIP_LAMPORTS,
        };
        if max_jito_tip_lamports > ABSOLUTE_MAX_JITO_TIP_LAMPORTS {
            return Err(invalid(
                ENV_MAX_JITO_TIP_LAMPORTS,
                "must be at most 10000000 lamports (0.01 SOL)",
            ));
        }
        // The tip is spent on top of the priority fee; bounding it keeps the
        // total fee spend of a launch bounded too (audit finding #2).
        if jito_tip_lamports > max_jito_tip_lamports {
            return Err(invalid(
                ENV_JITO_TIP_LAMPORTS,
                "exceeds MAX_JITO_TIP_LAMPORTS",
            ));
        }

        let dry_run = match clean(raw.dry_run) {
            Some(text) => parse_bool(&text).ok_or_else(|| {
                invalid(ENV_DRY_RUN, "expected true/false, 1/0, yes/no or on/off")
            })?,
            // Safe by default: nothing is sent unless explicitly enabled.
            None => true,
        };

        let log_level = match clean(raw.log_level) {
            Some(text) => tracing::Level::from_str(&text).map_err(|_| {
                invalid(
                    ENV_LOG_LEVEL,
                    "expected one of trace, debug, info, warn, error",
                )
            })?,
            None => DEFAULT_LOG_LEVEL,
        };

        let commitment = match clean(raw.commitment) {
            Some(text) => parse_commitment(ENV_COMMITMENT, &text)?,
            None => CommitmentConfig::confirmed(),
        };
        let preflight_commitment = match clean(raw.preflight_commitment) {
            Some(text) => parse_commitment(ENV_PREFLIGHT_COMMITMENT, &text)?,
            None => CommitmentConfig::processed(),
        };

        let live = LiveSettings::from_raw(RawLiveSettings {
            live_trading: raw.live_trading,
            live_confirm: raw.live_confirm,
            live_keypair_path: merge_alias(
                ENV_LIVE_KEYPAIR_PATH,
                ENV_PAYER_KEYPAIR_PATH,
                raw.live_keypair_path,
                raw.payer_keypair_path,
            )?,
            guard_state_path: raw.guard_state_path,
            max_sol_per_tx: raw.max_sol_per_tx,
            daily_drawdown_sol: raw.daily_drawdown_sol,
        })?;
        let production = ProductionSettings::from_raw(RawProductionSettings {
            yellowstone_endpoint: raw.yellowstone_endpoint,
            yellowstone_x_token: raw.yellowstone_x_token,
            jito_tip_stream_url: raw.jito_tip_stream_url,
        })?;

        Ok(Self {
            rpc_url: parse_url_secure(ENV_RPC_URL, &rpc_url)?,
            rpc_ws_url: clean(raw.rpc_ws_url)
                .map(|text| parse_url(ENV_RPC_WS_URL, &text, &["ws", "wss"]))
                .transpose()?,
            jito_engine_url: parse_url_secure(ENV_JITO_ENGINE_URL, &jito_engine_url)?,
            commitment,
            preflight_commitment,
            creator_buy_lamports,
            slippage_bps,
            compute_unit_limit,
            priority_fee_micro_lamports,
            max_priority_fee_lamports,
            jito_tip_lamports,
            max_jito_tip_lamports,
            dry_run,
            log_dir: clean(raw.log_dir)
                .map_or_else(|| PathBuf::from(DEFAULT_LOG_DIR), PathBuf::from),
            log_level,
            live,
            production,
            wallet,
        })
    }

    /// Borrows the configured wallet.
    #[must_use]
    pub fn wallet(&self) -> &SecureWallet {
        &self.wallet
    }

    /// Builds an independent [`SecureWallet`] from the configured key.
    ///
    /// The copy is made from the already-validated raw bytes (the original
    /// string is long gone), and it zeroizes itself when dropped.
    ///
    /// # Errors
    /// Any [`WalletError`]; none of them contains key material.
    pub fn load_wallet(&self) -> Result<SecureWallet, WalletError> {
        SecureWallet::from_keypair_bytes(self.wallet.secret().expose())
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("rpc_url", &redact_url(&self.rpc_url))
            .field("rpc_ws_url", &self.rpc_ws_url.as_ref().map(redact_url))
            .field("jito_engine_url", &redact_url(&self.jito_engine_url))
            .field("commitment", &self.commitment)
            .field("preflight_commitment", &self.preflight_commitment)
            .field("creator_buy_lamports", &self.creator_buy_lamports)
            .field("slippage_bps", &self.slippage_bps)
            .field("compute_unit_limit", &self.compute_unit_limit)
            .field(
                "priority_fee_micro_lamports",
                &self.priority_fee_micro_lamports,
            )
            .field("max_priority_fee_lamports", &self.max_priority_fee_lamports)
            .field("jito_tip_lamports", &self.jito_tip_lamports)
            .field("max_jito_tip_lamports", &self.max_jito_tip_lamports)
            .field("dry_run", &self.dry_run)
            .field("log_dir", &self.log_dir)
            .field("log_level", &self.log_level)
            .field("live", &self.live)
            .field("production", &self.production)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

/// Renders `scheme://host[:port]/...` only. Paths, queries, fragments and
/// credentials are dropped because RPC providers routinely embed API keys there.
#[must_use]
pub fn redact_url(url: &Url) -> String {
    let host = url.host_str().unwrap_or("<no-host>");
    match url.port() {
        Some(port) => format!("{}://{}:{}/...", url.scheme(), host, port),
        None => format!("{}://{}/...", url.scheme(), host),
    }
}

// --- Helpers -------------------------------------------------------------------

/// Priority fee in lamports that the network charges for a transaction:
/// `ceil(price_micro_lamports x compute_unit_limit / 1_000_000)`.
///
/// Computed in `u128`: the product of a `u64` and a `u32` is below `2^96`, so it
/// can neither overflow nor be truncated, which makes the cap comparison exact
/// for every input.
#[must_use]
pub fn priority_fee_lamports(price_micro_lamports: u64, compute_unit_limit: u32) -> u128 {
    (u128::from(price_micro_lamports) * u128::from(compute_unit_limit)).div_ceil(1_000_000)
}

/// Reads exactly `path` (no parent-directory search). A missing file is fine.
fn load_dotenv(path: &Path) -> Result<(), ConfigError> {
    match dotenvy::from_path(path) {
        Ok(()) => Ok(()),
        Err(err) if err.not_found() => Ok(()),
        Err(err) => Err(sanitize_dotenv_error(&err)),
    }
}

/// Snapshots the recognised variables from the process environment.
fn collect_env_vars() -> Result<HashMap<String, String>, ConfigError> {
    let mut vars = HashMap::with_capacity(ENV_KEYS.len());
    for key in ENV_KEYS.into_iter().chain(PRODUCTION_ENV_KEYS) {
        match std::env::var(key) {
            Ok(value) => {
                vars.insert(key.to_owned(), value);
            }
            Err(VarError::NotPresent) => {}
            Err(VarError::NotUnicode(_)) => {
                return Err(ConfigError::InvalidVar {
                    name: key,
                    reason: "value is not valid UTF-8".to_owned(),
                });
            }
        }
    }
    Ok(vars)
}

/// Removes the wallet key and the Yellowstone token from the process
/// environment.
fn scrub_secret_env() {
    std::env::remove_var(ENV_PRIVATE_KEY);
    std::env::remove_var(ENV_YELLOWSTONE_X_TOKEN);
}

/// Maps a `dotenvy` error to a variant that cannot leak file contents
/// (`dotenvy::Error::LineParse` carries the offending line verbatim).
fn sanitize_dotenv_error(err: &dotenvy::Error) -> ConfigError {
    match err {
        dotenvy::Error::LineParse(_, position) => ConfigError::DotEnvSyntax {
            position: *position,
        },
        dotenvy::Error::Io(io_err) => ConfigError::DotEnvIo(io_err.kind()),
        _ => ConfigError::DotEnvUnknown,
    }
}

fn invalid(name: &'static str, reason: &str) -> ConfigError {
    ConfigError::InvalidVar {
        name,
        reason: reason.to_owned(),
    }
}

/// Trims a value and treats blank as absent.
fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn require(name: &'static str, value: Option<String>) -> Result<String, ConfigError> {
    value.ok_or(ConfigError::MissingVar { name })
}

fn parse_number<T>(name: &'static str, text: &str) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    // The parse error of integer types never echoes the input.
    text.parse::<T>()
        .map_err(|err| invalid(name, &format!("not a valid number ({err})")))
}

fn parse_url(name: &'static str, text: &str, schemes: &[&str]) -> Result<Url, ConfigError> {
    // `url::ParseError` never echoes the input, so API keys stay out of errors.
    let url = Url::parse(text).map_err(|err| invalid(name, &format!("not a valid URL ({err})")))?;
    if !schemes.contains(&url.scheme()) {
        return Err(invalid(
            name,
            &format!("scheme must be one of: {}", schemes.join(", ")),
        ));
    }
    if url.host_str().is_none() {
        return Err(invalid(name, "URL must include a host"));
    }
    Ok(url)
}

/// [`parse_url`] for the endpoints that carry orders and API keys
/// (`RPC_URL`, `JITO_ENGINE_URL`): `https`, or plain `http` towards a loopback
/// host only (audit finding #47). The error names the variable, never the URL.
fn parse_url_secure(name: &'static str, text: &str) -> Result<Url, ConfigError> {
    let url = parse_url(name, text, &["http", "https"])?;
    let host = url.host_str().unwrap_or_default();
    if !endpoint_scheme_allowed(url.scheme(), host) {
        return Err(invalid(
            name,
            "must use https (plain http is only allowed for localhost, 127.0.0.1 or [::1])",
        ));
    }
    Ok(url)
}

fn parse_commitment(name: &'static str, text: &str) -> Result<CommitmentConfig, ConfigError> {
    match text.to_ascii_lowercase().as_str() {
        "processed" => Ok(CommitmentConfig::processed()),
        "confirmed" => Ok(CommitmentConfig::confirmed()),
        "finalized" => Ok(CommitmentConfig::finalized()),
        _ => Err(invalid(
            name,
            "expected one of: processed, confirmed, finalized",
        )),
    }
}
