//! Phase 7 test-suite: the dry-to-live barrier, the spending guards, the dynamic
//! Jito tip and live execution against a scripted Block Engine.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - "Mainnet" is mocked at the `BlockEngine` seam with scripted answers: no test
//!   opens a socket and none can spend real funds. The HTTP client of the real
//!   `JitoClient` is covered by `phase4_tests.rs`.
//! - Time-dependent tests run on Tokio's paused clock; the guards take the time as
//!   an argument, so day rollovers are exact.
//! - Keypair files are written to a per-process temp directory with mode `0600`
//!   (Unix); the permission checks are Unix-only and so are their tests.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use krypton::builder::alt::LaunchLookupTable;
use krypton::builder::math::{BuyPlan, CurveParams, SellPlan, DEFAULT_TOTAL_FEE_BPS};
use krypton::builder::sell::{SellBuilder, SellSettings};
use krypton::builder::transaction::{LaunchBuilder, LaunchParams, MintSigner};
use krypton::builder::{CreateOptions, CreateVariant, LaunchMetadata};
use krypton::config::{
    Config, ABSOLUTE_MAX_JITO_TIP_LAMPORTS, MAX_SLIPPAGE_BPS, MIN_JITO_TIP_LAMPORTS,
};
use krypton::errors::{ExitError, GuardError, JitoError, LiveError, MonitorError};
use krypton::execution::gate::{self, ArmedLive, MAX_KEYPAIR_FILE_BYTES};
use krypton::execution::guards::{GuardLimits, SpendKind, SpendingGuards, SECONDS_PER_DAY};
use krypton::execution::live::{
    subscription_for, BlockEngine, BundleOutcome, Clock, ExitRun, LaunchRequest, LiveExecutor,
};
use krypton::execution::settings::{
    LiveSettings, RawLiveSettings, ABSOLUTE_MAX_DAILY_DRAWDOWN_LAMPORTS,
    ABSOLUTE_MAX_SPEND_PER_TX_LAMPORTS, LIVE_CONFIRM_PHRASE,
};
use krypton::execution::tip::{
    Percentile, TipBasis, TipBook, TipFloor, TipPolicy, Urgency, DEFAULT_MAX_FLOOR_AGE,
};
use krypton::monitor::curve::PriceReference;
use krypton::monitor::exit::{
    ExitEngine, ExitPlan, ExitSettings, ExitTrigger, Position, SellOrder, SellOutcome,
    SellSubmitter,
};
use krypton::monitor::geyser::{
    EventStream, MonitorSettings, StreamCommitment, StreamTransport, StreamUpdate,
    SubscriptionRequest,
};
use krypton::monitor::shutdown_channel;
use krypton::monitor::submit::blockhash_channel;
use krypton::network::{BundleId, InflightStatus, JITO_TIP_ACCOUNTS};
use krypton::security::SecureWallet;
use krypton::simulation::market::MarketState;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use solana_sdk::transaction::VersionedTransaction;

type TestResult = Result<(), Box<dyn Error>>;

// --- Helpers -------------------------------------------------------------------------------------

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn keypair_bytes(seed: u8) -> [u8; 64] {
    Keypair::new_from_array([seed; 32]).to_bytes()
}

fn keypair_json(seed: u8) -> String {
    let items: Vec<String> = keypair_bytes(seed)
        .iter()
        .map(ToString::to_string)
        .collect();
    format!("[{}]", items.join(","))
}

fn wallet(seed: u8) -> Result<SecureWallet, Box<dyn Error>> {
    Ok(SecureWallet::from_keypair_bytes(&keypair_bytes(seed))?)
}

/// A scratch directory, unique per process and tag.
fn scratch(tag: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("krypton-p7-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), Box<dyn Error>> {
    Ok(())
}

fn write_keypair_file(dir: &Path, name: &str, seed: u8) -> Result<PathBuf, Box<dyn Error>> {
    let path = dir.join(name);
    std::fs::write(&path, keypair_json(seed))?;
    set_mode(&path, 0o600)?;
    Ok(path)
}

fn raw_live(pairs: &[(&str, &str)]) -> RawLiveSettings {
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_owned())
    };
    RawLiveSettings {
        live_trading: get("LIVE_TRADING"),
        live_confirm: get("LIVE_CONFIRM"),
        live_keypair_path: get("LIVE_KEYPAIR_PATH"),
        max_sol_per_tx: get("MAX_SOL_PER_TX"),
        daily_drawdown_sol: get("DAILY_DRAWDOWN_SOL"),
    }
}

/// Settings that arm, around the keypair file at `path`.
fn armed_settings(path: &Path) -> LiveSettings {
    LiveSettings {
        live_trading: true,
        confirmed: true,
        keypair_path: Some(path.to_path_buf()),
        max_spend_per_tx_lamports: Some(2_000_000_000),
        daily_drawdown_lamports: Some(5_000_000_000),
    }
}

fn config_vars(extra: &[(&str, &str)]) -> HashMap<String, String> {
    let mut vars: HashMap<String, String> = [
        ("RPC_URL", "https://rpc.example.com"),
        ("JITO_ENGINE_URL", "https://jito.example.com"),
        ("CREATOR_SOL_AMOUNT", "0.5"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    vars.insert("PRIVATE_KEY".to_owned(), keypair_json(7));
    for (key, value) in extra {
        vars.insert((*key).to_owned(), (*value).to_owned());
    }
    vars
}

// =====================================================================================================
// Live settings and configuration parsing
// =====================================================================================================

#[test]
fn live_settings_default_to_not_armed() -> TestResult {
    let settings = LiveSettings::from_raw(RawLiveSettings::default())?;
    assert_eq!(settings, LiveSettings::default());
    assert!(!settings.live_trading && !settings.confirmed);
    assert!(settings.keypair_path.is_none());
    assert!(settings.max_spend_per_tx_lamports.is_none());
    assert!(settings.daily_drawdown_lamports.is_none());
    Ok(())
}

#[test]
fn live_settings_parse_every_field() -> TestResult {
    let settings = LiveSettings::from_raw(raw_live(&[
        ("LIVE_TRADING", "true"),
        ("LIVE_CONFIRM", LIVE_CONFIRM_PHRASE),
        ("LIVE_KEYPAIR_PATH", "/keys/live.json"),
        ("MAX_SOL_PER_TX", "0.5"),
        ("DAILY_DRAWDOWN_SOL", "2.25"),
    ]))?;
    assert!(settings.live_trading && settings.confirmed);
    assert_eq!(
        settings.keypair_path,
        Some(PathBuf::from("/keys/live.json"))
    );
    assert_eq!(settings.max_spend_per_tx_lamports, Some(500_000_000));
    assert_eq!(settings.daily_drawdown_lamports, Some(2_250_000_000));
    Ok(())
}

#[test]
fn blank_live_values_count_as_not_set() -> TestResult {
    let settings = LiveSettings::from_raw(raw_live(&[
        ("LIVE_TRADING", "  "),
        ("LIVE_CONFIRM", ""),
        ("LIVE_KEYPAIR_PATH", " "),
        ("MAX_SOL_PER_TX", ""),
    ]))?;
    assert_eq!(settings, LiveSettings::default());
    Ok(())
}

#[test]
fn malformed_live_values_are_rejected_without_echoing_them() -> TestResult {
    let cases: [(&str, &str, &str); 8] = [
        ("LIVE_TRADING", "sure-why-not", "LIVE_TRADING"),
        ("LIVE_CONFIRM", "yes please", "LIVE_CONFIRM"),
        (
            "LIVE_CONFIRM",
            "i-understand-real-funds-will-be-spent",
            "LIVE_CONFIRM",
        ),
        ("MAX_SOL_PER_TX", "abc", "MAX_SOL_PER_TX"),
        ("MAX_SOL_PER_TX", "0", "MAX_SOL_PER_TX"),
        ("MAX_SOL_PER_TX", "10.000000001", "MAX_SOL_PER_TX"),
        ("DAILY_DRAWDOWN_SOL", "50.5", "DAILY_DRAWDOWN_SOL"),
        ("DAILY_DRAWDOWN_SOL", "-1", "DAILY_DRAWDOWN_SOL"),
    ];
    for (key, value, name) in cases {
        let error = err_of(LiveSettings::from_raw(raw_live(&[(key, value)])))?;
        let text = error.to_string();
        assert!(text.contains(name), "{text}");
        assert!(!text.contains(value), "the value leaked into: {text}");
    }
    Ok(())
}

#[test]
fn limits_can_reach_their_ceilings_but_not_pass_them() -> TestResult {
    let at_limit = LiveSettings::from_raw(raw_live(&[
        ("MAX_SOL_PER_TX", "10"),
        ("DAILY_DRAWDOWN_SOL", "50"),
    ]))?;
    assert_eq!(
        at_limit.max_spend_per_tx_lamports,
        Some(ABSOLUTE_MAX_SPEND_PER_TX_LAMPORTS)
    );
    assert_eq!(
        at_limit.daily_drawdown_lamports,
        Some(ABSOLUTE_MAX_DAILY_DRAWDOWN_LAMPORTS)
    );
    Ok(())
}

#[test]
fn live_settings_debug_hides_the_keypair_path() -> TestResult {
    let settings = LiveSettings::from_raw(raw_live(&[(
        "LIVE_KEYPAIR_PATH",
        "/home/someone/secret-keys/live.json",
    )]))?;
    let text = format!("{settings:?}");
    assert!(
        !text.contains("someone") && !text.contains("secret-keys"),
        "{text}"
    );
    Ok(())
}

#[test]
fn config_without_live_variables_cannot_arm() -> TestResult {
    let config = Config::from_vars(config_vars(&[]))?;
    assert!(config.dry_run, "DRY_RUN defaults to true");
    assert_eq!(config.live, LiveSettings::default());
    let error = err_of(LiveExecutor::arm(&config))?;
    assert!(matches!(error, LiveError::NotArmed), "{error:?}");
    Ok(())
}

#[test]
fn config_carries_the_live_variables() -> TestResult {
    let config = Config::from_vars(config_vars(&[
        ("LIVE_TRADING", "true"),
        ("LIVE_CONFIRM", LIVE_CONFIRM_PHRASE),
        ("LIVE_KEYPAIR_PATH", "/keys/live.json"),
        ("MAX_SOL_PER_TX", "1"),
        ("DAILY_DRAWDOWN_SOL", "3"),
        ("DRY_RUN", "false"),
    ]))?;
    assert!(!config.dry_run);
    assert!(config.live.live_trading && config.live.confirmed);
    assert_eq!(config.live.max_spend_per_tx_lamports, Some(1_000_000_000));
    assert_eq!(config.live.daily_drawdown_lamports, Some(3_000_000_000));
    Ok(())
}

#[test]
fn config_rejects_a_wrong_confirmation_phrase() -> TestResult {
    let error = err_of(Config::from_vars(config_vars(&[("LIVE_CONFIRM", "yes")])))?;
    assert!(error.to_string().contains("LIVE_CONFIRM"));
    Ok(())
}

// =====================================================================================================
// The dry-to-live barrier
// =====================================================================================================

fn arm_with(
    dry_run: bool,
    settings: &LiveSettings,
    configured: &SecureWallet,
) -> Result<(SecureWallet, ArmedLive), LiveError> {
    gate::arm(dry_run, settings, configured, 500, 100_000, 1_000_000)
}

#[test]
fn barrier_opens_only_when_everything_is_in_place() -> TestResult {
    let dir = scratch("open")?;
    let path = write_keypair_file(&dir, "live.json", 7)?;
    let configured = wallet(7)?;
    let (loaded, armed) = arm_with(false, &armed_settings(&path), &configured)?;
    assert_eq!(loaded.pubkey(), configured.pubkey());
    assert_eq!(armed.limits.max_per_tx_lamports(), 2_000_000_000);
    assert_eq!(armed.limits.daily_drawdown_lamports(), 5_000_000_000);
    assert_eq!(armed.limits.max_slippage_bps(), 500);
    assert_eq!(armed.tip_policy.fallback_lamports(), 100_000);
    assert_eq!(armed.tip_policy.cap_lamports(), 1_000_000);
    Ok(())
}

#[test]
fn each_missing_switch_is_refused_with_its_own_error() -> TestResult {
    let dir = scratch("switches")?;
    let path = write_keypair_file(&dir, "live.json", 7)?;
    let configured = wallet(7)?;
    let good = armed_settings(&path);

    let mut not_armed = good.clone();
    not_armed.live_trading = false;
    assert!(matches!(
        err_of(arm_with(false, &not_armed, &configured))?,
        LiveError::NotArmed
    ));

    assert!(
        matches!(
            err_of(arm_with(true, &good, &configured))?,
            LiveError::DryRunEnabled
        ),
        "LIVE_TRADING with DRY_RUN=true is a contradiction"
    );

    let mut unconfirmed = good.clone();
    unconfirmed.confirmed = false;
    assert!(matches!(
        err_of(arm_with(false, &unconfirmed, &configured))?,
        LiveError::ConfirmationMissing
    ));

    let mut no_cap = good.clone();
    no_cap.max_spend_per_tx_lamports = None;
    assert!(matches!(
        err_of(arm_with(false, &no_cap, &configured))?,
        LiveError::LimitMissing {
            name: "MAX_SOL_PER_TX"
        }
    ));

    let mut no_daily = good.clone();
    no_daily.daily_drawdown_lamports = None;
    assert!(matches!(
        err_of(arm_with(false, &no_daily, &configured))?,
        LiveError::LimitMissing {
            name: "DAILY_DRAWDOWN_SOL"
        }
    ));

    let mut no_path = good.clone();
    no_path.keypair_path = None;
    assert!(matches!(
        err_of(arm_with(false, &no_path, &configured))?,
        LiveError::KeypairPathMissing
    ));
    Ok(())
}

#[test]
fn the_default_settings_never_arm() -> TestResult {
    let configured = wallet(7)?;
    for dry_run in [true, false] {
        let error = err_of(arm_with(dry_run, &LiveSettings::default(), &configured))?;
        assert!(matches!(error, LiveError::NotArmed), "{error:?}");
    }
    Ok(())
}

#[test]
fn a_keypair_file_that_is_not_the_configured_wallet_is_refused() -> TestResult {
    let dir = scratch("mismatch")?;
    let path = write_keypair_file(&dir, "other.json", 9)?;
    let error = err_of(arm_with(false, &armed_settings(&path), &wallet(7)?))?;
    assert!(matches!(error, LiveError::KeypairMismatch), "{error:?}");
    Ok(())
}

fn keypair_file_reason(path: &Path) -> Result<&'static str, Box<dyn Error>> {
    match err_of(gate::load_keypair_file(path))? {
        LiveError::KeypairFile { reason } => Ok(reason),
        other => Err(format!("unexpected error: {other:?}").into()),
    }
}

#[test]
fn keypair_file_checks_reject_every_unsafe_shape() -> TestResult {
    let dir = scratch("files")?;

    assert_eq!(
        keypair_file_reason(&dir.join("missing.json"))?,
        "cannot be read"
    );
    assert_eq!(keypair_file_reason(&dir)?, "is not a regular file");

    let empty = dir.join("empty.json");
    std::fs::write(&empty, "")?;
    set_mode(&empty, 0o600)?;
    assert_eq!(keypair_file_reason(&empty)?, "has an unexpected size");

    let huge = dir.join("huge.json");
    let size = usize::try_from(MAX_KEYPAIR_FILE_BYTES)? + 1;
    std::fs::write(&huge, vec![b'['; size])?;
    set_mode(&huge, 0o600)?;
    assert_eq!(keypair_file_reason(&huge)?, "has an unexpected size");

    let garbage = dir.join("garbage.json");
    std::fs::write(&garbage, "definitely not a keypair")?;
    set_mode(&garbage, 0o600)?;
    assert_eq!(keypair_file_reason(&garbage)?, "is not a valid keypair");

    let binary = dir.join("binary.json");
    std::fs::write(&binary, [0xff_u8, 0xfe, 0xfd])?;
    set_mode(&binary, 0o600)?;
    assert_eq!(keypair_file_reason(&binary)?, "is not valid UTF-8");
    Ok(())
}

#[cfg(unix)]
#[test]
fn keypair_file_must_be_private_and_not_a_symlink() -> TestResult {
    let dir = scratch("unix")?;
    let loose = write_keypair_file(&dir, "loose.json", 7)?;
    for mode in [0o644, 0o640, 0o604, 0o666] {
        set_mode(&loose, mode)?;
        assert_eq!(
            keypair_file_reason(&loose)?,
            "must be readable by its owner only (chmod 600)",
            "mode {mode:o}"
        );
    }
    for mode in [0o600, 0o400] {
        set_mode(&loose, mode)?;
        assert_eq!(
            gate::load_keypair_file(&loose)?.pubkey(),
            wallet(7)?.pubkey()
        );
    }

    let link = dir.join("link.json");
    std::os::unix::fs::symlink(&loose, &link)?;
    assert_eq!(keypair_file_reason(&link)?, "is a symbolic link");
    Ok(())
}

#[test]
fn keypair_errors_never_name_the_path() -> TestResult {
    let dir = scratch("secretive-dir")?;
    let error = err_of(gate::load_keypair_file(&dir.join("hidden-name.json")))?;
    let text = error.to_string();
    assert!(
        !text.contains("secretive") && !text.contains("hidden-name"),
        "{text}"
    );
    Ok(())
}

#[test]
fn barrier_enforces_the_slippage_ceiling_and_the_limit_relationships() -> TestResult {
    let dir = scratch("limits")?;
    let path = write_keypair_file(&dir, "live.json", 7)?;
    let configured = wallet(7)?;

    let error = err_of(gate::arm(
        false,
        &armed_settings(&path),
        &configured,
        MAX_SLIPPAGE_BPS + 1,
        100_000,
        1_000_000,
    ))?;
    assert!(
        matches!(error, LiveError::Guard(GuardError::SlippageTooHigh)),
        "{error:?}"
    );

    let mut inverted = armed_settings(&path);
    inverted.max_spend_per_tx_lamports = Some(3_000_000_000);
    inverted.daily_drawdown_lamports = Some(2_000_000_000);
    let error = err_of(arm_with(false, &inverted, &configured))?;
    assert!(
        matches!(error, LiveError::Guard(GuardError::InvalidLimits { .. })),
        "{error:?}"
    );

    let error = err_of(gate::arm(
        false,
        &armed_settings(&path),
        &configured,
        500,
        2_000_000,
        1_000_000,
    ))?;
    assert!(
        matches!(error, LiveError::InvalidTipPolicy { .. }),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn arm_from_a_config_crosses_the_barrier_end_to_end() -> TestResult {
    let dir = scratch("config-arm")?;
    let path = write_keypair_file(&dir, "live.json", 7)?;
    let path_text = path.to_str().ok_or("non UTF-8 temp path")?;
    let vars = |dry_run: &str| {
        config_vars(&[
            ("LIVE_TRADING", "true"),
            ("LIVE_CONFIRM", LIVE_CONFIRM_PHRASE),
            ("LIVE_KEYPAIR_PATH", path_text),
            ("MAX_SOL_PER_TX", "1"),
            ("DAILY_DRAWDOWN_SOL", "3"),
            ("DRY_RUN", dry_run),
        ])
    };
    let dry = Config::from_vars(vars("true"))?;
    assert!(matches!(
        err_of(LiveExecutor::arm(&dry))?,
        LiveError::DryRunEnabled
    ));
    let live = Config::from_vars(vars("false"))?;
    let executor = LiveExecutor::arm(&live)?;
    assert_eq!(
        executor.guards().limits().max_per_tx_lamports(),
        1_000_000_000
    );
    Ok(())
}

// =====================================================================================================
// Spending guards
// =====================================================================================================

const DAY: u64 = SECONDS_PER_DAY;

fn limits(per_tx: u64, daily: u64) -> Result<GuardLimits, Box<dyn Error>> {
    Ok(GuardLimits::new(per_tx, daily, 500)?)
}

#[test]
fn guard_limits_are_validated() -> TestResult {
    assert!(GuardLimits::new(1, 1, 0).is_ok());
    for (per_tx, daily) in [
        (0, 1_000),
        (1_000, 0),
        (
            ABSOLUTE_MAX_SPEND_PER_TX_LAMPORTS + 1,
            ABSOLUTE_MAX_DAILY_DRAWDOWN_LAMPORTS,
        ),
        (1_000, ABSOLUTE_MAX_DAILY_DRAWDOWN_LAMPORTS + 1),
        (2_000, 1_000),
    ] {
        let error = err_of(GuardLimits::new(per_tx, daily, 500))?;
        assert!(
            matches!(error, GuardError::InvalidLimits { .. }),
            "{per_tx}/{daily}: {error:?}"
        );
    }
    assert_eq!(
        err_of(GuardLimits::new(1_000, 1_000, MAX_SLIPPAGE_BPS + 1))?,
        GuardError::SlippageTooHigh
    );
    assert!(
        GuardLimits::new(1_000, 1_000, MAX_SLIPPAGE_BPS).is_ok(),
        "10 % itself is allowed"
    );
    Ok(())
}

#[test]
fn slippage_check_uses_the_configured_ceiling() -> TestResult {
    let guards = SpendingGuards::new(GuardLimits::new(1_000, 2_000, 300)?);
    guards.check_slippage(0)?;
    guards.check_slippage(300)?;
    assert_eq!(
        err_of(guards.check_slippage(301))?,
        GuardError::SlippageTooHigh
    );
    Ok(())
}

#[test]
fn per_transaction_cap_applies_to_every_kind() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 10_000)?);
    for kind in [SpendKind::Entry, SpendKind::Exit] {
        let allowed = guards.reserve(kind, 1_000, 0)?;
        assert_eq!(allowed.lamports(), 1_000);
        assert_eq!(allowed.kind(), kind);
        guards.release(allowed)?;
        assert_eq!(
            err_of(guards.reserve(kind, 1_001, 0))?,
            GuardError::PerTransactionCap
        );
    }
    assert_eq!(guards.snapshot()?.reserved_lamports, 0);
    Ok(())
}

#[test]
fn open_reservations_count_against_the_daily_limit() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 2_500)?);
    let first = guards.reserve(SpendKind::Entry, 1_000, 0)?;
    let second = guards.reserve(SpendKind::Entry, 1_000, 0)?;
    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 1_000, 0))?,
        GuardError::DailyDrawdown,
        "two orders in flight already use 2000 of 2500"
    );
    guards.release(second)?;
    let third = guards.reserve(SpendKind::Entry, 500, 0)?;
    guards.commit(first, 0)?;
    guards.commit(third, 0)?;
    let snapshot = guards.snapshot()?;
    assert_eq!(snapshot.committed_lamports, 1_500);
    assert_eq!(snapshot.reserved_lamports, 0);
    assert_eq!(snapshot.realised_drawdown_lamports, 1_500);
    assert!(!snapshot.halted);
    Ok(())
}

#[test]
fn tripwire_halts_entries_but_never_exits() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 1_000)?);
    let entry = guards.reserve(SpendKind::Entry, 1_000, 0)?;
    guards.commit(entry, 0)?;
    assert!(
        guards.is_halted()?,
        "realised spend reached the daily limit"
    );

    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 1, 0))?,
        GuardError::Halted
    );
    // Selling is how the position is closed: never blocked by the halt.
    let exit = guards.reserve(SpendKind::Exit, 200, 0)?;
    guards.commit(exit, 0)?;
    Ok(())
}

#[test]
fn exit_fees_do_not_need_drawdown_headroom() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 1_000)?);
    let entry = guards.reserve(SpendKind::Entry, 900, 0)?;
    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 200, 0))?,
        GuardError::DailyDrawdown
    );
    // 900 reserved + 200 would pass the daily limit, yet an exit still goes through.
    let exit = guards.reserve(SpendKind::Exit, 200, 0)?;
    guards.commit(exit, 0)?;
    guards.release(entry)?;
    Ok(())
}

#[test]
fn returns_offset_spend_but_never_create_credit() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 1_500)?);
    let entry = guards.reserve(SpendKind::Entry, 1_000, 0)?;
    guards.commit(entry, 0)?;
    guards.record_return(600, 0)?;
    assert_eq!(guards.snapshot()?.realised_drawdown_lamports, 400);

    // A windfall far above the spend brings the drawdown to zero, not below.
    guards.record_return(1_000_000, 0)?;
    assert_eq!(guards.snapshot()?.realised_drawdown_lamports, 0);
    // ...and does not widen the allowance: the daily limit still bounds one day.
    let more = guards.reserve(SpendKind::Entry, 1_000, 0)?;
    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 1_000, 0))?,
        GuardError::DailyDrawdown
    );
    guards.release(more)?;
    Ok(())
}

#[test]
fn a_new_day_resets_the_books_but_not_the_halt() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 1_000)?);
    let entry = guards.reserve(SpendKind::Entry, 1_000, 10)?;
    guards.commit(entry, 10)?;
    assert!(guards.is_halted()?);

    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 100, DAY + 10))?,
        GuardError::Halted,
        "the latch survives midnight"
    );
    let snapshot = guards.snapshot()?;
    assert_eq!(snapshot.day, 1);
    assert_eq!(snapshot.committed_lamports, 0, "the counters did reset");

    guards.rearm()?;
    assert!(!guards.is_halted()?);
    let entry = guards.reserve(SpendKind::Entry, 1_000, DAY + 20)?;
    guards.commit(entry, DAY + 20)?;
    Ok(())
}

#[test]
fn a_new_day_without_a_halt_restores_the_allowance() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 1_500)?);
    let entry = guards.reserve(SpendKind::Entry, 1_000, 5)?;
    guards.commit(entry, 5)?;
    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 1_000, 6))?,
        GuardError::DailyDrawdown
    );
    let next_day = guards.reserve(SpendKind::Entry, 1_000, DAY)?;
    guards.release(next_day)?;
    Ok(())
}

#[test]
fn reservations_survive_a_day_rollover() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 1_500)?);
    let in_flight = guards.reserve(SpendKind::Entry, 1_000, DAY - 1)?;
    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 1_000, DAY + 1))?,
        GuardError::DailyDrawdown,
        "the order in flight still counts after midnight"
    );
    guards.commit(in_flight, DAY + 1)?;
    assert_eq!(guards.snapshot()?.committed_lamports, 1_000);
    Ok(())
}

#[test]
fn release_frees_the_money_without_spending_it() -> TestResult {
    let guards = SpendingGuards::new(limits(1_000, 1_000)?);
    for _ in 0..5 {
        let reservation = guards.reserve(SpendKind::Entry, 1_000, 0)?;
        guards.release(reservation)?;
    }
    let snapshot = guards.snapshot()?;
    assert_eq!(
        (snapshot.committed_lamports, snapshot.reserved_lamports),
        (0, 0)
    );
    assert!(!snapshot.halted);
    Ok(())
}

#[test]
fn concurrent_reservations_never_overshoot_the_daily_limit() -> TestResult {
    const THREADS: usize = 8;
    const ATTEMPTS: usize = 100;
    let guards = SpendingGuards::new(limits(1_000_000, 50_000_000)?);
    let granted = AtomicUsize::new(0);
    std::thread::scope(|scope| -> TestResult {
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let (guards, granted) = (&guards, &granted);
                scope.spawn(move || {
                    let mut kept = Vec::new();
                    for _ in 0..ATTEMPTS {
                        if let Ok(reservation) = guards.reserve(SpendKind::Entry, 1_000_000, 0) {
                            granted.fetch_add(1, Ordering::SeqCst);
                            kept.push(reservation);
                        }
                    }
                    kept
                })
            })
            .collect();
        for worker in workers {
            let _kept = worker.join().map_err(|_| "a guard thread panicked")?;
        }
        Ok(())
    })?;
    assert_eq!(
        granted.load(Ordering::SeqCst),
        50,
        "exactly the daily limit, never more"
    );
    assert_eq!(guards.snapshot()?.reserved_lamports, 50_000_000);
    Ok(())
}

#[test]
fn guard_errors_do_not_leak_amounts() {
    // Only the documented 10 % ceiling may appear as a number.
    for error in [
        GuardError::PerTransactionCap,
        GuardError::DailyDrawdown,
        GuardError::Halted,
        GuardError::StatePoisoned,
    ] {
        let text = error.to_string();
        assert!(!text.chars().any(|c| c.is_ascii_digit()), "{text}");
    }
    assert!(GuardError::SlippageTooHigh.to_string().contains("10%"));
}

// =====================================================================================================
// Dynamic tip
// =====================================================================================================

fn floor() -> TipFloor {
    TipFloor {
        p50: 20_000,
        p75: 50_000,
        p95: 200_000,
        p99: 900_000,
        ema_p50: 40_000,
    }
}

fn policy() -> Result<TipPolicy, Box<dyn Error>> {
    Ok(TipPolicy::new(10_000, 1_000_000)?)
}

const FRESH: Duration = Duration::from_secs(1);

#[test]
fn policy_parameters_are_validated() -> TestResult {
    assert!(TipPolicy::new(MIN_JITO_TIP_LAMPORTS, MIN_JITO_TIP_LAMPORTS).is_ok());
    for (fallback, cap) in [
        (MIN_JITO_TIP_LAMPORTS - 1, 1_000_000),
        (10_000, ABSOLUTE_MAX_JITO_TIP_LAMPORTS + 1),
        (2_000_000, 1_000_000),
    ] {
        let error = err_of(TipPolicy::new(fallback, cap))?;
        assert!(
            matches!(error, LiveError::InvalidTipPolicy { .. }),
            "{fallback}/{cap}: {error:?}"
        );
    }
    let base = policy()?;
    assert!(base.with_spike_multiple_bps(9_999).is_err());
    assert!(base.with_spike_multiple_bps(10_000).is_ok());
    assert!(base.with_max_share_bps(0).is_err());
    assert!(base.with_max_share_bps(10_001).is_err());
    assert!(base.with_max_share_bps(10_000).is_ok());
    Ok(())
}

#[test]
fn without_a_floor_the_configured_tip_is_used() -> TestResult {
    let quote = policy()?.quote(None, Urgency::Launch, 0);
    assert_eq!((quote.lamports, quote.basis), (10_000, TipBasis::Fallback));
    Ok(())
}

#[test]
fn a_stale_floor_is_not_trusted() -> TestResult {
    let policy = policy()?;
    let age = DEFAULT_MAX_FLOOR_AGE + Duration::from_secs(1);
    let stale = policy.quote(Some((&floor(), age)), Urgency::Launch, 0);
    assert_eq!(stale.basis, TipBasis::Fallback);
    assert_eq!(stale.lamports, 10_000);
    let edge = policy.quote(Some((&floor(), DEFAULT_MAX_FLOOR_AGE)), Urgency::Launch, 0);
    assert_ne!(
        edge.basis,
        TipBasis::Fallback,
        "exactly the maximum age is still fresh"
    );
    Ok(())
}

#[test]
fn launch_bids_high_and_exit_bids_moderately() -> TestResult {
    // The spike ceiling (3 x 40_000 = 120_000) is lifted for this test.
    let policy = policy()?.with_spike_multiple_bps(1_000_000)?;
    let launch = policy.quote(Some((&floor(), FRESH)), Urgency::Launch, 0);
    let exit = policy.quote(Some((&floor(), FRESH)), Urgency::Exit, 0);
    assert_eq!((launch.lamports, launch.basis), (200_000, TipBasis::Market));
    assert_eq!((exit.lamports, exit.basis), (50_000, TipBasis::Market));
    assert!(launch.lamports > exit.lamports);

    let custom = policy.with_percentiles(Percentile::P99, Percentile::P50);
    assert_eq!(
        custom
            .quote(Some((&floor(), FRESH)), Urgency::Launch, 0)
            .lamports,
        900_000
    );
    assert_eq!(
        custom
            .quote(Some((&floor(), FRESH)), Urgency::Exit, 0)
            .lamports,
        20_000
    );
    Ok(())
}

#[test]
fn fee_spikes_are_damped_to_a_multiple_of_the_calm_baseline() -> TestResult {
    let policy = policy()?;
    // p95 = 200_000 but 3 x the EMA median (40_000) = 120_000.
    let quote = policy.quote(Some((&floor(), FRESH)), Urgency::Launch, 0);
    assert_eq!(
        (quote.lamports, quote.basis),
        (120_000, TipBasis::SpikeDamped)
    );

    // A raging market does not drag the bid along.
    let raging = TipFloor {
        p95: 5_000_000,
        p99: 9_000_000,
        ..floor()
    };
    let quote = policy.quote(Some((&raging, FRESH)), Urgency::Launch, 0);
    assert_eq!(quote.lamports, 120_000);

    // The ceiling is never below the operator's own baseline.
    let calm = TipFloor {
        p95: 80_000,
        ema_p50: 1_000,
        ..floor()
    };
    let wealthy = TipPolicy::new(50_000, 1_000_000)?;
    let quote = wealthy.quote(Some((&calm, FRESH)), Urgency::Launch, 0);
    assert_eq!(quote.lamports, 50_000, "ceiling = max(3 x 1000, fallback)");
    Ok(())
}

#[test]
fn a_zero_ema_disables_damping_only() -> TestResult {
    let no_ema = TipFloor {
        ema_p50: 0,
        ..floor()
    };
    let quote = policy()?.quote(Some((&no_ema, FRESH)), Urgency::Launch, 0);
    assert_eq!((quote.lamports, quote.basis), (200_000, TipBasis::Market));
    Ok(())
}

#[test]
fn small_trades_never_overpay() -> TestResult {
    let policy = policy()?.with_spike_multiple_bps(1_000_000)?;
    // 5 % of 1_000_000 lamports = 50_000, below the 200_000 market bid.
    let quote = policy.quote(Some((&floor(), FRESH)), Urgency::Launch, 1_000_000);
    assert_eq!(
        (quote.lamports, quote.basis),
        (50_000, TipBasis::NotionalCapped)
    );
    // A tiny trade still pays the Jito minimum, not less.
    let tiny = policy.quote(Some((&floor(), FRESH)), Urgency::Launch, 100);
    assert_eq!(tiny.lamports, MIN_JITO_TIP_LAMPORTS);
    // Zero notional means "unknown": no share cap.
    assert_eq!(
        policy
            .quote(Some((&floor(), FRESH)), Urgency::Launch, 0)
            .lamports,
        200_000
    );
    Ok(())
}

#[test]
fn the_hard_cap_always_wins() -> TestResult {
    let tight = TipPolicy::new(10_000, 60_000)?.with_spike_multiple_bps(1_000_000)?;
    let quote = tight.quote(Some((&floor(), FRESH)), Urgency::Launch, 0);
    assert_eq!(
        (quote.lamports, quote.basis),
        (60_000, TipBasis::HardCapped)
    );

    for urgency in [Urgency::Launch, Urgency::Exit] {
        for notional in [0, 1_000, 1_000_000_000_000] {
            let monstrous = TipFloor {
                p50: u64::MAX,
                p75: u64::MAX,
                p95: u64::MAX,
                p99: u64::MAX,
                ema_p50: u64::MAX,
            };
            let quote = tight.quote(Some((&monstrous, FRESH)), urgency, notional);
            assert!(quote.lamports <= 60_000 && quote.lamports >= MIN_JITO_TIP_LAMPORTS);
        }
    }
    Ok(())
}

#[test]
fn quotes_are_always_within_the_jito_minimum_and_the_cap() -> TestResult {
    let policy = policy()?;
    let weird = [
        TipFloor::default(),
        TipFloor {
            p95: 1,
            p75: 1,
            ..TipFloor::default()
        },
        floor(),
    ];
    for floor in weird {
        for urgency in [Urgency::Launch, Urgency::Exit] {
            for notional in [0, 1, 10_000, 5_000_000_000] {
                let quote = policy.quote(Some((&floor, FRESH)), urgency, notional);
                assert!(
                    (MIN_JITO_TIP_LAMPORTS..=policy.cap_lamports()).contains(&quote.lamports),
                    "{quote:?}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn tip_book_serves_the_fallback_until_a_floor_arrives() -> TestResult {
    let book = TipBook::new(policy()?);
    let before = book.quote(Urgency::Launch, 0)?;
    assert_eq!(before.basis, TipBasis::Fallback);

    book.update(floor())?;
    let after = book.quote(Urgency::Launch, 0)?;
    assert_ne!(after.basis, TipBasis::Fallback);
    assert!(after.lamports > before.lamports);
    Ok(())
}

#[test]
fn tip_book_ignores_an_old_observation() -> TestResult {
    let book = TipBook::new(policy()?);
    let old = Instant::now()
        .checked_sub(DEFAULT_MAX_FLOOR_AGE + Duration::from_secs(5))
        .ok_or("the monotonic clock is too young")?;
    book.update_at(floor(), old)?;
    assert_eq!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);
    book.update(floor())?;
    assert_ne!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);
    Ok(())
}

// =====================================================================================================
// A scripted Block Engine ("mainnet")
// =====================================================================================================

#[derive(Default)]
struct Script {
    sends: VecDeque<Result<BundleId, JitoError>>,
    statuses: VecDeque<Result<InflightStatus, JitoError>>,
}

struct FakeEngine {
    dry_run: bool,
    script: Mutex<Script>,
    /// Answer once the scripted statuses run out.
    default_status: InflightStatus,
    sent: AtomicUsize,
    polls: AtomicUsize,
}

impl FakeEngine {
    fn new(default_status: InflightStatus) -> Self {
        Self {
            dry_run: false,
            script: Mutex::new(Script::default()),
            default_status,
            sent: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
        }
    }

    fn landing() -> Self {
        Self::new(InflightStatus::Landed { slot: 777 })
    }

    fn script_send(&self, result: Result<BundleId, JitoError>) -> Result<(), Box<dyn Error>> {
        self.script
            .lock()
            .map_err(|_| "poisoned")?
            .sends
            .push_back(result);
        Ok(())
    }

    fn script_status(
        &self,
        result: Result<InflightStatus, JitoError>,
    ) -> Result<(), Box<dyn Error>> {
        self.script
            .lock()
            .map_err(|_| "poisoned")?
            .statuses
            .push_back(result);
        Ok(())
    }

    fn sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }

    fn polls(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }
}

fn bundle_id() -> Result<BundleId, JitoError> {
    BundleId::parse(&"ab".repeat(32))
}

impl BlockEngine for FakeEngine {
    fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    async fn send_bundle(
        &self,
        transactions: &[VersionedTransaction],
    ) -> Result<BundleId, JitoError> {
        self.sent.fetch_add(transactions.len(), Ordering::SeqCst);
        let scripted = self
            .script
            .lock()
            .ok()
            .and_then(|mut s| s.sends.pop_front());
        scripted.unwrap_or_else(bundle_id)
    }

    async fn inflight_status(&self, _id: &BundleId) -> Result<InflightStatus, JitoError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let scripted = self
            .script
            .lock()
            .ok()
            .and_then(|mut s| s.statuses.pop_front());
        scripted.unwrap_or(Ok(self.default_status))
    }
}

fn mint_signer() -> MintSigner {
    MintSigner::from_seed([5; 32])
}

fn launch_builder(
    payer: &SecureWallet,
    supply_bps: u16,
    slippage_bps: u16,
) -> Result<LaunchBuilder, Box<dyn Error>> {
    let buy = BuyPlan::from_supply_bps(
        &CurveParams::default(),
        supply_bps,
        DEFAULT_TOTAL_FEE_BPS,
        slippage_bps,
    )?;
    let params = LaunchParams {
        metadata: LaunchMetadata::new(
            "Krypton Live",
            "KLIVE",
            &format!("https://ipfs.io/ipfs/{}", "b".repeat(46)),
        )?,
        variant: CreateVariant::CreateV2,
        options: CreateOptions::default(),
        creator: None,
        buy,
        compute_unit_limit: 300_000,
        priority_fee_micro_lamports: 100_000,
        max_priority_fee_lamports: 5_000_000,
        buyback_fee_recipient: None,
    };
    Ok(LaunchBuilder::new(
        payer.pubkey(),
        mint_signer().pubkey(),
        params,
    )?)
}

const RENT: u64 = 5_000_000;
const FALLBACK_TIP: u64 = 100_000;
const TIP_CAP: u64 = 1_000_000;

fn armed(per_tx: u64, daily: u64) -> Result<ArmedLive, Box<dyn Error>> {
    Ok(ArmedLive {
        limits: GuardLimits::new(per_tx, daily, 500)?,
        tip_policy: TipPolicy::new(FALLBACK_TIP, TIP_CAP)?,
    })
}

/// The worst-case outlay of a 1 % launch with the fallback tip.
fn outlay(builder: &LaunchBuilder) -> Result<u64, Box<dyn Error>> {
    Ok(builder.params().required_balance(RENT, FALLBACK_TIP)?)
}

fn fixed_clock(now: u64) -> Clock {
    Arc::new(move || Ok(now))
}

fn executor(
    engine: Arc<FakeEngine>,
    payer: &SecureWallet,
    armed: &ArmedLive,
) -> Result<LiveExecutor<FakeEngine>, Box<dyn Error>> {
    // `SecureWallet` is deliberately not `Clone`: the executor gets its own instance of
    // the same key, which must be the payer the fixture's builders were created for.
    let own = wallet(7)?;
    if own.pubkey() != payer.pubkey() {
        return Err("the fixture payer is not the seed-7 wallet".into());
    }
    Ok(LiveExecutor::with_engine(engine, Arc::new(own), armed)?
        .with_polling(Duration::from_millis(100), 5)
        .with_clock(fixed_clock(1_000)))
}

fn request<'a>(
    builder: &'a LaunchBuilder,
    mint: &'a MintSigner,
    table: &'a LaunchLookupTable,
    available: u64,
) -> LaunchRequest<'a> {
    LaunchRequest {
        builder,
        mint,
        table,
        available_lamports: available,
        create_rent_lamports: RENT,
        recent_blockhash: Hash::new_from_array([4; 32]),
    }
}

struct Fixture {
    payer: SecureWallet,
    builder: LaunchBuilder,
    mint: MintSigner,
    table: LaunchLookupTable,
}

fn fixture() -> Result<Fixture, Box<dyn Error>> {
    let payer = wallet(7)?;
    let builder = launch_builder(&payer, 100, 500)?;
    Ok(Fixture {
        payer,
        builder,
        mint: mint_signer(),
        table: LaunchLookupTable::from_static(Pubkey::new_from_array([0xAA; 32]))?,
    })
}

// =====================================================================================================
// Live launch
// =====================================================================================================

#[test]
fn a_dry_run_engine_cannot_become_a_live_executor() -> TestResult {
    let mut engine = FakeEngine::landing();
    engine.dry_run = true;
    let error = err_of(LiveExecutor::with_engine(
        Arc::new(engine),
        Arc::new(wallet(7)?),
        &armed(2_000_000_000, 5_000_000_000)?,
    ))?;
    assert!(matches!(error, LiveError::EngineIsDryRun), "{error:?}");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn launch_lands_and_the_money_is_committed() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    engine.script_status(Ok(InflightStatus::Pending))?;
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;

    let receipt = executor
        .launch(&request(&f.builder, &f.mint, &f.table, 10_000_000_000))
        .await?;
    assert_eq!(receipt.outcome, BundleOutcome::Landed { slot: 777 });
    assert!(receipt.bundle.is_some());
    assert_eq!(
        (receipt.tip_lamports, receipt.tip_basis),
        (FALLBACK_TIP, TipBasis::Fallback)
    );
    assert_eq!(engine.sent(), 1);
    assert_eq!(engine.polls(), 2, "pending, then landed");

    let books = executor.guards().snapshot()?;
    assert_eq!(books.committed_lamports, outlay(&f.builder)?);
    assert_eq!(books.reserved_lamports, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn launch_uses_the_dynamic_tip_when_a_fresh_floor_exists() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    executor.tips().update(floor())?;

    let receipt = executor
        .launch(&request(&f.builder, &f.mint, &f.table, 10_000_000_000))
        .await?;
    // p95 = 200_000 is damped to 3 x the 40_000 EMA = 120_000.
    assert_eq!(
        (receipt.tip_lamports, receipt.tip_basis),
        (120_000, TipBasis::SpikeDamped)
    );
    assert!(receipt.tip_lamports <= TIP_CAP);
    assert_eq!(
        executor.guards().snapshot()?.committed_lamports,
        f.builder.params().required_balance(RENT, 120_000)?
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_failed_bundle_releases_the_money() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::new(InflightStatus::Failed));
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let receipt = executor
        .launch(&request(&f.builder, &f.mint, &f.table, 10_000_000_000))
        .await?;
    assert_eq!(receipt.outcome, BundleOutcome::Failed);
    let books = executor.guards().snapshot()?;
    assert_eq!((books.committed_lamports, books.reserved_lamports), (0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_undecided_bundle_stays_committed() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::new(InflightStatus::Pending));
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let receipt = executor
        .launch(&request(&f.builder, &f.mint, &f.table, 10_000_000_000))
        .await?;
    assert_eq!(receipt.outcome, BundleOutcome::Unknown);
    assert_eq!(engine.polls(), 5, "the polling budget was used up");
    assert_eq!(
        executor.guards().snapshot()?.committed_lamports,
        outlay(&f.builder)?,
        "it may still land: the money counts as spent"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_ambiguous_send_is_unknown_not_an_error() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    engine.script_send(Err(JitoError::Timeout))?;
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let receipt = executor
        .launch(&request(&f.builder, &f.mint, &f.table, 10_000_000_000))
        .await?;
    assert_eq!(receipt.outcome, BundleOutcome::Unknown);
    assert!(receipt.bundle.is_none());
    assert_eq!(engine.polls(), 0, "there is no id to poll");
    assert_eq!(
        executor.guards().snapshot()?.committed_lamports,
        outlay(&f.builder)?
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_definite_send_error_releases_the_money() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    engine.script_send(Err(JitoError::Rpc {
        code: -32602,
        message: "bundle rejected".to_owned(),
    }))?;
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let error = err_of(
        executor
            .launch(&request(&f.builder, &f.mint, &f.table, 10_000_000_000))
            .await,
    )?;
    assert!(
        matches!(error, LiveError::Jito(JitoError::Rpc { .. })),
        "{error:?}"
    );
    let books = executor.guards().snapshot()?;
    assert_eq!((books.committed_lamports, books.reserved_lamports), (0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_per_transaction_cap_stops_a_launch_before_it_is_built() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let cap = outlay(&f.builder)? - 1;
    let executor = executor(Arc::clone(&engine), &f.payer, &armed(cap, cap * 4)?)?;
    let error = err_of(
        executor
            .launch(&request(&f.builder, &f.mint, &f.table, 10_000_000_000))
            .await,
    )?;
    assert!(
        matches!(error, LiveError::Guard(GuardError::PerTransactionCap)),
        "{error:?}"
    );
    assert_eq!(engine.sent(), 0);
    assert_eq!(executor.guards().snapshot()?.reserved_lamports, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_slippage_ceiling_stops_a_wide_launch() -> TestResult {
    let payer = wallet(7)?;
    let wide = launch_builder(&payer, 100, 500)?;
    let engine = Arc::new(FakeEngine::landing());
    let strict = ArmedLive {
        limits: GuardLimits::new(2_000_000_000, 5_000_000_000, 300)?,
        tip_policy: TipPolicy::new(FALLBACK_TIP, TIP_CAP)?,
    };
    let executor = executor(Arc::clone(&engine), &payer, &strict)?;
    let table = LaunchLookupTable::from_static(Pubkey::new_from_array([0xAA; 32]))?;
    let mint = mint_signer();
    let error = err_of(
        executor
            .launch(&request(&wide, &mint, &table, 10_000_000_000))
            .await,
    )?;
    assert!(
        matches!(error, LiveError::Guard(GuardError::SlippageTooHigh)),
        "{error:?}"
    );
    assert_eq!(engine.sent(), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_underfunded_launch_is_released_and_never_sent() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let error = err_of(
        executor
            .launch(&request(&f.builder, &f.mint, &f.table, 1_000))
            .await,
    )?;
    assert!(matches!(error, LiveError::Builder(_)), "{error:?}");
    assert_eq!(engine.sent(), 0);
    let books = executor.guards().snapshot()?;
    assert_eq!((books.committed_lamports, books.reserved_lamports), (0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_drawdown_tripwire_blocks_the_next_launch() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let cap = outlay(&f.builder)?;
    let executor = executor(Arc::clone(&engine), &f.payer, &armed(cap, cap)?)?;
    let req = request(&f.builder, &f.mint, &f.table, 10_000_000_000);

    let first = executor.launch(&req).await?;
    assert_eq!(first.outcome, BundleOutcome::Landed { slot: 777 });
    assert!(
        executor.guards().is_halted()?,
        "the first launch used the whole daily allowance"
    );

    let error = err_of(executor.launch(&req).await)?;
    assert!(
        matches!(error, LiveError::Guard(GuardError::Halted)),
        "{error:?}"
    );
    assert_eq!(
        engine.sent(),
        1,
        "the second launch never reached the engine"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_daily_limit_blocks_a_launch_that_would_pass_it() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let cap = outlay(&f.builder)?;
    let executor = executor(Arc::clone(&engine), &f.payer, &armed(cap, cap + cap / 2)?)?;
    let req = request(&f.builder, &f.mint, &f.table, 10_000_000_000);
    executor.launch(&req).await?;
    let error = err_of(executor.launch(&req).await)?;
    assert!(
        matches!(error, LiveError::Guard(GuardError::DailyDrawdown)),
        "{error:?}"
    );
    assert!(
        !executor.guards().is_halted()?,
        "the limit blocked it before the tripwire"
    );
    Ok(())
}

// =====================================================================================================
// Live exit orders
// =====================================================================================================

fn sell_builder(payer: &SecureWallet) -> Result<SellBuilder, Box<dyn Error>> {
    Ok(SellBuilder::new(
        payer.pubkey(),
        mint_signer().pubkey(),
        CreateVariant::CreateV2,
        SellSettings {
            compute_unit_limit: 200_000,
            priority_fee_micro_lamports: 100_000,
            max_priority_fee_lamports: 5_000_000,
            buyback_fee_recipient: None,
        },
    )?)
}

fn sell_order(sequence: u8) -> Result<SellOrder, Box<dyn Error>> {
    let buy = BuyPlan::from_supply_bps(&CurveParams::default(), 100, DEFAULT_TOTAL_FEE_BPS, 0)?;
    let curve = CurveParams::default();
    let plan =
        SellPlan::from_token_amount(&curve, buy.token_amount / 2, DEFAULT_TOTAL_FEE_BPS, 500)?;
    Ok(SellOrder {
        sequence,
        trigger: ExitTrigger::Tranche {
            index: usize::from(sequence),
        },
        plan,
        issued_at_ms: 1_000,
        curve_slot: 1,
        cost_lamports: 200_000,
    })
}

#[tokio::test(start_paused = true)]
async fn a_landed_sell_commits_its_fees_and_records_the_proceeds() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let (_tx, blockhash) = blockhash_channel(Hash::new_from_array([2; 32]));
    let submitter = executor.sell_submitter(sell_builder(&f.payer)?, blockhash);

    let order = sell_order(0)?;
    let outcome = submitter.submit(&order).await?;
    assert_eq!(outcome, SellOutcome::Landed { slot: 777 });
    assert_eq!(engine.sent(), 1);

    let books = executor.guards().snapshot()?;
    assert!(
        books.committed_lamports > 0 && books.committed_lamports < 1_000_000,
        "fees only"
    );
    assert_eq!(books.returned_lamports, order.plan.net_proceeds_lamports);
    assert_eq!(books.reserved_lamports, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn failed_and_unknown_sells_settle_differently() -> TestResult {
    let f = fixture()?;
    let (_tx, blockhash) = blockhash_channel(Hash::new_from_array([2; 32]));

    let failing = Arc::new(FakeEngine::new(InflightStatus::Failed));
    let exec = executor(
        Arc::clone(&failing),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let submitter = exec.sell_submitter(sell_builder(&f.payer)?, blockhash.clone());
    assert_eq!(
        submitter.submit(&sell_order(0)?).await?,
        SellOutcome::Failed
    );
    let books = exec.guards().snapshot()?;
    assert_eq!((books.committed_lamports, books.returned_lamports), (0, 0));

    let pending = Arc::new(FakeEngine::new(InflightStatus::Pending));
    let exec = executor(
        Arc::clone(&pending),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let submitter = exec.sell_submitter(sell_builder(&f.payer)?, blockhash.clone());
    assert_eq!(
        submitter.submit(&sell_order(1)?).await?,
        SellOutcome::Unknown
    );
    let books = exec.guards().snapshot()?;
    assert!(
        books.committed_lamports > 0,
        "fees of a bundle that may land are spent"
    );
    assert_eq!(
        books.returned_lamports, 0,
        "an unknown sell credits nothing"
    );

    let ambiguous = Arc::new(FakeEngine::landing());
    ambiguous.script_send(Err(JitoError::Timeout))?;
    let exec = executor(
        Arc::clone(&ambiguous),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let submitter = exec.sell_submitter(sell_builder(&f.payer)?, blockhash);
    assert_eq!(
        submitter.submit(&sell_order(2)?).await?,
        SellOutcome::Unknown
    );
    assert_eq!(ambiguous.polls(), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_definite_send_error_on_a_sell_is_an_error_and_frees_the_money() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    engine.script_send(Err(JitoError::RateLimited))?;
    let exec = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let (_tx, blockhash) = blockhash_channel(Hash::new_from_array([2; 32]));
    let submitter = exec.sell_submitter(sell_builder(&f.payer)?, blockhash);
    let error = err_of(submitter.submit(&sell_order(0)?).await)?;
    assert!(
        matches!(error, ExitError::Jito(JitoError::RateLimited)),
        "{error:?}"
    );
    let books = exec.guards().snapshot()?;
    assert_eq!((books.committed_lamports, books.reserved_lamports), (0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_stale_blockhash_stops_a_sell_before_anything_is_reserved() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let exec = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let (_tx, blockhash) = blockhash_channel(Hash::new_from_array([2; 32]));
    let submitter = exec.sell_submitter(sell_builder(&f.payer)?, blockhash);
    tokio::time::advance(Duration::from_secs(41)).await;
    let error = err_of(submitter.submit(&sell_order(0)?).await)?;
    assert!(matches!(error, ExitError::StaleBlockhash), "{error:?}");
    assert_eq!(engine.sent(), 0);
    assert_eq!(exec.guards().snapshot()?.reserved_lamports, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn sells_use_the_exit_tip_and_never_exceed_the_cap() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let exec = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let raging = TipFloor {
        p50: 9_000_000,
        p75: 9_000_000,
        p95: 9_000_000,
        p99: 9_000_000,
        ema_p50: 9_000_000,
    };
    exec.tips().update(raging)?;
    let (_tx, blockhash) = blockhash_channel(Hash::new_from_array([2; 32]));
    let submitter = exec.sell_submitter(sell_builder(&f.payer)?, blockhash);
    let order = sell_order(0)?;
    assert_eq!(
        submitter.submit(&order).await?,
        SellOutcome::Landed { slot: 777 }
    );
    // The fee budget is bounded by the cap even in a raging market.
    let books = exec.guards().snapshot()?;
    assert!(
        books.committed_lamports <= 6_000_000 + TIP_CAP,
        "{}",
        books.committed_lamports
    );
    Ok(())
}

// =====================================================================================================
// The live exit pipeline
// =====================================================================================================

#[derive(Clone)]
struct ScriptedTransport {
    updates: Vec<StreamUpdate>,
}

struct ScriptedStream {
    pending: VecDeque<StreamUpdate>,
}

impl StreamTransport for ScriptedTransport {
    type Stream = ScriptedStream;

    async fn connect(
        &mut self,
        _request: &SubscriptionRequest,
    ) -> Result<ScriptedStream, MonitorError> {
        Ok(ScriptedStream {
            pending: self.updates.iter().cloned().collect(),
        })
    }
}

impl EventStream for ScriptedStream {
    async fn next_update(&mut self) -> Result<Option<StreamUpdate>, MonitorError> {
        match self.pending.pop_front() {
            Some(update) => Ok(Some(update)),
            // A quiet market: the connection stays open and the watchdog is fed.
            None => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(Some(StreamUpdate::Heartbeat))
            }
        }
    }
}

#[test]
fn subscription_watches_the_curve_and_the_pump_program() {
    let curve = Pubkey::new_from_array([3; 32]);
    let request = subscription_for(curve);
    assert_eq!(request.accounts, vec![curve]);
    assert!(request.instruction_program.is_some());
    assert_eq!(request.commitment, StreamCommitment::Processed);
}

#[tokio::test(start_paused = true)]
async fn live_exit_sells_the_whole_position_through_the_guards() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;

    // The market after our 1 % buy, as the stream would deliver it.
    let curve = CurveParams::default();
    let buy = BuyPlan::from_supply_bps(&curve, 100, DEFAULT_TOTAL_FEE_BPS, 500)?;
    let mut market = MarketState::new(curve, DEFAULT_TOTAL_FEE_BPS);
    market.apply_creator_buy(&buy)?;
    let curve_account = f.builder.addresses().bonding_curve;
    let update = market.account_update(curve_account);

    let exit_engine = ExitEngine::new(
        ExitPlan::evenly_spaced(2, 10_000, None)?,
        ExitSettings {
            fee_bps: DEFAULT_TOTAL_FEE_BPS,
            slippage_bps: 500,
            order_cost_lamports: 400_000,
            max_total_cost_lamports: 4_000_000,
        },
        Position {
            tokens: buy.token_amount,
            entry: PriceReference::after_buy(&curve, &buy)?,
        },
    )?;
    let (_hash_tx, blockhash) = blockhash_channel(Hash::new_from_array([2; 32]));
    let (_trigger, shutdown) = shutdown_channel();

    let report = executor
        .run_exit(
            ScriptedTransport {
                updates: vec![update],
            },
            ExitRun {
                engine: exit_engine,
                curve_account,
                genesis: tokio::time::Instant::now(),
                builder: sell_builder(&f.payer)?,
                blockhash,
                monitor: MonitorSettings::default(),
                drain_timeout: Duration::from_secs(5),
            },
            shutdown,
        )
        .await?;

    assert_eq!(report.summary.position.orders_issued, 2);
    assert_eq!(report.summary.position.tokens_sold, buy.token_amount);
    assert_eq!(report.summary.position.tokens_unsold, 0);
    assert!(!report.summary.interrupted && !report.summary.unsettled);
    assert_eq!(engine.sent(), 2, "one bundle per tranche");

    let books = executor.guards().snapshot()?;
    assert!(books.returned_lamports > 0);
    assert_eq!(books.reserved_lamports, 0);
    Ok(())
}

// =====================================================================================================
// Source hygiene
// =====================================================================================================

#[test]
fn source_scan_execution_has_no_panics_or_unsafe_code() {
    const SOURCES: [(&str, &str); 5] = [
        ("settings.rs", include_str!("../src/execution/settings.rs")),
        ("gate.rs", include_str!("../src/execution/gate.rs")),
        ("guards.rs", include_str!("../src/execution/guards.rs")),
        ("tip.rs", include_str!("../src/execution/tip.rs")),
        ("live.rs", include_str!("../src/execution/live.rs")),
    ];
    const FORBIDDEN: [&str; 7] = [
        ".unwrap()",
        ".expect(",
        "panic!",
        "todo!",
        "unimplemented!",
        "unsafe",
        "println!",
    ];
    for (name, source) in SOURCES {
        for word in FORBIDDEN {
            assert!(!source.contains(word), "{name} contains `{word}`");
        }
    }
}

#[test]
fn tip_accounts_are_available_for_the_live_executor() {
    assert_eq!(JITO_TIP_ACCOUNTS.len(), 8);
}
