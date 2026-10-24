//! Audit hardening of 2026-10-24 (findings #35, #37, #44, #45, #46, #47, #48).
//!
//! Everything here runs offline: files in a scratch directory, scripted
//! engines, no sockets.

use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use krypton::app::{
    Cli, Command, ExitArgs, Mode, DEFAULT_SELL_BPS, DEFAULT_TRANCHES, MAX_ELAPSED_MS,
};
use krypton::config::Config;
use krypton::errors::{CliError, GuardError, LauncherError, LiveError, StateFileError};
use krypton::execution::gate::open_guards;
use krypton::execution::guards::{GuardLimits, SpendKind, SpendingGuards, SECONDS_PER_DAY};
use krypton::execution::live::LiveExecutor;
use krypton::execution::persist::{
    init_state, read_state, write_state, PersistedGuards, StateLock, StateWriter,
    MAX_STATE_FILE_BYTES, STATE_HEADER,
};
use krypton::execution::settings::{LiveSettings, LIVE_CONFIRM_PHRASE};
use krypton::monitor::shutdown_channel;
use krypton::parsing::{endpoint_scheme_allowed, is_loopback_host};
use krypton::production::{ProductionSettings, RawProductionSettings};
use krypton::runner::{init_state as run_init_state, run_exit_command};
use krypton::security::SecretString;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;

type TestResult = Result<(), Box<dyn Error>>;

const DAY: u64 = SECONDS_PER_DAY;

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn scratch(tag: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("krypton-audit-{}-{tag}", std::process::id()));
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

fn keypair_json(seed: u8) -> String {
    let items: Vec<String> = Keypair::new_from_array([seed; 32])
        .to_bytes()
        .iter()
        .map(ToString::to_string)
        .collect();
    format!("[{}]", items.join(","))
}

fn limits() -> Result<GuardLimits, Box<dyn Error>> {
    Ok(GuardLimits::new(1_000, 3_000, 500)?)
}

fn settings_with_state(path: Option<PathBuf>) -> LiveSettings {
    LiveSettings {
        live_trading: true,
        confirmed: true,
        keypair_path: None,
        guard_state_path: path,
        max_spend_per_tx_lamports: Some(1_000),
        daily_drawdown_lamports: Some(3_000),
    }
}

fn record(day: u64, committed: u64, returned: u64, halted: bool) -> PersistedGuards {
    PersistedGuards {
        day,
        committed_lamports: committed,
        returned_lamports: returned,
        halted,
    }
}

// =====================================================================================================
// #35 - the record: format, checksum, strict decoding
// =====================================================================================================

#[test]
fn a_record_round_trips_through_its_text() -> TestResult {
    for original in [
        PersistedGuards::default(),
        record(20_385, 1_500_000_000, 250, false),
        record(u64::MAX, u64::MAX, u64::MAX, true),
    ] {
        let text = original.encode();
        assert!(text.starts_with(STATE_HEADER));
        assert!(text.ends_with('\n'));
        assert!(text.len() < 200, "records stay tiny");
        assert_eq!(PersistedGuards::decode(&text)?, original);
    }
    Ok(())
}

#[test]
fn decoding_rejects_anything_that_is_not_exactly_what_encode_writes() -> TestResult {
    let good = record(20_385, 1_500, 200, true).encode();
    let tampered: Vec<(&str, String)> = vec![
        ("empty", String::new()),
        (
            "value edited",
            good.replace("committed=1500", "committed=15"),
        ),
        ("halt flag flipped", good.replace("halted=1", "halted=0")),
        ("halted out of range", good.replace("halted=1", "halted=2")),
        ("checksum edited", {
            let flipped = if good.contains("check=0") {
                "check=1"
            } else {
                "check=0"
            };
            let start = good.find("check=").ok_or("no checksum")?;
            format!("{}{flipped}{}", &good[..start], &good[start + 7..])
        }),
        ("unknown version", good.replace("v1", "v2")),
        ("no final newline", good.trim_end().to_owned()),
        ("extra line", format!("{good}extra=1\n")),
        ("missing line", good.replace("returned=200\n", "")),
        (
            "reordered",
            good.replace(
                "committed=1500\nreturned=200\n",
                "returned=200\ncommitted=1500\n",
            ),
        ),
        ("plus sign", good.replace("day=20385", "day=+20385")),
        ("leading zero", good.replace("day=20385", "day=020385")),
        ("negative", good.replace("day=20385", "day=-1")),
        (
            "overflow",
            good.replace("day=20385", "day=99999999999999999999"),
        ),
        ("crlf", good.replace('\n', "\r\n")),
    ];
    for (label, text) in tampered {
        let error = err_of(PersistedGuards::decode(&text))?;
        assert!(
            matches!(error, StateFileError::Corrupt { .. }),
            "{label}: {error:?}"
        );
    }
    Ok(())
}

// =====================================================================================================
// #35 - the file: atomic writes, 0600, fail-closed reads
// =====================================================================================================

#[test]
fn a_written_record_reads_back_and_leaves_no_temporary_file() -> TestResult {
    let dir = scratch("write")?;
    let path = dir.join("guards.state");
    write_state(&path, &record(7, 100, 50, false))?;
    assert_eq!(read_state(&path)?, record(7, 100, 50, false));
    write_state(&path, &record(7, 400, 50, true))?;
    assert_eq!(read_state(&path)?, record(7, 400, 50, true));
    assert!(!dir.join("guards.state.tmp").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn the_state_file_is_private_and_a_stale_temporary_does_not_block_a_write() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("mode")?;
    let path = dir.join("guards.state");
    // A crashed earlier write left a world-readable temporary behind.
    std::fs::write(dir.join("guards.state.tmp"), "junk")?;
    set_mode(&dir.join("guards.state.tmp"), 0o644)?;
    write_state(&path, &record(1, 2, 3, false))?;
    let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "created with 0600, never wider");
    assert!(!dir.join("guards.state.tmp").exists());
    Ok(())
}

#[test]
fn a_missing_file_is_reported_as_missing() -> TestResult {
    let dir = scratch("missing")?;
    assert_eq!(
        err_of(read_state(&dir.join("nope.state")))?,
        StateFileError::Missing
    );
    Ok(())
}

#[test]
fn a_corrupted_or_oversized_file_is_rejected() -> TestResult {
    let dir = scratch("corrupt")?;
    let path = dir.join("guards.state");
    for (label, bytes) in [
        ("empty", Vec::new()),
        ("garbage", b"not a state file".to_vec()),
        ("binary", vec![0xff, 0xfe, 0x00, 0x01]),
        ("truncated", {
            let text = record(5, 10, 0, false).encode();
            text.as_bytes()[..text.len() - 6].to_vec()
        }),
        (
            "oversized",
            vec![b'a'; usize::try_from(MAX_STATE_FILE_BYTES)? + 1],
        ),
    ] {
        std::fs::write(&path, bytes)?;
        set_mode(&path, 0o600)?;
        let error = err_of(read_state(&path))?;
        assert!(
            matches!(error, StateFileError::Corrupt { .. }),
            "{label}: {error:?}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_file_others_can_read_or_a_symlink_is_not_trusted() -> TestResult {
    let dir = scratch("insecure")?;
    let path = dir.join("guards.state");
    write_state(&path, &record(5, 10, 0, false))?;
    for mode in [0o644, 0o640, 0o604, 0o660] {
        set_mode(&path, mode)?;
        let error = err_of(read_state(&path))?;
        assert!(
            matches!(error, StateFileError::Insecure { .. }),
            "{mode:o}: {error:?}"
        );
    }
    set_mode(&path, 0o400)?;
    assert!(read_state(&path).is_ok(), "stricter than 0600 is fine");
    set_mode(&path, 0o600)?;

    let link = dir.join("link.state");
    std::os::unix::fs::symlink(&path, &link)?;
    assert!(matches!(
        err_of(read_state(&link))?,
        StateFileError::Insecure { .. }
    ));
    // A directory is not a state file either.
    assert!(matches!(
        err_of(read_state(&dir))?,
        StateFileError::Insecure { .. }
    ));
    Ok(())
}

#[test]
fn init_creates_the_file_once_and_never_overwrites() -> TestResult {
    let dir = scratch("init")?;
    let path = dir.join("guards.state");
    init_state(&path, 20_000)?;
    assert_eq!(read_state(&path)?, record(20_000, 0, 0, false));
    write_state(&path, &record(20_000, 900, 0, true))?;
    assert_eq!(
        err_of(init_state(&path, 20_001))?,
        StateFileError::AlreadyExists
    );
    assert_eq!(
        read_state(&path)?,
        record(20_000, 900, 0, true),
        "the existing books survive a second init"
    );
    Ok(())
}

#[test]
fn the_lock_has_one_owner_at_a_time() -> TestResult {
    let dir = scratch("lock")?;
    let path = dir.join("guards.state");
    let first = StateLock::acquire(&path)?;
    assert_eq!(err_of(StateLock::acquire(&path))?, StateFileError::Locked);
    drop(first);
    let _second = StateLock::acquire(&path)?;
    Ok(())
}

// =====================================================================================================
// #35 - the books: restore
// =====================================================================================================

#[test]
fn restore_keeps_today_and_forgets_yesterdays_counters_but_not_the_halt() -> TestResult {
    let now = 10 * DAY + 5;
    // Same day: everything is kept.
    let same = SpendingGuards::restore(limits()?, &record(10, 1_200, 100, false), now)?;
    let snapshot = same.snapshot()?;
    assert_eq!(snapshot.day, 10);
    assert_eq!(snapshot.committed_lamports, 1_200);
    assert_eq!(snapshot.returned_lamports, 100);
    assert!(!snapshot.halted);

    // An earlier day: the counters reset, the latch stays.
    let later = SpendingGuards::restore(limits()?, &record(8, 2_000, 0, true), now)?;
    let snapshot = later.snapshot()?;
    assert_eq!(snapshot.day, 10);
    assert_eq!(snapshot.committed_lamports, 0);
    assert!(snapshot.halted, "a halt survives the day rollover");
    assert_eq!(
        err_of(later.reserve(SpendKind::Entry, 10, now))?,
        GuardError::Halted
    );
    Ok(())
}

#[test]
fn restore_refuses_a_record_from_the_future() -> TestResult {
    let error = err_of(SpendingGuards::restore(
        limits()?,
        &record(11, 0, 0, false),
        10 * DAY,
    ))?;
    assert_eq!(error, GuardError::PersistedStateInFuture);
    Ok(())
}

#[test]
fn restore_halts_when_the_restored_spend_already_reaches_the_limit() -> TestResult {
    // The limit (3000) was lowered below what the record already spent.
    let guards = SpendingGuards::restore(limits()?, &record(10, 3_000, 0, false), 10 * DAY)?;
    assert!(guards.is_halted()?);
    // Returns that offset the spend do not trigger it.
    let guards = SpendingGuards::restore(limits()?, &record(10, 3_000, 1_000, false), 10 * DAY)?;
    assert!(!guards.is_halted()?);
    Ok(())
}

#[test]
fn restored_spend_counts_against_the_drawdown() -> TestResult {
    let now = 10 * DAY;
    let guards = SpendingGuards::restore(limits()?, &record(10, 2_500, 0, false), now)?;
    // 2500 spent + 600 > 3000
    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 600, now))?,
        GuardError::DailyDrawdown
    );
    let ok = guards.reserve(SpendKind::Entry, 500, now)?;
    guards.release(ok)?;
    Ok(())
}

// =====================================================================================================
// #35 - the books: persistence across "runs"
// =====================================================================================================

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn two_successive_runs_share_the_days_spend() -> TestResult {
    let dir = scratch("runs")?;
    let path = dir.join("guards.state");
    let now = 10 * DAY + 100;
    init_state(&path, 10)?;
    let settings = settings_with_state(Some(path.clone()));

    // Run 1 spends 2000 and gets 500 back.
    {
        let guards = open_guards(&settings, limits()?, now)?;
        let first = guards.reserve(SpendKind::Entry, 1_000, now)?;
        guards.commit(first, now)?;
        let second = guards.reserve(SpendKind::Entry, 1_000, now)?;
        guards.commit(second, now)?;
        guards.record_return(500, now)?;
        assert!(guards.flush_persistence(), "the books reached the disk");
    }

    // Run 2 starts from the file, not from zero.
    let guards = open_guards(&settings, limits()?, now + 60)?;
    let snapshot = guards.snapshot()?;
    assert_eq!(snapshot.committed_lamports, 2_000);
    assert_eq!(snapshot.returned_lamports, 500);
    assert_eq!(snapshot.realised_drawdown_lamports, 1_500);
    // 1500 + 1000 is fine, 1500 + 1000 + 1000 is not.
    let third = guards.reserve(SpendKind::Entry, 1_000, now + 60)?;
    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 1_000, now + 60))?,
        GuardError::DailyDrawdown
    );
    guards.commit(third, now + 60)?;
    assert!(guards.flush_persistence());
    drop(guards);
    assert_eq!(read_state(&path)?, record(10, 3_000, 500, false));
    Ok(())
}

#[test]
fn the_halt_latch_survives_a_restart_and_a_day_change() -> TestResult {
    let dir = scratch("halt")?;
    let path = dir.join("guards.state");
    let now = 10 * DAY;
    init_state(&path, 10)?;
    let settings = settings_with_state(Some(path.clone()));
    {
        let guards = open_guards(&settings, limits()?, now)?;
        for _ in 0..3 {
            let order = guards.reserve(SpendKind::Entry, 1_000, now)?;
            guards.commit(order, now)?;
        }
        assert!(guards.is_halted()?, "3000 reached the daily limit");
        assert!(guards.flush_persistence());
    }
    // Same day, new process.
    {
        let guards = open_guards(&settings, limits()?, now + 10)?;
        assert!(guards.is_halted()?);
        assert_eq!(
            err_of(guards.reserve(SpendKind::Entry, 1, now + 10))?,
            GuardError::Halted
        );
        // Exits are never blocked.
        let exit = guards.reserve(SpendKind::Exit, 1, now + 10)?;
        guards.release(exit)?;
    }
    // Two days later: the counters are fresh, the latch is not.
    let guards = open_guards(&settings, limits()?, now + 2 * DAY)?;
    let snapshot = guards.snapshot()?;
    assert_eq!(snapshot.committed_lamports, 0);
    assert!(snapshot.halted);
    drop(guards);
    // The rollover itself was written back.
    assert_eq!(read_state(&path)?, record(12, 0, 0, true));
    Ok(())
}

#[test]
fn rearming_is_persisted_too() -> TestResult {
    let dir = scratch("rearm")?;
    let path = dir.join("guards.state");
    let now = 10 * DAY;
    write_state(&path, &record(10, 0, 0, true))?;
    let settings = settings_with_state(Some(path.clone()));
    {
        let guards = open_guards(&settings, limits()?, now)?;
        assert!(guards.is_halted()?);
        guards.rearm()?;
        assert!(guards.flush_persistence());
    }
    assert!(!read_state(&path)?.halted);
    Ok(())
}

#[test]
fn a_second_process_cannot_share_the_state() -> TestResult {
    let dir = scratch("exclusive")?;
    let path = dir.join("guards.state");
    init_state(&path, 10)?;
    let settings = settings_with_state(Some(path));
    let first = open_guards(&settings, limits()?, 10 * DAY)?;
    let error = err_of(open_guards(&settings, limits()?, 10 * DAY))?;
    assert!(
        matches!(error, LiveError::GuardState(StateFileError::Locked)),
        "{error:?}"
    );
    drop(first);
    open_guards(&settings, limits()?, 10 * DAY)?;
    Ok(())
}

// =====================================================================================================
// #35 - fail closed at arm time
// =====================================================================================================

#[test]
fn opening_the_guards_fails_closed_on_every_bad_state() -> TestResult {
    let dir = scratch("closed")?;
    let now = 10 * DAY;

    // No path configured.
    assert!(matches!(
        err_of(open_guards(&settings_with_state(None), limits()?, now))?,
        LiveError::GuardStatePathMissing
    ));

    // The file does not exist: never an implicit fresh day.
    let path = dir.join("guards.state");
    let settings = settings_with_state(Some(path.clone()));
    assert!(matches!(
        err_of(open_guards(&settings, limits()?, now))?,
        LiveError::GuardState(StateFileError::Missing)
    ));
    assert!(!path.exists(), "a refused arm must not create the file");

    // Corrupted.
    std::fs::write(&path, "krypton-guards-v1\nday=10\n")?;
    set_mode(&path, 0o600)?;
    assert!(matches!(
        err_of(open_guards(&settings, limits()?, now))?,
        LiveError::GuardState(StateFileError::Corrupt { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "krypton-guards-v1\nday=10\n",
        "a corrupted file is left for the operator, not overwritten"
    );

    // Dated in the future.
    write_state(&path, &record(11, 0, 0, false))?;
    assert!(matches!(
        err_of(open_guards(&settings, limits()?, now))?,
        LiveError::Guard(GuardError::PersistedStateInFuture)
    ));

    // Unreadable (a directory where the file should be).
    let directory = dir.join("is-a-directory");
    std::fs::create_dir_all(&directory)?;
    assert!(matches!(
        err_of(open_guards(
            &settings_with_state(Some(directory)),
            limits()?,
            now
        ))?,
        LiveError::GuardState(StateFileError::Insecure { .. } | StateFileError::Unreadable { .. })
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn opening_the_guards_refuses_a_world_readable_file() -> TestResult {
    let dir = scratch("closed-mode")?;
    let path = dir.join("guards.state");
    write_state(&path, &record(10, 0, 0, false))?;
    set_mode(&path, 0o644)?;
    assert!(matches!(
        err_of(open_guards(
            &settings_with_state(Some(path)),
            limits()?,
            10 * DAY
        ))?,
        LiveError::GuardState(StateFileError::Insecure { .. })
    ));
    Ok(())
}

fn armed_vars(dir: &Path, state: Option<&Path>) -> Result<HashMap<String, String>, Box<dyn Error>> {
    let keypair = dir.join("live.json");
    std::fs::write(&keypair, keypair_json(7))?;
    set_mode(&keypair, 0o600)?;
    let mut vars: HashMap<String, String> = [
        ("RPC_URL", "https://rpc.example.com"),
        ("JITO_ENGINE_URL", "https://jito.example.com"),
        ("CREATOR_SOL_AMOUNT", "0.5"),
        ("LIVE_TRADING", "true"),
        ("LIVE_CONFIRM", LIVE_CONFIRM_PHRASE),
        ("DRY_RUN", "false"),
        ("MAX_SOL_PER_TX", "1"),
        ("DAILY_DRAWDOWN_SOL", "2"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    vars.insert("PRIVATE_KEY".to_owned(), keypair_json(7));
    vars.insert(
        "LIVE_KEYPAIR_PATH".to_owned(),
        keypair.display().to_string(),
    );
    if let Some(state) = state {
        vars.insert("GUARD_STATE_PATH".to_owned(), state.display().to_string());
    }
    Ok(vars)
}

#[test]
fn live_arm_refuses_without_a_valid_state_file_and_accepts_with_one() -> TestResult {
    let dir = scratch("arm")?;
    let state = dir.join("guards.state");

    // 1. No GUARD_STATE_PATH at all.
    let config = Config::from_vars(armed_vars(&dir, None)?)?;
    assert!(matches!(
        err_of(LiveExecutor::arm(&config))?,
        LiveError::GuardStatePathMissing
    ));

    // 2. A path, but no file.
    let config = Config::from_vars(armed_vars(&dir, Some(&state))?)?;
    assert!(matches!(
        err_of(LiveExecutor::arm(&config))?,
        LiveError::GuardState(StateFileError::Missing)
    ));

    // 3. A corrupted file.
    std::fs::write(&state, "garbage")?;
    set_mode(&state, 0o600)?;
    assert!(matches!(
        err_of(LiveExecutor::arm(&config))?,
        LiveError::GuardState(StateFileError::Corrupt { .. })
    ));

    // 4. A valid file: armed, and the books are the file's.
    write_state(&state, &record(0, 0, 0, true))?;
    let executor = LiveExecutor::arm(&config)?;
    assert!(
        executor.guards().is_halted()?,
        "the persisted halt is honoured"
    );
    Ok(())
}

#[test]
fn arm_checks_the_barrier_before_it_looks_at_the_state_file() -> TestResult {
    // A dry run is refused for being dry, whatever the state file says.
    let dir = scratch("arm-order")?;
    let mut vars = armed_vars(&dir, Some(&dir.join("missing.state")))?;
    vars.insert("DRY_RUN".to_owned(), "true".to_owned());
    let config = Config::from_vars(vars)?;
    assert!(matches!(
        err_of(LiveExecutor::arm(&config))?,
        LiveError::DryRunEnabled
    ));
    Ok(())
}

// =====================================================================================================
// #35 - the writer: coalescing, failure, recovery
// =====================================================================================================

#[test]
fn the_writer_keeps_only_the_newest_record() -> TestResult {
    let dir = scratch("writer")?;
    let path = dir.join("guards.state");
    init_state(&path, 1)?;
    let writer = StateWriter::spawn(path.clone(), StateLock::acquire(&path)?)?;
    for committed in 1..=500_u64 {
        writer.submit(record(1, committed, 0, false));
    }
    assert!(writer.flush());
    assert!(!writer.has_failed());
    assert_eq!(read_state(&path)?, record(1, 500, 0, false));
    Ok(())
}

#[test]
fn a_failing_disk_blocks_entries_but_never_exits_and_recovery_unblocks_them() -> TestResult {
    let dir = scratch("failing")?;
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir)?;
    let path = state_dir.join("guards.state");
    let now = 10 * DAY;
    init_state(&path, 10)?;
    let settings = settings_with_state(Some(path.clone()));
    let guards = open_guards(&settings, limits()?, now)?;

    // The state directory disappears (disk unplugged, volume unmounted). Let
    // the startup write finish first so it cannot race the removal.
    assert!(guards.flush_persistence());
    std::fs::remove_dir_all(&state_dir)?;
    let order = guards.reserve(SpendKind::Exit, 100, now)?;
    guards.commit(order, now)?;
    assert!(
        !guards.flush_persistence(),
        "the newest books are not on disk"
    );

    assert_eq!(
        err_of(guards.reserve(SpendKind::Entry, 100, now))?,
        GuardError::PersistenceFailed
    );
    let exit = guards.reserve(SpendKind::Exit, 100, now)?;
    guards.release(exit)?;

    // The disk comes back: the writer retries by itself and entries resume.
    std::fs::create_dir_all(&state_dir)?;
    assert!(
        wait_until(|| guards
            .reserve(SpendKind::Entry, 100, now)
            .map(|r| guards.release(r))
            .is_ok()),
        "entries resume once the record is written"
    );
    assert!(guards.flush_persistence());
    assert_eq!(read_state(&path)?, record(10, 100, 0, false));
    Ok(())
}

// =====================================================================================================
// #44 - `--elapsed-ms` anchors the exit window at the launch
// =====================================================================================================

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn exit_argv(extra: &[&str]) -> Vec<String> {
    let mint = Pubkey::new_from_array([9; 32]).to_string();
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

fn parse_exit(extra: &[&str]) -> Result<ExitArgs, Box<dyn Error>> {
    match Cli::parse(exit_argv(extra))?.command {
        Command::Exit(parsed) => Ok(parsed),
        _ => Err("expected the exit command".into()),
    }
}

fn exit_args() -> ExitArgs {
    ExitArgs {
        mint: Pubkey::new_from_array([9; 32]),
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

#[test]
fn the_elapsed_flag_defaults_to_zero_and_reads_both_spellings() -> TestResult {
    assert_eq!(parse_exit(&[])?.elapsed_since_launch_ms, 0);
    assert_eq!(
        parse_exit(&["--elapsed-ms", "2500"])?.elapsed_since_launch_ms,
        2_500
    );
    assert_eq!(
        parse_exit(&["--elapsed-ms=7000"])?.elapsed_since_launch_ms,
        7_000
    );
    assert_eq!(
        parse_exit(&["--elapsed-ms", "0"])?.elapsed_since_launch_ms,
        0
    );
    Ok(())
}

#[test]
fn the_elapsed_flag_is_validated() -> TestResult {
    assert_eq!(
        parse_exit(&["--elapsed-ms", &MAX_ELAPSED_MS.to_string()])?.elapsed_since_launch_ms,
        MAX_ELAPSED_MS
    );
    for bad in [
        "-5",
        "abc",
        "1.5",
        "",
        "99999999999999999999999",
        &(MAX_ELAPSED_MS + 1).to_string(),
    ] {
        let error = err_of(Cli::parse(exit_argv(&["--elapsed-ms", bad])))?;
        assert!(
            matches!(
                error,
                CliError::InvalidValue {
                    flag: "--elapsed-ms",
                    ..
                }
            ),
            "{bad:?}: {error:?}"
        );
    }
    assert_eq!(
        err_of(Cli::parse(exit_argv(&["--elapsed-ms"])))?,
        CliError::MissingValue {
            flag: "--elapsed-ms"
        }
    );
    assert_eq!(
        err_of(Cli::parse(exit_argv(&[
            "--elapsed-ms",
            "1",
            "--elapsed-ms=2"
        ])))?,
        CliError::DuplicateFlag {
            flag: "--elapsed-ms"
        }
    );
    // It belongs to `exit` only.
    assert_eq!(
        err_of(Cli::parse(args(&["check", "--elapsed-ms", "5"])))?,
        CliError::UnknownArgument
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn genesis_is_the_session_start_moved_back_by_the_elapsed_time() -> TestResult {
    let started = tokio::time::Instant::now();
    // Let the clock move so a subtraction has room.
    tokio::time::sleep(Duration::from_secs(60)).await;
    let started = started.max(tokio::time::Instant::now());

    let mut args = exit_args();
    assert_eq!(
        args.genesis(started),
        started,
        "no flag: the window starts now"
    );

    args.elapsed_since_launch_ms = 3_500;
    let genesis = args.genesis(started);
    assert_eq!(
        started.duration_since(genesis),
        Duration::from_millis(3_500)
    );

    // The time the setup takes counts against the window, as it does in reality.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        tokio::time::Instant::now().duration_since(genesis),
        Duration::from_millis(5_500)
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_absurd_elapsed_time_is_clamped_and_never_panics() -> TestResult {
    let started = tokio::time::Instant::now();
    let mut args = exit_args();
    args.elapsed_since_launch_ms = u64::MAX;
    let genesis = args.genesis(started);
    // One hour back, or `started` itself when the clock cannot go back that far.
    let back = started.duration_since(genesis);
    assert!(
        back == Duration::from_millis(MAX_ELAPSED_MS) || back == Duration::ZERO,
        "{back:?}"
    );
    Ok(())
}

// =====================================================================================================
// #46 - the barrier is crossed before any network I/O
// =====================================================================================================

fn production_with_stream() -> Result<ProductionSettings, Box<dyn Error>> {
    Ok(ProductionSettings::from_raw(RawProductionSettings {
        yellowstone_endpoint: Some("https://grpc.example.com".to_owned()),
        yellowstone_x_token: Some(SecretString::new("abc123token".to_owned())),
        jito_tip_stream_url: None,
    })?)
}

#[tokio::test]
async fn a_live_session_that_cannot_arm_touches_no_network() -> TestResult {
    // A loopback listener plays the RPC node. Nothing may connect to it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();

    let dir = scratch("order")?;
    // The state file does not exist: arming must fail.
    let mut vars = armed_vars(&dir, Some(&dir.join("guards.state")))?;
    vars.insert("RPC_URL".to_owned(), format!("http://127.0.0.1:{port}"));
    let mut config = Config::from_vars(vars)?;
    config.production = production_with_stream()?;

    let (_trigger, shutdown) = shutdown_channel();
    let error = err_of(run_exit_command(config, exit_args(), Mode::Live, shutdown).await)?;
    assert!(
        matches!(
            error,
            LauncherError::Live(LiveError::GuardState(StateFileError::Missing))
        ),
        "{error:?}"
    );
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("the session connected to the RPC node before arming".into()),
    }
    Ok(())
}

#[tokio::test]
async fn a_live_session_with_a_dry_environment_is_refused_by_the_barrier_first() -> TestResult {
    let dir = scratch("order-dry")?;
    let mut vars = armed_vars(&dir, Some(&dir.join("guards.state")))?;
    vars.insert("DRY_RUN".to_owned(), "true".to_owned());
    let mut config = Config::from_vars(vars)?;
    config.production = production_with_stream()?;
    let (_trigger, shutdown) = shutdown_channel();
    let error = err_of(run_exit_command(config, exit_args(), Mode::Live, shutdown).await)?;
    assert!(
        matches!(error, LauncherError::Live(LiveError::DryRunEnabled)),
        "{error:?}"
    );
    Ok(())
}

// =====================================================================================================
// #35 - `krypton init-state`
// =====================================================================================================

#[test]
fn the_init_state_command_parses_and_takes_no_arguments() -> TestResult {
    assert_eq!(
        Cli::parse(args(&["init-state"]))?.command,
        Command::InitState
    );
    for extra in [
        &["--live"][..],
        &["--mint", "x"],
        &["--confirm-live", "x"],
        &["--legacy-create"],
        &["--elapsed-ms", "5"],
    ] {
        let mut argv = args(&["init-state"]);
        argv.extend(args(extra));
        assert!(Cli::parse(argv).is_err(), "{extra:?}");
    }
    Ok(())
}

#[test]
fn init_state_creates_the_file_once_from_the_configuration() -> TestResult {
    let dir = scratch("init-cmd")?;
    let state = dir.join("guards.state");

    // No path configured.
    let config = Config::from_vars(armed_vars(&dir, None)?)?;
    let error = err_of(run_init_state(&config))?;
    assert!(
        matches!(error, LauncherError::Live(LiveError::GuardStatePathMissing)),
        "{error:?}"
    );

    let config = Config::from_vars(armed_vars(&dir, Some(&state))?)?;
    run_init_state(&config)?;
    let record = read_state(&state)?;
    assert_eq!(
        (record.committed_lamports, record.returned_lamports),
        (0, 0)
    );
    assert!(!record.halted);
    assert!(
        record.day > 19_000,
        "a plausible UTC day index, got {}",
        record.day
    );

    // A second run refuses and the file is untouched.
    write_state(&state, &record_with_spend(&record))?;
    let error = err_of(run_init_state(&config))?;
    assert!(
        matches!(
            error,
            LauncherError::Live(LiveError::GuardState(StateFileError::AlreadyExists))
        ),
        "{error:?}"
    );
    assert_eq!(read_state(&state)?.committed_lamports, 123);
    Ok(())
}

fn record_with_spend(base: &PersistedGuards) -> PersistedGuards {
    PersistedGuards {
        committed_lamports: 123,
        ..*base
    }
}

#[test]
fn an_initialised_state_arms_live_trading_right_away() -> TestResult {
    let dir = scratch("init-arm")?;
    let state = dir.join("guards.state");
    let config = Config::from_vars(armed_vars(&dir, Some(&state))?)?;
    run_init_state(&config)?;
    let executor = LiveExecutor::arm(&config)?;
    let snapshot = executor.guards().snapshot()?;
    assert_eq!(snapshot.committed_lamports, 0);
    assert!(!snapshot.halted);
    Ok(())
}

// =====================================================================================================
// #47 - the transport policy
// =====================================================================================================

#[test]
fn https_is_always_allowed_and_http_only_towards_loopback() {
    for host in [
        "rpc.example.com",
        "localhost",
        "127.0.0.1",
        "[::1]",
        "8.8.8.8",
    ] {
        assert!(endpoint_scheme_allowed("https", host), "https://{host}");
    }
    for host in ["localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "[::1]"] {
        assert!(endpoint_scheme_allowed("http", host), "http://{host}");
    }
    for host in [
        "rpc.example.com",
        "localhost.evil.com",
        "evil-localhost",
        "127.0.0.1.evil.com",
        "10.0.0.1",
        "192.168.1.5",
        "0.0.0.0",
        "[::2]",
        "",
    ] {
        assert!(!endpoint_scheme_allowed("http", host), "http://{host}");
    }
    for scheme in ["ftp", "ws", "wss", "file", "", "HTTP", "HTTPS"] {
        assert!(!endpoint_scheme_allowed(scheme, "localhost"), "{scheme}");
    }
}

#[test]
fn loopback_detection_is_exact() {
    assert!(is_loopback_host("localhost"));
    assert!(is_loopback_host("127.0.0.1"));
    assert!(is_loopback_host("[::1]"));
    assert!(!is_loopback_host("localhost."));
    assert!(!is_loopback_host("1.1.1.1"));
    assert!(!is_loopback_host("example.com"));
}
