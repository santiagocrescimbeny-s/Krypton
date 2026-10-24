//! Production-bridge test-suite: the command line, the dry-to-live barrier, the
//! keypair preflight, the production settings, the blockhash refresher, the
//! background services and a whole dry exit session.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - Nothing here touches the network: sources and the stream are scripted.
//! - Time-dependent tests run on Tokio's paused clock.
//! - What needs the real configuration loader, `reqwest` or the process
//!   environment lives in `bridge_config_tests.rs`.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use krypton::app::{
    decide_mode, preflight_keypair, run_blockhash_refresher, summarize, BlockhashFeed,
    BlockhashSource, Cli, Command, ExitArgs, LiveRequest, Mode, RefresherStats, Services, TipFeed,
    DEFAULT_SELL_BPS, DEFAULT_TRANCHES,
};
use krypton::builder::math::{BuyPlan, CurveParams, DEFAULT_TOTAL_FEE_BPS};
use krypton::builder::pda::CreateVariant;
use krypton::config::Config;
use krypton::errors::{
    BridgeError, CliError, ConfigError, ExitError, LauncherError, LiveError, MonitorError,
    TipFeedError,
};
use krypton::execution::live::{run_exit_pipeline, ExitPipeline, ExitReport};
use krypton::execution::settings::{LiveSettings, LIVE_CONFIRM_PHRASE};
use krypton::execution::tip::{TipBasis, TipBook, TipFloor, TipPolicy, Urgency};
use krypton::execution::tip_feed::{TipFeedSettings, TipSource};
use krypton::monitor::curve::PriceReference;
use krypton::monitor::exit::{
    BalanceOutcome, EngineSnapshot, ExitSummary, SellOrder, SellOutcome, SellSubmitter,
};
use krypton::monitor::geyser::{
    EventStream, HealthStatus, MonitorSettings, StreamTransport, StreamUpdate, SubscriptionRequest,
};
use krypton::monitor::shutdown_channel;
use krypton::monitor::submit::blockhash_channel;
use krypton::production::{
    merge_alias, ProductionSettings, RawProductionSettings, ENV_JITO_TIP_STREAM_URL,
    ENV_YELLOWSTONE_ENDPOINT, ENV_YELLOWSTONE_X_TOKEN,
};
use krypton::runner::{check, run_exit_command, GRPC_COMPILED};
use krypton::security::{SecretString, SecureWallet};
use krypton::simulation::market::MarketState;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;

type TestResult = Result<(), Box<dyn Error>>;

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn key(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte; 32])
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

fn scratch(tag: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("krypton-bridge-{}-{tag}", std::process::id()));
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

fn write_keypair(dir: &Path, name: &str, seed: u8, mode: u32) -> Result<PathBuf, Box<dyn Error>> {
    let path = dir.join(name);
    std::fs::write(&path, keypair_json(seed))?;
    set_mode(&path, mode)?;
    Ok(path)
}

fn mint_text() -> String {
    key(9).to_string()
}

fn exit_argv(extra: &[&str]) -> Vec<String> {
    let mint = mint_text();
    let mut argv = args(&[
        "exit",
        "--mint",
        &mint,
        "--tokens",
        "1000000",
        "--entry-sol-reserves",
        "32000000000",
        "--entry-token-reserves",
        "1073000000000000",
    ]);
    argv.extend(extra.iter().map(|item| (*item).to_owned()));
    argv
}

fn exit_args() -> ExitArgs {
    ExitArgs {
        mint: key(9),
        tokens: 1_000_000,
        entry_virtual_sol_reserves: 32_000_000_000,
        entry_virtual_token_reserves: 1_073_000_000_000_000,
        tranches: DEFAULT_TRANCHES,
        sell_bps: DEFAULT_SELL_BPS,
        stop_loss_bps: None,
        max_total_cost_lamports: None,
        legacy_create: false,
        elapsed_since_launch_ms: 0,
    }
}

// =====================================================================================================
// Command line
// =====================================================================================================

#[test]
fn the_exit_command_parses_with_its_defaults() -> TestResult {
    let cli = Cli::parse(exit_argv(&[]))?;
    assert_eq!(cli.live, LiveRequest::default());
    let Command::Exit(parsed) = cli.command else {
        return Err("expected the exit command".into());
    };
    assert_eq!(parsed, exit_args());
    assert_eq!(parsed.variant(), CreateVariant::CreateV2);
    Ok(())
}

#[test]
fn every_exit_option_is_read() -> TestResult {
    let cli = Cli::parse(exit_argv(&[
        "--tranches=6",
        "--sell-bps",
        "9000",
        "--stop-loss-bps",
        "2500",
        "--max-total-cost=3000000",
        "--legacy-create",
    ]))?;
    let Command::Exit(parsed) = cli.command else {
        return Err("expected the exit command".into());
    };
    assert_eq!(parsed.tranches, 6);
    assert_eq!(parsed.sell_bps, 9_000);
    assert_eq!(parsed.stop_loss_bps, Some(2_500));
    assert_eq!(parsed.max_total_cost_lamports, Some(3_000_000));
    assert_eq!(parsed.variant(), CreateVariant::LegacyCreate);
    Ok(())
}

#[test]
fn flags_may_come_before_the_command() -> TestResult {
    let mut argv = args(&["--live", "--confirm-live", LIVE_CONFIRM_PHRASE]);
    argv.extend(exit_argv(&[]));
    let cli = Cli::parse(argv)?;
    assert!(cli.live.requested);
    assert_eq!(cli.live.confirmation.as_deref(), Some(LIVE_CONFIRM_PHRASE));
    assert!(matches!(cli.command, Command::Exit(_)));
    Ok(())
}

#[test]
fn help_and_check_parse() -> TestResult {
    for argv in [
        args(&["help"]),
        args(&["--help"]),
        args(&["-h"]),
        args(&["check", "--help"]),
    ] {
        assert_eq!(Cli::parse(argv)?.command, Command::Help);
    }
    assert_eq!(Cli::parse(args(&["check"]))?.command, Command::Check);
    let checked = Cli::parse(args(&["check", "--live", "--confirm-live=x"]))?;
    assert_eq!(checked.command, Command::Check);
    assert!(checked.live.requested);
    Ok(())
}

#[test]
fn malformed_command_lines_are_rejected() -> TestResult {
    assert_eq!(err_of(Cli::parse(args(&[])))?, CliError::NoCommand);
    assert_eq!(err_of(Cli::parse(args(&["--live"])))?, CliError::NoCommand);
    assert_eq!(
        err_of(Cli::parse(args(&["launch"])))?,
        CliError::UnknownCommand
    );
    assert_eq!(
        err_of(Cli::parse(args(&["check", "exit"])))?,
        CliError::ExtraCommand
    );
    assert_eq!(
        err_of(Cli::parse(args(&["check", "--nonsense"])))?,
        CliError::UnknownArgument
    );
    assert_eq!(
        err_of(Cli::parse(args(&["check", "-x"])))?,
        CliError::UnknownArgument
    );
    assert_eq!(
        err_of(Cli::parse(args(&["check", "--"])))?,
        CliError::UnknownArgument
    );
    // `check` takes no exit options.
    assert_eq!(
        err_of(Cli::parse(args(&["check", "--tokens", "5"])))?,
        CliError::UnknownArgument
    );
    assert_eq!(
        err_of(Cli::parse(args(&["check", "--legacy-create"])))?,
        CliError::UnknownArgument
    );
    Ok(())
}

#[test]
fn flag_values_are_checked() -> TestResult {
    // A value-less flag, also when the next token is another flag.
    assert_eq!(
        err_of(Cli::parse(exit_argv(&["--tranches"])))?,
        CliError::MissingValue { flag: "--tranches" }
    );
    assert_eq!(
        err_of(Cli::parse(exit_argv(&["--tranches", "--sell-bps", "5"])))?,
        CliError::MissingValue { flag: "--tranches" }
    );
    // Twice.
    assert_eq!(
        err_of(Cli::parse(exit_argv(&["--tranches=2", "--tranches=3"])))?,
        CliError::DuplicateFlag { flag: "--tranches" }
    );
    assert_eq!(
        err_of(Cli::parse(args(&["check", "--live", "--live"])))?,
        CliError::DuplicateFlag { flag: "--live" }
    );
    // A switch that takes no value.
    assert!(matches!(
        err_of(Cli::parse(args(&["check", "--live=yes"])))?,
        CliError::InvalidValue { flag: "--live", .. }
    ));
    // Missing mandatory flags.
    assert_eq!(
        err_of(Cli::parse(args(&["exit"])))?,
        CliError::MissingFlag { flag: "--mint" }
    );
    let mint = mint_text();
    assert_eq!(
        err_of(Cli::parse(args(&["exit", "--mint", &mint])))?,
        CliError::MissingFlag { flag: "--tokens" }
    );
    // Bad numbers and keys.
    for (flag, bad) in [
        ("--tokens", "0"),
        ("--tokens", "-1"),
        ("--tokens", "1.5"),
        ("--tokens", "lots"),
        ("--entry-sol-reserves", "0"),
        ("--entry-token-reserves", "0"),
        ("--tranches", "256"),
        ("--sell-bps", "65536"),
        ("--stop-loss-bps", "x"),
        ("--max-total-cost", ""),
    ] {
        let mut argv = exit_argv(&[]);
        // The later occurrence of a mandatory flag would be a duplicate: edit in place.
        let pair = argv.iter().position(|item| item == flag);
        match pair {
            Some(index) => {
                let slot = argv.get_mut(index + 1).ok_or("flag without value")?;
                bad.clone_into(slot);
            }
            None => {
                argv.push(format!("{flag}={bad}"));
            }
        }
        assert!(
            matches!(err_of(Cli::parse(argv))?, CliError::InvalidValue { .. }),
            "{flag}={bad} must be rejected"
        );
    }
    let mut argv = exit_argv(&[]);
    let index = argv
        .iter()
        .position(|item| item == "--mint")
        .ok_or("no --mint")?;
    argv.get_mut(index + 1)
        .ok_or("no mint value")?
        .push_str("not-a-key");
    assert!(matches!(
        err_of(Cli::parse(argv))?,
        CliError::InvalidValue { flag: "--mint", .. }
    ));
    Ok(())
}

#[test]
fn the_confirmation_needs_live() -> TestResult {
    assert!(matches!(
        err_of(Cli::parse(exit_argv(&["--confirm-live", "x"])))?,
        CliError::InvalidValue {
            flag: "--confirm-live",
            ..
        }
    ));
    Ok(())
}

#[test]
fn cli_errors_never_echo_the_arguments() -> TestResult {
    let secret = "SuperSecretValue123";
    let mut texts = Vec::new();
    texts.push(err_of(Cli::parse(args(&[secret])))?.to_string());
    texts.push(err_of(Cli::parse(args(&["check", &format!("--{secret}")])))?.to_string());
    texts.push(err_of(Cli::parse(args(&["check", &format!("--{secret}=1")])))?.to_string());
    texts.push(err_of(Cli::parse(args(&["exit", "--mint", secret])))?.to_string());
    texts.push(err_of(Cli::parse(args(&["check", secret, secret])))?.to_string());
    let mut argv = exit_argv(&[]);
    argv.push(format!("--tokens={secret}"));
    texts.push(err_of(Cli::parse(argv))?.to_string());
    for text in texts {
        assert!(!text.contains(secret), "leaked: {text}");
    }
    Ok(())
}

// =====================================================================================================
// The dry-to-live barrier
// =====================================================================================================

fn armed_settings(path: Option<PathBuf>) -> LiveSettings {
    LiveSettings {
        live_trading: true,
        confirmed: true,
        keypair_path: path,
        guard_state_path: None,
        max_spend_per_tx_lamports: Some(1_000_000_000),
        daily_drawdown_lamports: Some(2_000_000_000),
    }
}

fn live_request(confirmation: Option<&str>) -> LiveRequest {
    LiveRequest {
        requested: true,
        confirmation: confirmation.map(str::to_owned),
    }
}

#[test]
fn without_the_flag_everything_runs_dry() -> TestResult {
    let armed = armed_settings(Some(PathBuf::from("/secure/key.json")));
    // Even a fully armed environment stays dry when `--live` is absent...
    assert_eq!(
        decide_mode(&LiveRequest::default(), false, &armed)?,
        Mode::Dry
    );
    // ...and a stray confirmation phrase does not count as the flag.
    let phrase_only = LiveRequest {
        requested: false,
        confirmation: Some(LIVE_CONFIRM_PHRASE.to_owned()),
    };
    assert_eq!(decide_mode(&phrase_only, false, &armed)?, Mode::Dry);
    assert_eq!(
        decide_mode(&LiveRequest::default(), true, &LiveSettings::default())?,
        Mode::Dry
    );
    Ok(())
}

#[test]
fn live_needs_the_exact_phrase_on_the_command_line() -> TestResult {
    let armed = armed_settings(Some(PathBuf::from("/secure/key.json")));
    for confirmation in [
        None,
        Some(""),
        Some("yes"),
        Some("i-understand-real-funds-will-be-spent"),
        Some(" I-UNDERSTAND-REAL-FUNDS-WILL-BE-SPENT"),
        Some("I-UNDERSTAND-REAL-FUNDS-WILL-BE-SPENT "),
    ] {
        let error = err_of(decide_mode(&live_request(confirmation), false, &armed))?;
        assert!(
            matches!(
                error,
                LauncherError::Bridge(BridgeError::LiveConfirmationMissing)
            ),
            "{confirmation:?} must be refused"
        );
    }
    Ok(())
}

#[test]
fn live_needs_every_environment_switch() -> TestResult {
    let request = live_request(Some(LIVE_CONFIRM_PHRASE));
    let path = Some(PathBuf::from("/secure/key.json"));

    let mut settings = armed_settings(path.clone());
    settings.live_trading = false;
    assert!(matches!(
        err_of(decide_mode(&request, false, &settings))?,
        LauncherError::Live(LiveError::NotArmed)
    ));

    let settings = armed_settings(path.clone());
    assert!(matches!(
        err_of(decide_mode(&request, true, &settings))?,
        LauncherError::Live(LiveError::DryRunEnabled)
    ));

    let mut settings = armed_settings(path);
    settings.confirmed = false;
    assert!(matches!(
        err_of(decide_mode(&request, false, &settings))?,
        LauncherError::Live(LiveError::ConfirmationMissing)
    ));

    let settings = armed_settings(None);
    assert!(matches!(
        err_of(decide_mode(&request, false, &settings))?,
        LauncherError::Live(LiveError::KeypairPathMissing)
    ));
    Ok(())
}

#[test]
fn live_is_decided_only_when_everything_agrees() -> TestResult {
    let settings = armed_settings(Some(PathBuf::from("/secure/key.json")));
    let request = live_request(Some(LIVE_CONFIRM_PHRASE));
    assert_eq!(decide_mode(&request, false, &settings)?, Mode::Live);
    Ok(())
}

// =====================================================================================================
// The keypair preflight
// =====================================================================================================

#[test]
fn no_configured_path_means_nothing_to_check() -> TestResult {
    assert!(!preflight_keypair(&LiveSettings::default(), &wallet(7)?)?);
    Ok(())
}

#[test]
fn an_owner_only_keypair_of_the_configured_wallet_passes() -> TestResult {
    let dir = scratch("ok")?;
    let path = write_keypair(&dir, "payer.json", 7, 0o600)?;
    assert!(preflight_keypair(&armed_settings(Some(path)), &wallet(7)?)?);
    Ok(())
}

#[test]
fn another_wallets_keypair_is_refused() -> TestResult {
    let dir = scratch("mismatch")?;
    let path = write_keypair(&dir, "payer.json", 8, 0o600)?;
    let error = err_of(preflight_keypair(&armed_settings(Some(path)), &wallet(7)?))?;
    assert!(matches!(
        error,
        LauncherError::Live(LiveError::KeypairMismatch)
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_keypair_others_can_read_is_refused() -> TestResult {
    let dir = scratch("modes")?;
    for mode in [0o644, 0o640, 0o604, 0o660, 0o666, 0o700 | 0o070] {
        let path = write_keypair(&dir, &format!("payer-{mode:o}.json"), 7, mode)?;
        let error = err_of(preflight_keypair(&armed_settings(Some(path)), &wallet(7)?))?;
        assert!(
            matches!(error, LauncherError::Live(LiveError::KeypairFile { .. })),
            "mode {mode:o} must be refused"
        );
    }
    // 0400 (read-only for the owner) is fine.
    let path = write_keypair(&dir, "payer-400.json", 7, 0o400)?;
    assert!(preflight_keypair(&armed_settings(Some(path)), &wallet(7)?)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symlinked_keypair_is_refused() -> TestResult {
    let dir = scratch("symlink")?;
    let target = write_keypair(&dir, "real.json", 7, 0o600)?;
    let link = dir.join("link.json");
    std::os::unix::fs::symlink(&target, &link)?;
    let error = err_of(preflight_keypair(&armed_settings(Some(link)), &wallet(7)?))?;
    assert!(matches!(
        error,
        LauncherError::Live(LiveError::KeypairFile { .. })
    ));
    Ok(())
}

#[test]
fn missing_empty_and_garbage_keypair_files_are_refused() -> TestResult {
    let dir = scratch("bad")?;
    let missing = dir.join("absent.json");
    let empty = dir.join("empty.json");
    std::fs::write(&empty, "")?;
    set_mode(&empty, 0o600)?;
    let garbage = dir.join("garbage.json");
    std::fs::write(&garbage, "this is not a keypair")?;
    set_mode(&garbage, 0o600)?;
    for path in [missing, empty, garbage, dir.clone()] {
        let error = err_of(preflight_keypair(&armed_settings(Some(path)), &wallet(7)?))?;
        assert!(matches!(
            error,
            LauncherError::Live(LiveError::KeypairFile { .. })
        ));
    }
    Ok(())
}

#[test]
fn keypair_errors_do_not_reveal_the_path_or_the_contents() -> TestResult {
    let dir = scratch("leak")?;
    let garbage = dir.join("very-recognizable-name.json");
    std::fs::write(&garbage, "TOP-SECRET-CONTENT")?;
    set_mode(&garbage, 0o600)?;
    let error = err_of(preflight_keypair(
        &armed_settings(Some(garbage)),
        &wallet(7)?,
    ))?;
    let text = format!("{error} {error:?}");
    assert!(!text.contains("very-recognizable-name"));
    assert!(!text.contains("TOP-SECRET-CONTENT"));
    Ok(())
}

// =====================================================================================================
// The exit engine
// =====================================================================================================

#[test]
fn the_default_plan_builds_an_engine() -> TestResult {
    let engine = exit_args().engine(500, 400_000)?;
    assert_eq!(engine.snapshot().tokens_unsold, 1_000_000);
    assert_eq!(engine.snapshot().orders_issued, 0);
    Ok(())
}

#[test]
fn an_invalid_plan_is_rejected_by_the_engine() -> TestResult {
    let mut args = exit_args();
    args.tranches = 0;
    assert!(matches!(
        err_of(args.engine(500, 400_000))?,
        ExitError::InvalidPlan { .. }
    ));
    let mut args = exit_args();
    args.sell_bps = 10_001;
    assert!(matches!(
        err_of(args.engine(500, 400_000))?,
        ExitError::InvalidPlan { .. }
    ));
    // Slippage above the 10 % cap is the engine's call, not this layer's.
    assert!(matches!(
        err_of(exit_args().engine(1_001, 400_000))?,
        ExitError::InvalidConfig { .. }
    ));
    Ok(())
}

// =====================================================================================================
// Production settings
// =====================================================================================================

fn raw(endpoint: Option<&str>, token: Option<&str>, tip: Option<&str>) -> RawProductionSettings {
    RawProductionSettings {
        yellowstone_endpoint: endpoint.map(str::to_owned),
        yellowstone_x_token: token.map(|text| SecretString::new(text.to_owned())),
        jito_tip_stream_url: tip.map(str::to_owned),
    }
}

#[test]
fn aliases_agree_or_conflict() -> TestResult {
    let some = |text: &str| Some(text.to_owned());
    assert_eq!(merge_alias("A", "B", None, None)?, None);
    assert_eq!(merge_alias("A", "B", some("x"), None)?, some("x"));
    assert_eq!(merge_alias("A", "B", None, some("y"))?, some("y"));
    assert_eq!(merge_alias("A", "B", some("x"), some(" x "))?, some("x"));
    // Blank counts as unset.
    assert_eq!(merge_alias("A", "B", some("  "), some("y"))?, some("y"));
    let error = err_of(merge_alias(
        "A",
        "B",
        some("secret-one"),
        some("secret-two"),
    ))?;
    assert!(matches!(error, ConfigError::InvalidVar { name: "B", .. }));
    let text = error.to_string();
    assert!(!text.contains("secret-one") && !text.contains("secret-two"));
    Ok(())
}

#[test]
fn production_settings_are_optional() -> TestResult {
    let settings = ProductionSettings::from_raw(raw(None, None, None))?;
    assert!(settings.yellowstone.is_none());
    assert!(settings.tip_feed_url.is_none());
    // Blank values count as unset.
    let settings = ProductionSettings::from_raw(raw(Some("  "), Some(" "), Some("")))?;
    assert!(settings.yellowstone.is_none() && settings.tip_feed_url.is_none());
    Ok(())
}

#[test]
fn a_full_production_setup_is_accepted() -> TestResult {
    let settings = ProductionSettings::from_raw(raw(
        Some("https://grpc.example.com:443"),
        Some("abc123token"),
        Some("https://bundles.jito.wtf/api/v1/bundles/tip_floor"),
    ))?;
    let yellowstone = settings.yellowstone.as_ref().ok_or("no yellowstone")?;
    assert_eq!(yellowstone.origin(), "https://grpc.example.com:443");
    assert!(yellowstone.x_token().is_some());
    assert_eq!(
        settings.tip_feed_url.as_deref(),
        Some("https://bundles.jito.wtf/api/v1/bundles/tip_floor")
    );
    // A loopback test endpoint without a token is fine too.
    let local = ProductionSettings::from_raw(raw(Some("http://127.0.0.1:10000"), None, None))?;
    assert!(local.yellowstone.is_some());
    Ok(())
}

#[test]
fn bad_production_values_are_rejected_without_being_echoed() -> TestResult {
    let secret = "supersecretvalue";
    let cases = [
        (
            raw(None, Some(secret), None),
            ENV_YELLOWSTONE_X_TOKEN,
            "token without endpoint",
        ),
        (
            raw(Some(&format!("http://{secret}.example.com")), None, None),
            ENV_YELLOWSTONE_ENDPOINT,
            "plain http to a remote host",
        ),
        (
            raw(
                Some(&format!("https://user:{secret}@example.com")),
                None,
                None,
            ),
            ENV_YELLOWSTONE_ENDPOINT,
            "credentials in the authority",
        ),
        (
            raw(Some("https://grpc.example.com"), Some("bad token"), None),
            ENV_YELLOWSTONE_X_TOKEN,
            "whitespace in the token",
        ),
        (
            raw(
                None,
                None,
                Some(&format!("wss://{secret}.example.com/tips")),
            ),
            ENV_JITO_TIP_STREAM_URL,
            "websocket tip stream",
        ),
        (
            raw(None, None, Some(&format!("WS://{secret}.example.com/tips"))),
            ENV_JITO_TIP_STREAM_URL,
            "upper-case websocket tip stream",
        ),
        (
            raw(
                None,
                None,
                Some(&format!("http://{secret}.example.com/tips")),
            ),
            ENV_JITO_TIP_STREAM_URL,
            "plain http tip feed",
        ),
        (
            raw(None, None, Some("not a url")),
            ENV_JITO_TIP_STREAM_URL,
            "garbage tip feed",
        ),
    ];
    for (input, expected_name, what) in cases {
        let error = err_of(ProductionSettings::from_raw(input))?;
        match &error {
            ConfigError::InvalidVar { name, .. } => {
                assert_eq!(*name, expected_name, "{what}");
            }
            other => return Err(format!("{what}: unexpected error {other:?}").into()),
        }
        let text = format!("{error} {error:?}");
        assert!(!text.contains(secret), "{what} leaked: {text}");
    }
    Ok(())
}

#[test]
fn production_settings_debug_hides_the_secrets() -> TestResult {
    let settings = ProductionSettings::from_raw(raw(
        Some("https://grpc.example.com/very-secret-path"),
        Some("tok3n-value"),
        Some("https://tips.example.com/key-in-path"),
    ))?;
    let text = format!("{settings:?}");
    for secret in ["very-secret-path", "tok3n-value", "key-in-path"] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
    Ok(())
}

// =====================================================================================================
// Blockhash refresher
// =====================================================================================================

enum HashStep {
    Hash(u8),
    Fail,
    Hang,
}

struct ScriptedHashes {
    steps: VecDeque<HashStep>,
    calls: Arc<AtomicUsize>,
}

impl BlockhashSource for ScriptedHashes {
    async fn latest(&mut self) -> Result<Hash, BridgeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.steps.pop_front() {
            Some(HashStep::Hash(byte)) => Ok(Hash::new_from_array([byte; 32])),
            Some(HashStep::Fail) => Err(BridgeError::BlockhashUnavailable),
            Some(HashStep::Hang) | None => {
                std::future::pending::<()>().await;
                Err(BridgeError::BlockhashUnavailable)
            }
        }
    }
}

fn hashes(steps: Vec<HashStep>) -> (ScriptedHashes, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        ScriptedHashes {
            steps: steps.into(),
            calls: Arc::clone(&calls),
        },
        calls,
    )
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn the_refresher_publishes_fresh_stamped_blockhashes() -> TestResult {
    let (source, calls) = hashes(vec![HashStep::Hash(2), HashStep::Hash(3)]);
    let (sender, receiver) = blockhash_channel(Hash::new_from_array([1; 32]));
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_blockhash_refresher(
        source,
        sender,
        Duration::from_secs(2),
        shutdown,
    ));

    let seeded = *receiver.borrow();
    assert_eq!(seeded.0, Hash::new_from_array([1; 32]));

    tokio::time::sleep(Duration::from_millis(2_100)).await;
    settle().await;
    let first = *receiver.borrow();
    assert_eq!(first.0, Hash::new_from_array([2; 32]));
    assert!(
        first.1 >= seeded.1,
        "the stamp must be taken at refresh time"
    );

    tokio::time::sleep(Duration::from_secs(2)).await;
    settle().await;
    assert_eq!(receiver.borrow().0, Hash::new_from_array([3; 32]));

    trigger.trigger();
    let stats = task.await?;
    assert_eq!(
        stats,
        RefresherStats {
            refreshed: 2,
            failures: 0
        }
    );
    assert!(calls.load(Ordering::SeqCst) >= 2);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn failures_and_silence_keep_the_last_blockhash() -> TestResult {
    let (source, _calls) = hashes(vec![HashStep::Fail, HashStep::Hang, HashStep::Hash(9)]);
    let (sender, receiver) = blockhash_channel(Hash::new_from_array([1; 32]));
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_blockhash_refresher(
        source,
        sender,
        Duration::from_secs(2),
        shutdown,
    ));

    // 1st round: the source fails. 2nd round: it hangs until the 3 s deadline.
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    settle().await;
    assert_eq!(receiver.borrow().0, Hash::new_from_array([1; 32]));
    tokio::time::sleep(Duration::from_millis(2_000 + 3_100)).await;
    settle().await;
    assert_eq!(receiver.borrow().0, Hash::new_from_array([1; 32]));

    // 3rd round: it recovers.
    tokio::time::sleep(Duration::from_secs(2)).await;
    settle().await;
    assert_eq!(receiver.borrow().0, Hash::new_from_array([9; 32]));

    trigger.trigger();
    let stats = task.await?;
    assert_eq!(stats.refreshed, 1);
    assert_eq!(stats.failures, 2);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_tiny_interval_is_raised_to_the_minimum() -> TestResult {
    struct Endless(Arc<AtomicUsize>);
    impl BlockhashSource for Endless {
        async fn latest(&mut self) -> Result<Hash, BridgeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Hash::new_from_array([5; 32]))
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let (sender, _receiver) = blockhash_channel(Hash::new_from_array([1; 32]));
    let (trigger, shutdown) = shutdown_channel();
    let counter = Arc::clone(&calls);
    let task = tokio::spawn(run_blockhash_refresher(
        Endless(counter),
        sender,
        Duration::ZERO,
        shutdown,
    ));
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    trigger.trigger();
    task.await?;
    // 250 ms floor: at most 4 fetches in one second, never a busy loop.
    let made = calls.load(Ordering::SeqCst);
    assert!((3..=5).contains(&made), "made {made} fetches");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_a_hanging_fetch() -> TestResult {
    let (source, _calls) = hashes(vec![HashStep::Hang]);
    let (sender, _receiver) = blockhash_channel(Hash::new_from_array([1; 32]));
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_blockhash_refresher(
        source,
        sender,
        Duration::from_secs(2),
        shutdown,
    ));
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    settle().await;
    trigger.trigger();
    let stats = task.await?;
    assert_eq!(stats, RefresherStats::default());
    Ok(())
}

// =====================================================================================================
// Services
// =====================================================================================================

struct ScriptedTips {
    floors: VecDeque<Result<TipFloor, TipFeedError>>,
}

impl TipSource for ScriptedTips {
    async fn fetch(&mut self) -> Result<TipFloor, TipFeedError> {
        match self.floors.pop_front() {
            Some(result) => result,
            None => {
                std::future::pending::<()>().await;
                Err(TipFeedError::Timeout)
            }
        }
    }
}

fn market_floor() -> TipFloor {
    TipFloor {
        p50: 20_000,
        p75: 50_000,
        p95: 200_000,
        p99: 900_000,
        ema_p50: 40_000,
    }
}

fn tip_book() -> Result<Arc<TipBook>, Box<dyn Error>> {
    Ok(Arc::new(TipBook::new(TipPolicy::new(10_000, 1_000_000)?)))
}

#[tokio::test(start_paused = true)]
async fn services_run_and_stop_together() -> TestResult {
    let book = tip_book()?;
    assert_eq!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);

    let (hash_source, _calls) = hashes(vec![HashStep::Hash(2), HashStep::Hash(3)]);
    let (sender, receiver) = blockhash_channel(Hash::new_from_array([1; 32]));
    let services = Services::new()
        .with_blockhash(BlockhashFeed {
            source: hash_source,
            sender,
            interval: Duration::from_secs(2),
        })
        .with_tip_feed(TipFeed {
            source: ScriptedTips {
                floors: VecDeque::from([Ok(market_floor())]),
            },
            book: Arc::clone(&book),
            settings: TipFeedSettings::default(),
        });

    tokio::time::sleep(Duration::from_millis(4_100)).await;
    settle().await;
    // The tip left the fallback; the blockhash moved on.
    assert_ne!(
        book.quote(Urgency::Exit, 1_000_000_000)?.basis,
        TipBasis::Fallback
    );
    assert_eq!(receiver.borrow().0, Hash::new_from_array([3; 32]));

    let report = services.stop().await?;
    assert_eq!(report.blockhash.refreshed, 2);
    let tips = report.tip_feed.ok_or("the tip feed did not run")?;
    assert_eq!(tips.updates, 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn services_without_a_tip_feed_report_none() -> TestResult {
    let (hash_source, _calls) = hashes(vec![]);
    let (sender, _receiver) = blockhash_channel(Hash::new_from_array([1; 32]));
    let services = Services::new().with_blockhash(BlockhashFeed {
        source: hash_source,
        sender,
        interval: Duration::from_secs(2),
    });
    let report = services.stop().await?;
    assert!(report.tip_feed.is_none());
    assert_eq!(report.blockhash, RefresherStats::default());
    // No services at all is valid as well.
    let empty = Services::default().stop().await?;
    assert!(empty.tip_feed.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_tip_feed_with_bad_settings_fails_the_stop() -> TestResult {
    let services = Services::new().with_tip_feed(TipFeed {
        source: ScriptedTips {
            floors: VecDeque::new(),
        },
        book: tip_book()?,
        settings: TipFeedSettings {
            interval: Duration::from_millis(1),
            request_timeout: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        },
    });
    settle().await;
    let error = err_of(services.stop().await)?;
    assert_eq!(
        error,
        BridgeError::ServiceFailed {
            service: "tip_feed"
        }
    );
    Ok(())
}

// =====================================================================================================
// A whole dry session
// =====================================================================================================

/// A stream that delivers the prepared updates, then goes silent.
struct ScriptedStream {
    updates: VecDeque<StreamUpdate>,
}

impl EventStream for ScriptedStream {
    async fn next_update(&mut self) -> Result<Option<StreamUpdate>, MonitorError> {
        match self.updates.pop_front() {
            Some(update) => Ok(Some(update)),
            None => {
                std::future::pending::<()>().await;
                Ok(None)
            }
        }
    }
}

struct ScriptedTransport {
    updates: Vec<StreamUpdate>,
}

impl StreamTransport for ScriptedTransport {
    type Stream = ScriptedStream;

    async fn connect(
        &mut self,
        _request: &SubscriptionRequest,
    ) -> Result<Self::Stream, MonitorError> {
        Ok(ScriptedStream {
            updates: self.updates.clone().into(),
        })
    }
}

/// Records every order and answers like a dry-run client: `Simulated`.
#[derive(Default)]
struct RecordingSubmitter {
    orders: Mutex<Vec<SellOrder>>,
}

impl SellSubmitter for RecordingSubmitter {
    async fn submit(&self, order: &SellOrder) -> Result<SellOutcome, ExitError> {
        self.orders
            .lock()
            .map_err(|_| ExitError::InvalidConfig {
                reason: "recording submitter poisoned",
            })?
            .push(*order);
        Ok(SellOutcome::Simulated)
    }
}

#[tokio::test(start_paused = true)]
async fn a_dry_session_runs_from_the_stream_to_the_orders() -> TestResult {
    // The market right after our own 1 % buy, as the stream would deliver it.
    let curve = CurveParams::default();
    let buy = BuyPlan::from_supply_bps(&curve, 100, DEFAULT_TOTAL_FEE_BPS, 500)?;
    let mut market = MarketState::new(curve, DEFAULT_TOTAL_FEE_BPS);
    market.apply_creator_buy(&buy)?;
    let curve_account = key(0x42);
    let update = market.account_update(curve_account);
    let transport = ScriptedTransport {
        updates: vec![StreamUpdate::Slot(1), update],
    };

    let mut args = exit_args();
    args.tokens = buy.token_amount;
    args.entry_virtual_sol_reserves = PriceReference::after_buy(&curve, &buy)?.virtual_sol_reserves;
    args.entry_virtual_token_reserves =
        PriceReference::after_buy(&curve, &buy)?.virtual_token_reserves;
    args.tranches = 2;
    let engine = args.engine(500, 400_000)?;

    // Services: a blockhash refresher and a tip updater, as `main` starts them.
    let (hash_source, _calls) = hashes(vec![
        HashStep::Hash(2),
        HashStep::Hash(3),
        HashStep::Hash(4),
    ]);
    let (sender, _receiver) = blockhash_channel(Hash::new_from_array([1; 32]));
    let book = tip_book()?;
    let services = Services::new()
        .with_blockhash(BlockhashFeed {
            source: hash_source,
            sender,
            interval: Duration::from_secs(2),
        })
        .with_tip_feed(TipFeed {
            source: ScriptedTips {
                floors: VecDeque::from([Ok(market_floor())]),
            },
            book: Arc::clone(&book),
            settings: TipFeedSettings::default(),
        });

    let submitter = Arc::new(RecordingSubmitter::default());
    let (_trigger, shutdown) = shutdown_channel();
    let report = run_exit_pipeline(
        transport,
        Arc::clone(&submitter),
        ExitPipeline {
            engine,
            curve_account,
            genesis: tokio::time::Instant::now(),
            monitor: MonitorSettings::default(),
            drain_timeout: Duration::from_secs(5),
            balance: None,
        },
        None,
        shutdown,
    )
    .await?;

    let services_report = services.stop().await?;
    assert!(
        services_report.blockhash.refreshed >= 1,
        "the blockhash never moved"
    );
    assert_eq!(services_report.tip_feed.map(|stats| stats.updates), Some(1));

    let summary = summarize(&report);
    assert_eq!(summary.orders_issued, 2);
    assert!(!summary.needs_attention);
    assert!(!summary.interrupted);
    assert_eq!(summary.dropped, 0);
    assert_eq!(report.summary.position.tokens_sold, buy.token_amount);
    let orders = submitter.orders.lock().map_err(|_| "poisoned")?;
    assert_eq!(orders.len(), 2, "one order per tranche");
    Ok(())
}

// =====================================================================================================
// The sessions (`runner`) on a parsed configuration
// =====================================================================================================

fn base_vars(dry_run: bool) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for (name, value) in [
        ("RPC_URL", "https://rpc.example.com"),
        ("JITO_ENGINE_URL", "https://jito.example.com"),
        ("CREATOR_SOL_AMOUNT", "0.5"),
    ] {
        vars.insert(name.to_owned(), value.to_owned());
    }
    vars.insert("PRIVATE_KEY".to_owned(), keypair_json(7));
    vars.insert("DRY_RUN".to_owned(), dry_run.to_string());
    vars
}

fn armed_vars(path: &Path) -> HashMap<String, String> {
    let mut vars = base_vars(false);
    for (name, value) in [
        ("LIVE_TRADING", "true"),
        ("LIVE_CONFIRM", LIVE_CONFIRM_PHRASE),
        ("MAX_SOL_PER_TX", "1"),
        ("DAILY_DRAWDOWN_SOL", "2"),
    ] {
        vars.insert(name.to_owned(), value.to_owned());
    }
    vars.insert("LIVE_KEYPAIR_PATH".to_owned(), path.display().to_string());
    vars.insert(
        "GUARD_STATE_PATH".to_owned(),
        path.with_file_name("guards.state").display().to_string(),
    );
    vars
}

/// Creates the persisted spending state next to the keypair file, as
/// `krypton init-state` does.
fn init_guard_state(keypair: &Path) -> TestResult {
    krypton::execution::persist::init_state(&keypair.with_file_name("guards.state"), 0)?;
    Ok(())
}

#[test]
fn a_dry_check_needs_no_flags_and_reports_the_build() -> TestResult {
    let config = Config::from_vars(base_vars(true))?;
    let report = check(&config, &LiveRequest::default())?;
    assert_eq!(report.mode, Mode::Dry);
    assert!(!report.keypair_verified);
    assert!(!report.live_armed);
    assert!(report.yellowstone_origin.is_none());
    assert!(!report.tip_feed_configured);
    assert_eq!(report.grpc_compiled, GRPC_COMPILED);
    Ok(())
}

#[test]
fn a_check_verifies_the_keypair_even_when_dry() -> TestResult {
    let dir = scratch("check-dry")?;
    let path = write_keypair(&dir, "payer.json", 7, 0o600)?;
    let config = Config::from_vars(armed_vars(&path))?;
    // No `--live`: the environment is fully armed and the answer is still dry.
    let report = check(&config, &LiveRequest::default())?;
    assert_eq!(report.mode, Mode::Dry);
    assert!(report.keypair_verified);
    assert!(!report.live_armed);
    Ok(())
}

#[test]
fn a_live_check_crosses_the_whole_barrier_without_the_network() -> TestResult {
    let dir = scratch("check-live")?;
    let path = write_keypair(&dir, "payer.json", 7, 0o600)?;
    init_guard_state(&path)?;
    let config = Config::from_vars(armed_vars(&path))?;
    let report = check(&config, &live_request(Some(LIVE_CONFIRM_PHRASE)))?;
    assert_eq!(report.mode, Mode::Live);
    assert!(report.keypair_verified);
    assert!(report.live_armed);
    Ok(())
}

#[test]
fn a_live_check_without_the_phrase_is_refused() -> TestResult {
    let dir = scratch("check-nophrase")?;
    let path = write_keypair(&dir, "payer.json", 7, 0o600)?;
    let config = Config::from_vars(armed_vars(&path))?;
    let error = err_of(check(&config, &live_request(Some("please"))))?;
    assert!(matches!(
        error,
        LauncherError::Bridge(BridgeError::LiveConfirmationMissing)
    ));
    Ok(())
}

#[test]
fn a_live_check_with_dry_run_on_is_refused() -> TestResult {
    let dir = scratch("check-dryrun")?;
    let path = write_keypair(&dir, "payer.json", 7, 0o600)?;
    let mut vars = armed_vars(&path);
    vars.insert("DRY_RUN".to_owned(), "true".to_owned());
    let config = Config::from_vars(vars)?;
    let error = err_of(check(&config, &live_request(Some(LIVE_CONFIRM_PHRASE))))?;
    assert!(matches!(
        error,
        LauncherError::Live(LiveError::DryRunEnabled)
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_check_refuses_a_loose_keypair_file_in_any_mode() -> TestResult {
    let dir = scratch("check-loose")?;
    let path = write_keypair(&dir, "payer.json", 7, 0o644)?;
    let config = Config::from_vars(armed_vars(&path))?;
    for request in [
        LiveRequest::default(),
        live_request(Some(LIVE_CONFIRM_PHRASE)),
    ] {
        let error = err_of(check(&config, &request))?;
        assert!(matches!(
            error,
            LauncherError::Live(LiveError::KeypairFile { .. })
        ));
    }
    Ok(())
}

#[tokio::test]
async fn an_exit_without_a_yellowstone_endpoint_does_not_start() -> TestResult {
    let config = Config::from_vars(base_vars(true))?;
    let (_trigger, shutdown) = shutdown_channel();
    let error = err_of(run_exit_command(config, exit_args(), Mode::Dry, shutdown).await)?;
    assert!(matches!(
        error,
        LauncherError::Bridge(BridgeError::YellowstoneNotConfigured)
    ));
    Ok(())
}

#[cfg(not(feature = "grpc"))]
#[tokio::test]
async fn a_build_without_the_grpc_link_refuses_to_start_an_exit() -> TestResult {
    // A live session crosses the barrier first (audit #46), so it needs a
    // configuration that arms; the gRPC check comes right after.
    let dir = scratch("nogrpc")?;
    let path = write_keypair(&dir, "payer.json", 7, 0o600)?;
    init_guard_state(&path)?;
    for mode in [Mode::Dry, Mode::Live] {
        let (_trigger, shutdown) = shutdown_channel();
        let mut config = Config::from_vars(armed_vars(&path))?;
        config.production = ProductionSettings::from_raw(raw(
            Some("https://grpc.example.com"),
            Some("abc123token"),
            None,
        ))?;
        let error = err_of(run_exit_command(config, exit_args(), mode, shutdown).await)?;
        assert!(matches!(
            error,
            LauncherError::Bridge(BridgeError::GrpcNotCompiled)
        ));
    }
    Ok(())
}

#[test]
fn the_check_report_holds_nothing_secret() -> TestResult {
    let mut config = Config::from_vars(base_vars(true))?;
    config.production = ProductionSettings::from_raw(raw(
        Some("https://grpc.example.com/path-with-a-key"),
        Some("tok3n-value"),
        Some("https://tips.example.com/key-in-path"),
    ))?;
    let report = check(&config, &LiveRequest::default())?;
    assert_eq!(
        report.yellowstone_origin.as_deref(),
        Some("https://grpc.example.com")
    );
    assert!(report.tip_feed_configured);
    let text = format!("{report:?}");
    for secret in ["path-with-a-key", "tok3n-value", "key-in-path"] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
    Ok(())
}

// =====================================================================================================
// The report
// =====================================================================================================

fn report(position: EngineSnapshot, unsettled: bool, interrupted: bool) -> ExitReport {
    ExitReport {
        summary: ExitSummary {
            position,
            observations: 12,
            decode_errors: 1,
            interrupted,
            unsettled,
            balance: BalanceOutcome::NotNeeded,
        },
        health: HealthStatus {
            reconnects: 3,
            ..HealthStatus::default()
        },
    }
}

#[test]
fn a_clean_exit_needs_no_attention() {
    let summary = summarize(&report(
        EngineSnapshot {
            orders_issued: 4,
            tokens_sold: 1_000,
            ..EngineSnapshot::default()
        },
        false,
        false,
    ));
    assert!(!summary.needs_attention);
    assert_eq!(summary.orders_issued, 4);
    assert_eq!(summary.observations, 12);
    assert_eq!(summary.decode_errors, 1);
    assert_eq!(summary.reconnects, 3);
}

#[test]
fn unresolved_or_unsettled_orders_need_attention() {
    let unresolved = EngineSnapshot {
        tokens_unresolved: 5,
        ..EngineSnapshot::default()
    };
    let in_flight = EngineSnapshot {
        tokens_in_flight: 5,
        ..EngineSnapshot::default()
    };
    assert!(summarize(&report(unresolved, false, false)).needs_attention);
    assert!(summarize(&report(in_flight, false, true)).needs_attention);
    assert!(summarize(&report(EngineSnapshot::default(), true, false)).needs_attention);
}

#[test]
fn the_summary_carries_no_amounts() {
    // The loggable summary is made of counters and flags only: compile-time proof
    // by exhaustive destructuring, so a new field must be looked at here.
    let summary = summarize(&report(EngineSnapshot::default(), false, false));
    let krypton::app::SessionSummary {
        orders_issued: _,
        dropped: _,
        stop_loss_fired: _,
        observations: _,
        decode_errors: _,
        reconnects: _,
        interrupted: _,
        needs_attention: _,
    } = summary;
}

// =====================================================================================================
// Source hygiene
// =====================================================================================================

#[test]
fn source_scan_bridge_modules_have_no_panics_or_unsafe_code() {
    const SOURCES: [(&str, &str); 6] = [
        ("app.rs", include_str!("../src/app.rs")),
        ("production.rs", include_str!("../src/production.rs")),
        ("runner.rs", include_str!("../src/runner.rs")),
        ("rpc.rs", include_str!("../src/network/rpc.rs")),
        ("grpc.rs", include_str!("../src/network/grpc.rs")),
        ("main.rs", include_str!("../src/main.rs")),
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
        // `#![forbid(unsafe_code)]` is the guard, not a violation.
        let source = source.replace("forbid(unsafe_code)", "");
        for word in FORBIDDEN {
            assert!(!source.contains(word), "{name} contains `{word}`");
        }
        // Unchecked indexing panics too: `[` after an identifier or `)` outside
        // attributes and macros would be caught by clippy; here, the obvious form.
        for pattern in ["args[", "argv[", "bytes[", "data[", "keys[", "vars["] {
            assert!(!source.contains(pattern), "{name} indexes with `{pattern}`");
        }
    }
}

#[test]
fn the_bridge_logs_labels_and_counters_only() {
    // No event of the bridge formats a key, an address, a signature or an amount:
    // the only values interpolated are the ones named below.
    let source = include_str!("../src/app.rs");
    for forbidden in ["pubkey", "signature", "secret", "keypair =", "lamports ="] {
        for line in source.lines().filter(|line| {
            line.contains("info!") || line.contains("warn!") || line.contains("error!")
        }) {
            assert!(!line.to_lowercase().contains(forbidden), "{line}");
        }
    }
}
