//! The production bridge: everything `main` does that can be tested without a
//! network, a Solana node or the real configuration loader.
//!
//! - [`Cli`]: the command line. `check` validates the setup, `exit` runs the
//!   exit of one position (dry unless `--live`).
//! - [`decide_mode`]: the **dry-to-live barrier at the process level**. Live
//!   trading needs, all at once, `--live`, `--confirm-live` with the exact
//!   phrase, and what the environment already demanded (`DRY_RUN=false`,
//!   `LIVE_TRADING=true`, `LIVE_CONFIRM`, the capital limits and a keypair
//!   path); [`crate::execution::gate::arm`] then re-checks all of it and reads
//!   the keypair file under its `0600` rule. Without `--live` the answer is
//!   always [`Mode::Dry`].
//! - [`preflight_keypair`]: the keypair file check (regular file, not a
//!   symlink, owner-only permissions on Unix, matches the configured wallet).
//! - [`BlockhashSource`] / [`run_blockhash_refresher`]: keeps the sell
//!   submitters supplied with a fresh, timestamped blockhash.
//! - [`Services`]: the background tasks of a session (blockhash refresher and
//!   the tip-floor updater of the [`TipBook`]) with one orderly stop.
//! - [`summarize`]: the end-of-run report **without amounts**.
//!
//! The concrete pieces (Yellowstone gRPC, the RPC blockhash source, the Jito
//! client) are built by `main`; every function here is generic over them.
//!
//! # Logging hygiene
//! Events carry static labels, counters and booleans. Never keys, signatures,
//! addresses, token amounts or balances.

use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

use crate::builder::math::DEFAULT_TOTAL_FEE_BPS;
use crate::builder::pda::CreateVariant;
use crate::errors::{BridgeError, CliError, ExitError, LauncherError, LiveError, TipFeedError};
use crate::execution::gate::load_keypair_file;
use crate::execution::live::ExitReport;
use crate::execution::settings::{LiveSettings, LIVE_CONFIRM_PHRASE};
use crate::execution::tip::TipBook;
use crate::execution::tip_feed::{run_tip_feed, TipFeedSettings, TipFeedStats, TipSource};
use crate::monitor::curve::PriceReference;
use crate::monitor::exit::{ExitEngine, ExitPlan, ExitSettings, Position};
use crate::monitor::submit::{stamp_blockhash, TimedBlockhash};
use crate::monitor::{shutdown_channel, ShutdownSignal, ShutdownTrigger};
use crate::security::SecureWallet;

// --- Command line --------------------------------------------------------------

/// Usage text of the binary.
pub const USAGE: &str = "\
krypton - Solana atomic launcher and exit engine

USAGE:
    krypton check [--live --confirm-live <PHRASE>]
    krypton exit --mint <PUBKEY> --tokens <BASE_UNITS>
                 --entry-sol-reserves <LAMPORTS> --entry-token-reserves <BASE_UNITS>
                 [--tranches <1..>] [--sell-bps <1..=10000>] [--stop-loss-bps <BPS>]
                 [--max-total-cost <LAMPORTS>] [--legacy-create]
                 [--live --confirm-live <PHRASE>]
    krypton help

`check` validates the configuration, the payer keypair file (0600) and the
endpoints without touching the network. `exit` runs the exit of one position
from the Yellowstone stream: a DRY RUN unless `--live` is given together with
`--confirm-live` and the exact confirmation phrase, on top of the environment
switches (DRY_RUN=false, LIVE_TRADING=true, LIVE_CONFIRM, MAX_SOL_PER_TX,
DAILY_DRAWDOWN_SOL, PAYER_KEYPAIR_PATH). See README.md.
";

/// Tranches of the exit unless `--tranches` says otherwise.
pub const DEFAULT_TRANCHES: u8 = 4;
/// Share of the position sold unless `--sell-bps` says otherwise: all of it.
pub const DEFAULT_SELL_BPS: u16 = 10_000;
/// Orders beyond the tranches that the default cost budget allows (stop-loss
/// sweeps).
pub const DEFAULT_EXTRA_ORDERS: u64 = 2;

const VALUE_FLAGS: [&str; 9] = [
    "--confirm-live",
    "--mint",
    "--tokens",
    "--entry-sol-reserves",
    "--entry-token-reserves",
    "--tranches",
    "--sell-bps",
    "--stop-loss-bps",
    "--max-total-cost",
];
const SWITCHES: [&str; 3] = ["--live", "--legacy-create", "--help"];

const FLAG_LIVE: &str = "--live";
const FLAG_CONFIRM: &str = "--confirm-live";
const FLAG_MINT: &str = "--mint";
const FLAG_TOKENS: &str = "--tokens";
const FLAG_ENTRY_SOL: &str = "--entry-sol-reserves";
const FLAG_ENTRY_TOKENS: &str = "--entry-token-reserves";
const FLAG_TRANCHES: &str = "--tranches";
const FLAG_SELL_BPS: &str = "--sell-bps";
const FLAG_STOP_LOSS: &str = "--stop-loss-bps";
const FLAG_MAX_COST: &str = "--max-total-cost";
const FLAG_LEGACY: &str = "--legacy-create";
const FLAG_HELP: &str = "--help";

/// What the operator asked for with `--live` / `--confirm-live`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveRequest {
    /// `--live` was given.
    pub requested: bool,
    /// The value of `--confirm-live`.
    pub confirmation: Option<String>,
}

/// The position to liquidate and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitArgs {
    /// The coin's mint.
    pub mint: Pubkey,
    /// Tokens held, in base units.
    pub tokens: u64,
    /// Virtual SOL reserves of the curve right after our own buy (the entry
    /// price reference of the stop-loss), in lamports.
    pub entry_virtual_sol_reserves: u64,
    /// Virtual token reserves at the same moment, in base units.
    pub entry_virtual_token_reserves: u64,
    /// Number of scheduled tranches.
    pub tranches: u8,
    /// Share of the position the tranches sell together, in basis points.
    pub sell_bps: u16,
    /// Stop-loss, as a price drop in basis points from the entry.
    pub stop_loss_bps: Option<u16>,
    /// Cost budget of all orders together, in lamports (`None`: derived).
    pub max_total_cost_lamports: Option<u64>,
    /// The coin was created with the legacy `create` instruction.
    pub legacy_create: bool,
}

impl ExitArgs {
    /// The create variant the coin's addresses are derived with.
    #[must_use]
    pub fn variant(&self) -> CreateVariant {
        if self.legacy_create {
            CreateVariant::LegacyCreate
        } else {
            CreateVariant::CreateV2
        }
    }

    /// Builds the exit engine: an evenly spaced plan over the execution window
    /// and the cost budget.
    ///
    /// `order_cost_lamports` is the worst-case cost of one order (see
    /// `SellBuilder::order_cost_lamports`). Without `--max-total-cost` the
    /// budget is that cost times the tranches plus [`DEFAULT_EXTRA_ORDERS`].
    ///
    /// # Errors
    /// [`ExitError::InvalidPlan`] / [`ExitError::InvalidConfig`] for a plan,
    /// slippage or position the engine rejects.
    pub fn engine(
        &self,
        slippage_bps: u16,
        order_cost_lamports: u64,
    ) -> Result<ExitEngine, ExitError> {
        let plan = ExitPlan::evenly_spaced(self.tranches, self.sell_bps, self.stop_loss_bps)?;
        let budget = self.max_total_cost_lamports.unwrap_or_else(|| {
            order_cost_lamports
                .saturating_mul(u64::from(self.tranches).saturating_add(DEFAULT_EXTRA_ORDERS))
        });
        ExitEngine::new(
            plan,
            ExitSettings {
                fee_bps: DEFAULT_TOTAL_FEE_BPS,
                slippage_bps,
                order_cost_lamports,
                max_total_cost_lamports: budget,
            },
            Position {
                tokens: self.tokens,
                entry: PriceReference {
                    virtual_sol_reserves: self.entry_virtual_sol_reserves,
                    virtual_token_reserves: self.entry_virtual_token_reserves,
                },
            },
        )
    }
}

/// A parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print the usage.
    Help,
    /// Validate the setup without the network.
    Check,
    /// Run the exit of one position.
    Exit(ExitArgs),
}

/// The parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// The command.
    pub command: Command,
    /// The `--live` request.
    pub live: LiveRequest,
}

#[derive(Default)]
struct RawArgs {
    command: Option<String>,
    values: Vec<(&'static str, String)>,
    switches: Vec<&'static str>,
}

impl RawArgs {
    fn value(&self, flag: &'static str) -> Option<&str> {
        self.values
            .iter()
            .find(|(name, _)| *name == flag)
            .map(|(_, value)| value.as_str())
    }

    fn has(&self, flag: &'static str) -> bool {
        self.switches.contains(&flag)
    }
}

fn scan<I: IntoIterator<Item = String>>(args: I) -> Result<RawArgs, CliError> {
    let mut raw = RawArgs::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "-h" {
            raw.switches.push(FLAG_HELP);
        } else if let Some(body) = arg.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (body, None),
            };
            let flag = format!("--{name}");
            if let Some(known) = SWITCHES.iter().copied().find(|known| *known == flag) {
                if inline.is_some() {
                    return Err(CliError::InvalidValue {
                        flag: known,
                        reason: "takes no value",
                    });
                }
                if raw.has(known) {
                    return Err(CliError::DuplicateFlag { flag: known });
                }
                raw.switches.push(known);
            } else if let Some(known) = VALUE_FLAGS.iter().copied().find(|known| *known == flag) {
                let value = match inline {
                    Some(value) => value,
                    None => match args.next() {
                        Some(next) if !next.starts_with("--") => next,
                        _ => return Err(CliError::MissingValue { flag: known }),
                    },
                };
                if raw.value(known).is_some() {
                    return Err(CliError::DuplicateFlag { flag: known });
                }
                raw.values.push((known, value));
            } else {
                return Err(CliError::UnknownArgument);
            }
        } else if arg.starts_with('-') {
            return Err(CliError::UnknownArgument);
        } else if raw.command.is_none() {
            raw.command = Some(arg);
        } else {
            return Err(CliError::ExtraCommand);
        }
    }
    Ok(raw)
}

fn number<T: FromStr>(raw: &RawArgs, flag: &'static str) -> Result<Option<T>, CliError> {
    raw.value(flag)
        .map(|text| {
            text.parse::<T>().map_err(|_| CliError::InvalidValue {
                flag,
                reason: "not a valid non-negative integer in range",
            })
        })
        .transpose()
}

fn positive<T: FromStr + PartialEq + Default>(
    raw: &RawArgs,
    flag: &'static str,
) -> Result<T, CliError> {
    match number::<T>(raw, flag)? {
        Some(value) if value != T::default() => Ok(value),
        Some(_) => Err(CliError::InvalidValue {
            flag,
            reason: "must be greater than zero",
        }),
        None => Err(CliError::MissingFlag { flag }),
    }
}

fn parse_exit(raw: &RawArgs) -> Result<ExitArgs, CliError> {
    let mint_text = raw
        .value(FLAG_MINT)
        .ok_or(CliError::MissingFlag { flag: FLAG_MINT })?;
    let mint = Pubkey::from_str(mint_text.trim()).map_err(|_| CliError::InvalidValue {
        flag: FLAG_MINT,
        reason: "not a valid public key",
    })?;
    Ok(ExitArgs {
        mint,
        tokens: positive(raw, FLAG_TOKENS)?,
        entry_virtual_sol_reserves: positive(raw, FLAG_ENTRY_SOL)?,
        entry_virtual_token_reserves: positive(raw, FLAG_ENTRY_TOKENS)?,
        tranches: number(raw, FLAG_TRANCHES)?.unwrap_or(DEFAULT_TRANCHES),
        sell_bps: number(raw, FLAG_SELL_BPS)?.unwrap_or(DEFAULT_SELL_BPS),
        stop_loss_bps: number(raw, FLAG_STOP_LOSS)?,
        max_total_cost_lamports: number(raw, FLAG_MAX_COST)?,
        legacy_create: raw.has(FLAG_LEGACY),
    })
}

impl Cli {
    /// Parses the arguments after the program name.
    ///
    /// Flags accept `--flag value` and `--flag=value`. `--live` and
    /// `--confirm-live` are valid for `check` and `exit` only together.
    ///
    /// # Errors
    /// [`CliError`]; arguments are never echoed in it.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, CliError> {
        let raw = scan(args)?;
        if raw.has(FLAG_HELP) {
            return Ok(Self {
                command: Command::Help,
                live: LiveRequest::default(),
            });
        }
        let live = LiveRequest {
            requested: raw.has(FLAG_LIVE),
            confirmation: raw.value(FLAG_CONFIRM).map(str::to_owned),
        };
        if live.confirmation.is_some() && !live.requested {
            return Err(CliError::InvalidValue {
                flag: FLAG_CONFIRM,
                reason: "requires --live",
            });
        }
        let command = match raw.command.as_deref() {
            None => return Err(CliError::NoCommand),
            Some("help") => Command::Help,
            Some("check") => {
                let only_live_flags = raw.values.iter().all(|(name, _)| *name == FLAG_CONFIRM)
                    && !raw.has(FLAG_LEGACY);
                if !only_live_flags {
                    return Err(CliError::UnknownArgument);
                }
                Command::Check
            }
            Some("exit") => Command::Exit(parse_exit(&raw)?),
            Some(_) => return Err(CliError::UnknownCommand),
        };
        Ok(Self { command, live })
    }
}

// --- The dry-to-live barrier -----------------------------------------------------

/// How the session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Everything is assembled and signed; nothing is submitted.
    Dry,
    /// Real funds are spent.
    Live,
}

impl Mode {
    /// A static label for logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Dry => "dry",
            Self::Live => "live",
        }
    }
}

/// The process-level barrier. [`Mode::Live`] only when every condition holds:
///
/// 1. `--live` was given **and** `--confirm-live` carries exactly
///    [`LIVE_CONFIRM_PHRASE`];
/// 2. `LIVE_TRADING=true`, `DRY_RUN=false` and `LIVE_CONFIRM` carries the
///    phrase;
/// 3. a keypair path (`PAYER_KEYPAIR_PATH` / `LIVE_KEYPAIR_PATH`) is configured.
///
/// Without `--live` the result is [`Mode::Dry`] whatever the environment says.
/// The checks are repeated, and the keypair file read under its `0600` rule, by
/// [`crate::execution::live::LiveExecutor::arm`]: this function only decides
/// whether to try, and fails early with the same errors.
///
/// # Errors
/// [`BridgeError::LiveConfirmationMissing`] or the [`LiveError`] of the first
/// missing condition.
pub fn decide_mode(
    request: &LiveRequest,
    dry_run: bool,
    live: &LiveSettings,
) -> Result<Mode, LauncherError> {
    if !request.requested {
        return Ok(Mode::Dry);
    }
    if request.confirmation.as_deref() != Some(LIVE_CONFIRM_PHRASE) {
        return Err(BridgeError::LiveConfirmationMissing.into());
    }
    if !live.live_trading {
        return Err(LiveError::NotArmed.into());
    }
    if dry_run {
        return Err(LiveError::DryRunEnabled.into());
    }
    if !live.confirmed {
        return Err(LiveError::ConfirmationMissing.into());
    }
    if live.keypair_path.is_none() {
        return Err(LiveError::KeypairPathMissing.into());
    }
    Ok(Mode::Live)
}

/// Checks the payer keypair file, if one is configured: a regular file, not a
/// symbolic link, a plausible size, **readable by its owner only (`0600`) on
/// Unix**, a valid keypair, and the same wallet as `PRIVATE_KEY`. The key read
/// is dropped (and wiped) at once.
///
/// Returns `false` when no path is configured. On non-Unix targets the mode
/// check cannot be made; restrict the file with the platform's ACLs.
///
/// # Errors
/// [`LiveError::KeypairFile`] (with a static reason) or
/// [`LiveError::KeypairMismatch`].
pub fn preflight_keypair(
    live: &LiveSettings,
    configured: &SecureWallet,
) -> Result<bool, LauncherError> {
    let Some(path) = live.keypair_path.as_deref() else {
        return Ok(false);
    };
    let wallet = load_keypair_file(path)?;
    if wallet.pubkey() != configured.pubkey() {
        return Err(LiveError::KeypairMismatch.into());
    }
    Ok(true)
}

// --- Blockhash --------------------------------------------------------------------

/// Pause between blockhash refreshes.
pub const DEFAULT_BLOCKHASH_INTERVAL: Duration = Duration::from_secs(2);
/// Lower bound of the refresh interval.
pub const MIN_BLOCKHASH_INTERVAL: Duration = Duration::from_millis(250);
/// Deadline of one fetch.
pub const BLOCKHASH_FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// Where recent blockhashes come from (an RPC node in production).
pub trait BlockhashSource: Send + 'static {
    /// The latest blockhash. Must be cancel safe: it is dropped on shutdown or
    /// after [`BLOCKHASH_FETCH_TIMEOUT`].
    fn latest(&mut self) -> impl Future<Output = Result<Hash, BridgeError>> + Send;
}

/// What a refresher did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefresherStats {
    /// Blockhashes published.
    pub refreshed: u64,
    /// Failed or timed-out fetches.
    pub failures: u64,
}

/// A refresher's inputs.
pub struct BlockhashFeed<B> {
    /// The source.
    pub source: B,
    /// The channel the submitters read; seed it with the first blockhash
    /// (`blockhash_channel`) before starting.
    pub sender: watch::Sender<TimedBlockhash>,
    /// Pause between fetches.
    pub interval: Duration,
}

/// Publishes a fresh, timestamped blockhash every `interval` until `shutdown`.
///
/// A failed fetch keeps the previous value: the submitters refuse a blockhash
/// older than `MAX_BLOCKHASH_AGE`, so a dead source ends in refused orders, not
/// in orders signed with a stale blockhash.
pub async fn run_blockhash_refresher<S: BlockhashSource>(
    mut source: S,
    sender: watch::Sender<TimedBlockhash>,
    interval: Duration,
    mut shutdown: ShutdownSignal,
) -> RefresherStats {
    let interval = interval.max(MIN_BLOCKHASH_INTERVAL);
    let mut stats = RefresherStats::default();
    loop {
        tokio::select! {
            biased;
            () = shutdown.wait() => break,
            () = sleep(interval) => {}
        }
        let fetched = tokio::select! {
            biased;
            () = shutdown.wait() => break,
            fetched = timeout(BLOCKHASH_FETCH_TIMEOUT, source.latest()) => fetched,
        };
        match fetched {
            Ok(Ok(hash)) => {
                sender.send_replace(stamp_blockhash(hash));
                stats.refreshed = stats.refreshed.saturating_add(1);
            }
            Ok(Err(error)) => {
                stats.failures = stats.failures.saturating_add(1);
                warn!(
                    error = error.label(),
                    failures = stats.failures,
                    "blockhash refresh failed; keeping the last one"
                );
            }
            Err(_) => {
                stats.failures = stats.failures.saturating_add(1);
                warn!(
                    failures = stats.failures,
                    "blockhash refresh timed out; keeping the last one"
                );
            }
        }
    }
    stats
}

// --- Services ---------------------------------------------------------------------

/// A tip-floor updater's inputs.
pub struct TipFeed<T> {
    /// The source (`HttpTipSource` in production).
    pub source: T,
    /// The book the executor quotes from (`LiveExecutor::tips`).
    pub book: Arc<TipBook>,
    /// Polling parameters.
    pub settings: TipFeedSettings,
}

/// The background tasks of a session. Dropping it without [`Services::stop`]
/// still stops them (the shutdown trigger goes with it).
pub struct Services {
    trigger: ShutdownTrigger,
    signal: ShutdownSignal,
    blockhash: Option<JoinHandle<RefresherStats>>,
    tip_feed: Option<JoinHandle<Result<TipFeedStats, TipFeedError>>>,
}

impl Default for Services {
    fn default() -> Self {
        Self::new()
    }
}

impl Services {
    /// No tasks yet.
    #[must_use]
    pub fn new() -> Self {
        let (trigger, signal) = shutdown_channel();
        Self {
            trigger,
            signal,
            blockhash: None,
            tip_feed: None,
        }
    }

    /// Starts the blockhash refresher.
    #[must_use]
    pub fn with_blockhash<B: BlockhashSource>(mut self, feed: BlockhashFeed<B>) -> Self {
        self.blockhash = Some(tokio::spawn(run_blockhash_refresher(
            feed.source,
            feed.sender,
            feed.interval,
            self.signal.clone(),
        )));
        self
    }

    /// Starts the tip-floor updater.
    #[must_use]
    pub fn with_tip_feed<T: TipSource + 'static>(mut self, feed: TipFeed<T>) -> Self {
        self.tip_feed = Some(tokio::spawn(run_tip_feed(
            feed.source,
            feed.book,
            feed.settings,
            self.signal.clone(),
        )));
        self
    }

    /// Stops every task and collects what they did.
    ///
    /// # Errors
    /// [`BridgeError::ServiceFailed`] if a task panicked or the tip feed ended
    /// with an error (bad settings, unusable tip book).
    pub async fn stop(self) -> Result<ServicesReport, BridgeError> {
        self.trigger.trigger();
        let blockhash = match self.blockhash {
            Some(handle) => handle.await.map_err(|_| BridgeError::ServiceFailed {
                service: "blockhash",
            })?,
            None => RefresherStats::default(),
        };
        let tip_feed = match self.tip_feed {
            Some(handle) => Some(
                handle
                    .await
                    .map_err(|_| BridgeError::ServiceFailed {
                        service: "tip_feed",
                    })?
                    .map_err(|_| BridgeError::ServiceFailed {
                        service: "tip_feed",
                    })?,
            ),
            None => None,
        };
        Ok(ServicesReport {
            blockhash,
            tip_feed,
        })
    }
}

/// What the services did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServicesReport {
    /// The blockhash refresher's counters.
    pub blockhash: RefresherStats,
    /// The tip updater's counters, if one ran.
    pub tip_feed: Option<TipFeedStats>,
}

// --- Report -------------------------------------------------------------------------

/// The end-of-run facts that are safe to log: counters and flags, no amounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSummary {
    /// Orders issued.
    pub orders_issued: u32,
    /// Tranches and sweeps dropped.
    pub dropped: u32,
    /// The stop-loss fired.
    pub stop_loss_fired: bool,
    /// Curve observations applied.
    pub observations: u64,
    /// Curve updates that failed to decode.
    pub decode_errors: u64,
    /// Stream reconnections.
    pub reconnects: u64,
    /// The run was interrupted before the exit finished.
    pub interrupted: bool,
    /// Some order's outcome is unknown or unreported: check the token account
    /// before doing anything else with the position.
    pub needs_attention: bool,
}

/// Reduces an exit report to the loggable facts.
#[must_use]
pub fn summarize(report: &ExitReport) -> SessionSummary {
    let position = &report.summary.position;
    SessionSummary {
        orders_issued: position.orders_issued,
        dropped: position.dropped,
        stop_loss_fired: position.stop_loss_fired,
        observations: report.summary.observations,
        decode_errors: report.summary.decode_errors,
        reconnects: report.health.reconnects,
        interrupted: report.summary.interrupted,
        needs_attention: position.tokens_unresolved > 0
            || position.tokens_in_flight > 0
            || report.summary.unsettled,
    }
}

/// Logs the summary at `info` (or `warn` when it needs attention).
pub fn log_summary(mode: Mode, summary: &SessionSummary) {
    if summary.needs_attention {
        warn!(
            mode = mode.label(),
            orders = summary.orders_issued,
            dropped = summary.dropped,
            stop_loss = summary.stop_loss_fired,
            interrupted = summary.interrupted,
            "exit ended with unresolved orders: check the token account before acting"
        );
    } else {
        info!(
            mode = mode.label(),
            orders = summary.orders_issued,
            dropped = summary.dropped,
            stop_loss = summary.stop_loss_fired,
            observations = summary.observations,
            decode_errors = summary.decode_errors,
            reconnects = summary.reconnects,
            interrupted = summary.interrupted,
            "exit finished"
        );
    }
}
