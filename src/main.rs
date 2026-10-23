//! The `krypton` binary: command line, configuration, logging, the process-level
//! dry-to-live barrier and the exit session.
//!
//! The logic lives in the library (`krypton::app`, `krypton::runner`), where the
//! test-suite covers it; this file only strings the steps together:
//!
//! 1. parse the command line (nothing else is read before it parses);
//! 2. load the configuration while the process is still single-threaded
//!    (`PRIVATE_KEY` and `YELLOWSTONE_X_TOKEN` are scrubbed from the process
//!    environment there);
//! 3. install the `tracing` subscriber: `info` unless `LOG_LEVEL`/`RUST_LOG`
//!    say otherwise, and nothing sensitive at any level;
//! 4. verify the payer keypair file (owner-only, `0600`) when one is configured;
//! 5. decide the mode: dry unless `--live --confirm-live <PHRASE>` **and** the
//!    environment switches all agree;
//! 6. run.
//!
//! Output is written with `write!` and errors ignored: a closed pipe must not
//! panic the process (Zero Panics policy).

#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

use std::io::Write;
use std::process::ExitCode;

use krypton::app::{decide_mode, preflight_keypair, Cli, Command, USAGE};
use krypton::config::Config;
use krypton::errors::{BridgeError, CliError, LauncherError};
use krypton::logging;
use krypton::monitor::{shutdown_channel, trigger_on_ctrl_c};
use krypton::runner;
use tracing::warn;

/// Exit status when the run ended with orders whose outcome is unknown: the
/// operator must look at the token account before doing anything else.
const EXIT_NEEDS_ATTENTION: u8 = 3;
/// Exit status for a command-line mistake.
const EXIT_USAGE: u8 = 2;

fn say(text: &str) {
    // A closed stdout is not worth a panic.
    let _ = std::io::stdout().write_all(text.as_bytes());
}

fn complain(text: &str) {
    let _ = std::io::stderr().write_all(text.as_bytes());
}

fn arguments() -> Result<Vec<String>, CliError> {
    // `std::env::args` panics on non-Unicode input; `args_os` does not.
    std::env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| CliError::UnknownArgument)
        })
        .collect()
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            // Error texts carry no keys, URLs, paths or amounts (see `errors`).
            complain(&format!("krypton: {error}\n"));
            if matches!(error, LauncherError::Cli(_)) {
                ExitCode::from(EXIT_USAGE)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn yes_no(flag: bool) -> &'static str {
    if flag {
        "yes"
    } else {
        "no"
    }
}

fn run() -> Result<ExitCode, LauncherError> {
    let cli = Cli::parse(arguments()?)?;
    if cli.command == Command::Help {
        say(USAGE);
        return Ok(ExitCode::SUCCESS);
    }

    // Single-threaded here: `Config::from_env` edits the process environment.
    let config = Config::from_env()?;
    logging::init(config.log_level)?;

    if !cli.live.requested && config.live.live_trading && !config.dry_run {
        warn!("the environment arms live trading but --live was not given: running dry");
    }

    match cli.command {
        Command::Help => Ok(ExitCode::SUCCESS),
        Command::Check => {
            let report = runner::check(&config, &cli.live)?;
            say(&format!(
                "mode: {}\n\
                 payer keypair file verified (owner-only): {}\n\
                 live barrier crossed: {}\n\
                 yellowstone endpoint: {}\n\
                 yellowstone gRPC link compiled in: {}\n\
                 tip feed url configured: {}\n",
                report.mode.label(),
                yes_no(report.keypair_verified),
                yes_no(report.live_armed),
                report.yellowstone_origin.as_deref().unwrap_or("not set"),
                yes_no(report.grpc_compiled),
                yes_no(report.tip_feed_configured),
            ));
            Ok(ExitCode::SUCCESS)
        }
        Command::Exit(args) => {
            // The keypair file is verified in every mode when one is configured,
            // so a bad permission shows up in a rehearsal, not on the real run.
            preflight_keypair(&config.live, config.wallet())?;
            let mode = decide_mode(&cli.live, config.dry_run, &config.live)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| BridgeError::Runtime)?;
            let summary = runtime.block_on(async move {
                let (trigger, shutdown) = shutdown_channel();
                // If the Ctrl-C handler cannot be installed the trigger is
                // dropped, which also requests shutdown: the safe failure.
                tokio::spawn(async move {
                    if trigger_on_ctrl_c(trigger).await.is_err() {
                        warn!("cannot listen for Ctrl-C: shutting down");
                    }
                });
                runner::run_exit_command(config, args, mode, shutdown).await
            })?;
            if summary.needs_attention {
                Ok(ExitCode::from(EXIT_NEEDS_ATTENTION))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}
