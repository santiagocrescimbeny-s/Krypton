//! Durable spending state (audit finding #35).
//!
//! Without persistence the daily drawdown and the halt latch live in RAM only:
//! a restart (a crash, a deploy, an operator's reflex after a halt) would hand
//! the next process a fresh day and a cleared tripwire. This module keeps the
//! four numbers that matter on disk:
//!
//! | Field | Meaning |
//! |-------|---------|
//! | `day` | UTC day index (`unix_seconds / 86_400`) the counters belong to |
//! | `committed` | lamports of orders that landed or may have landed, that day |
//! | `returned` | lamports returned by sells that landed, that day |
//! | `halted` | the drawdown tripwire has fired (survives day rollovers) |
//!
//! Orders still in flight (`reserved`) are deliberately **not** persisted: a
//! reservation is not spent money, and after a crash nobody can tell whether
//! the order landed. The exposure of that window is bounded by the
//! per-transaction cap times the orders in flight; the operator reconciles it
//! (see the README).
//!
//! # File format
//! Seven short text lines, canonical (the decoder re-encodes what it parsed and
//! requires byte equality), ended by an FNV-1a checksum so a truncated or
//! hand-edited file is rejected instead of trusted:
//!
//! ```text
//! krypton-guards-v1
//! day=20385
//! committed=1500000000
//! returned=0
//! halted=0
//! check=3f2a9c4e1b7d5a60
//! ```
//!
//! The checksum is an integrity check against accidents, not authentication: an
//! attacker who can write the file can write a valid one. The file is protected
//! by its `0600` mode and the directory's permissions.
//!
//! # Writing
//! Atomic: the record goes to `<file>.tmp` (created `0600`, never following a
//! symlink), is `fsync`ed, renamed over the real file, and the directory is
//! `fsync`ed. A crash leaves either the old record or the new one, never half
//! of each. All writes after startup happen on a dedicated background thread
//! ([`StateWriter`]) so the submit path never waits for the disk; writes
//! coalesce (only the latest record matters) and a failed write is retried.
//!
//! While a write is failing the guards refuse new **entries** (exits are never
//! blocked): spending that cannot be recorded is not allowed.
//!
//! # One owner
//! Whoever arms live trading takes an exclusive advisory lock on
//! `<file>.lock` ([`StateLock`]) and keeps it for the life of the
//! [`StateWriter`]. A second process (another session, or `check --live`
//! while a session runs) is refused with [`StateFileError::Locked`] instead of
//! overwriting the first one's books.
//!
//! # Fail closed
//! Live trading arms only if the file exists, is a regular non-symlink file with
//! mode `0600` (Unix), is small, decodes, passes its checksum and is not dated
//! in the future. The first run creates it explicitly with
//! `krypton init-state`; there is no implicit creation, so a wrong or deleted
//! path can never silently reset the day's accounting.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::errors::StateFileError;

/// First line of every state file.
pub const STATE_HEADER: &str = "krypton-guards-v1";
/// Largest state file accepted, in bytes. A valid record is under 150.
pub const MAX_STATE_FILE_BYTES: u64 = 1_024;
/// Pause before a failed background write is retried.
pub const WRITE_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// How long [`StateWriter::flush`] waits for the background thread.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// The persisted accounting. See the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PersistedGuards {
    /// UTC day index the counters belong to.
    pub day: u64,
    /// Spend of orders that landed or may have landed, that day.
    pub committed_lamports: u64,
    /// Proceeds of sells that landed, that day.
    pub returned_lamports: u64,
    /// The drawdown tripwire has fired.
    pub halted: bool,
}

/// FNV-1a, 64 bit.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The value of the next line, which must be `name=value`.
fn take_field<'a>(
    lines: &mut std::str::Split<'a, char>,
    name: &str,
) -> Result<&'a str, StateFileError> {
    lines
        .next()
        .and_then(|line| line.strip_prefix(name))
        .and_then(|rest| rest.strip_prefix('='))
        .ok_or(StateFileError::Corrupt {
            reason: "missing or reordered field",
        })
}

impl PersistedGuards {
    fn body(&self) -> String {
        format!(
            "{STATE_HEADER}\nday={}\ncommitted={}\nreturned={}\nhalted={}\n",
            self.day,
            self.committed_lamports,
            self.returned_lamports,
            u8::from(self.halted)
        )
    }

    /// The canonical text of the record, checksum included.
    #[must_use]
    pub fn encode(&self) -> String {
        let body = self.body();
        let check = fnv1a(body.as_bytes());
        format!("{body}check={check:016x}\n")
    }

    /// Parses a record.
    ///
    /// # Errors
    /// [`StateFileError::Corrupt`] with a static reason for anything that is
    /// not exactly what [`PersistedGuards::encode`] writes.
    pub fn decode(text: &str) -> Result<Self, StateFileError> {
        let corrupt = |reason| StateFileError::Corrupt { reason };
        let mut lines = text.split('\n');
        if lines.next() != Some(STATE_HEADER) {
            return Err(corrupt("unknown header"));
        }
        let day = take_field(&mut lines, "day")?;
        let committed = take_field(&mut lines, "committed")?;
        let returned = take_field(&mut lines, "returned")?;
        let halted = take_field(&mut lines, "halted")?;
        // The checksum's value is verified by the canonical comparison below.
        take_field(&mut lines, "check")?;
        let number = |text: &str| {
            text.parse::<u64>()
                .map_err(|_| corrupt("a counter is not a number"))
        };
        let record = Self {
            day: number(day)?,
            committed_lamports: number(committed)?,
            returned_lamports: number(returned)?,
            halted: match halted {
                "0" => false,
                "1" => true,
                _ => return Err(corrupt("halted must be 0 or 1")),
            },
        };
        // Canonical form: rejects "+5", leading zeros, a wrong checksum,
        // trailing lines and missing final newline in one comparison.
        if record.encode() == text {
            Ok(record)
        } else {
            Err(corrupt("checksum or format mismatch"))
        }
    }
}

// --- Reading ----------------------------------------------------------------------------

/// Reads and validates the state file.
///
/// # Errors
/// - [`StateFileError::Missing`] when the file does not exist;
/// - [`StateFileError::Unreadable`] for any other I/O failure;
/// - [`StateFileError::Insecure`] for a symlink, a non-regular file or (Unix)
///   a mode that is not `0600` or stricter;
/// - [`StateFileError::Corrupt`] for an oversized, non-UTF-8, malformed or
///   checksum-failing file.
pub fn read_state(path: &Path) -> Result<PersistedGuards, StateFileError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StateFileError::Missing)
        }
        Err(_) => {
            return Err(StateFileError::Unreadable {
                reason: "metadata unavailable",
            })
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(StateFileError::Insecure {
            reason: "is a symbolic link",
        });
    }
    if !metadata.is_file() {
        return Err(StateFileError::Insecure {
            reason: "is not a regular file",
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(StateFileError::Insecure {
                reason: "must be readable by its owner only (chmod 600)",
            });
        }
    }
    if metadata.len() > MAX_STATE_FILE_BYTES {
        return Err(StateFileError::Corrupt {
            reason: "unexpected size",
        });
    }
    let bytes = std::fs::read(path).map_err(|_| StateFileError::Unreadable {
        reason: "read failed",
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|_| StateFileError::Corrupt {
        reason: "not valid UTF-8",
    })?;
    PersistedGuards::decode(text)
}

// --- Writing ----------------------------------------------------------------------------

fn sibling_path(path: &Path, suffix: &str) -> Result<PathBuf, StateFileError> {
    let mut name = path
        .file_name()
        .ok_or(StateFileError::Write {
            reason: "the path has no file name",
        })?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// Exclusive advisory lock on `<file>.lock`, released when dropped.
#[derive(Debug)]
pub struct StateLock {
    _file: File,
}

impl StateLock {
    /// Takes the lock without waiting.
    ///
    /// # Errors
    /// [`StateFileError::Locked`] when another holder has it,
    /// [`StateFileError::Insecure`] when the lock path is a symlink,
    /// [`StateFileError::Write`] for any other failure.
    pub fn acquire(path: &Path) -> Result<Self, StateFileError> {
        let lock_path = sibling_path(path, ".lock")?;
        if let Ok(metadata) = std::fs::symlink_metadata(&lock_path) {
            if metadata.file_type().is_symlink() {
                return Err(StateFileError::Insecure {
                    reason: "the lock file is a symbolic link",
                });
            }
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .map_err(|_| StateFileError::Write {
                reason: "the lock file cannot be opened",
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(StateFileError::Locked),
            Err(std::fs::TryLockError::Error(_)) => Err(StateFileError::Write {
                reason: "the lock cannot be taken",
            }),
        }
    }
}

fn create_private_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    // `create_new` is O_EXCL: it fails instead of following a symlink.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StateFileError> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(directory)
        .and_then(|handle| handle.sync_all())
        .map_err(|_| StateFileError::Write {
            reason: "directory sync failed",
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StateFileError> {
    // Windows cannot open a directory for `fsync`; the rename is durable enough
    // there (`MoveFileExW` with write-through is not exposed by `std`).
    Ok(())
}

/// Writes `record` to `path` atomically: temporary file (`0600`), `fsync`,
/// rename, directory `fsync`.
///
/// A stale temporary file from a crashed earlier write is removed first.
///
/// # Errors
/// [`StateFileError::Write`] with a static reason.
pub fn write_state(path: &Path, record: &PersistedGuards) -> Result<(), StateFileError> {
    let fail = |reason| StateFileError::Write { reason };
    let temporary = sibling_path(path, ".tmp")?;
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(fail("stale temporary file cannot be removed")),
    }
    let mut file =
        create_private_new(&temporary).map_err(|_| fail("temporary file cannot be created"))?;
    let written = file
        .write_all(record.encode().as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err(fail("temporary file cannot be written"));
    }
    if std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err(fail("rename failed"));
    }
    sync_directory(path)
}

/// Creates the state file for the first time, holding an empty day.
///
/// # Errors
/// [`StateFileError::AlreadyExists`] when anything exists at `path` (it is never
/// overwritten: deleting it is a deliberate human act), or
/// [`StateFileError::Write`].
pub fn init_state(path: &Path, today: u64) -> Result<(), StateFileError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Err(StateFileError::AlreadyExists),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(StateFileError::Unreadable {
                reason: "metadata unavailable",
            })
        }
    }
    write_state(
        path,
        &PersistedGuards {
            day: today,
            ..PersistedGuards::default()
        },
    )
}

// --- Background writer --------------------------------------------------------------------

enum Message {
    State(PersistedGuards),
    Flush(Sender<bool>),
}

/// Writes the latest record to disk on a dedicated thread.
///
/// [`StateWriter::submit`] never blocks and never fails: it queues the record.
/// The thread keeps only the newest record, writes it atomically and retries
/// every [`WRITE_RETRY_INTERVAL`] until it succeeds. [`StateWriter::has_failed`]
/// is `true` while the newest record is not on disk because of an error.
#[derive(Debug)]
pub struct StateWriter {
    sender: Mutex<Option<Sender<Message>>>,
    failing: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    // Declared last: released after the thread has been joined in `Drop`.
    _lock: StateLock,
}

impl StateWriter {
    /// Starts the writer thread for `path`, which `lock` protects.
    ///
    /// # Errors
    /// [`StateFileError::Write`] when the thread cannot be spawned.
    pub fn spawn(path: PathBuf, lock: StateLock) -> Result<Self, StateFileError> {
        let (sender, receiver) = channel();
        let failing = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&failing);
        let thread = std::thread::Builder::new()
            .name("krypton-guard-state".to_owned())
            .spawn(move || writer_loop(&path, &receiver, &flag))
            .map_err(|_| StateFileError::Write {
                reason: "writer thread cannot be spawned",
            })?;
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            failing,
            thread: Mutex::new(Some(thread)),
            _lock: lock,
        })
    }

    fn send(&self, message: Message) -> bool {
        let Ok(guard) = self.sender.lock() else {
            return false;
        };
        guard
            .as_ref()
            .is_some_and(|sender| sender.send(message).is_ok())
    }

    /// Queues `record` for writing. A dead writer counts as a failure.
    pub fn submit(&self, record: PersistedGuards) {
        if !self.send(Message::State(record)) {
            self.failing.store(true, Ordering::SeqCst);
        }
    }

    /// Whether the newest record could not be written.
    #[must_use]
    pub fn has_failed(&self) -> bool {
        self.failing.load(Ordering::SeqCst)
    }

    /// Blocks until everything queued so far was attempted. Returns `true` when
    /// the newest record is on disk. Call it from a blocking context (for
    /// example `spawn_blocking`) at shutdown.
    #[must_use]
    pub fn flush(&self) -> bool {
        let (ack_sender, ack_receiver) = channel();
        if !self.send(Message::Flush(ack_sender)) {
            return false;
        }
        ack_receiver.recv_timeout(FLUSH_TIMEOUT).unwrap_or(false)
    }
}

impl Drop for StateWriter {
    fn drop(&mut self) {
        // Closing the channel lets the thread finish its last write and exit.
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        if let Ok(mut thread) = self.thread.lock() {
            if let Some(handle) = thread.take() {
                let _ = handle.join();
            }
        }
    }
}

fn writer_loop(path: &Path, receiver: &Receiver<Message>, failing: &AtomicBool) {
    let mut pending: Option<PersistedGuards> = None;
    let mut acks: Vec<Sender<bool>> = Vec::new();
    loop {
        let first = if pending.is_some() {
            // A write failed earlier: wait a little, then retry what is pending.
            match receiver.recv_timeout(WRITE_RETRY_INTERVAL) {
                Ok(message) => Some(message),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    // Last attempt before exiting.
                    if let Some(record) = pending {
                        let _ = write_state(path, &record);
                    }
                    return;
                }
            }
        } else {
            match receiver.recv() {
                Ok(message) => Some(message),
                Err(_) => return,
            }
        };
        let mut incoming = first;
        loop {
            match incoming {
                Some(Message::State(record)) => pending = Some(record),
                Some(Message::Flush(ack)) => acks.push(ack),
                None => {}
            }
            incoming = receiver.try_recv().ok();
            if incoming.is_none() {
                break;
            }
        }
        if let Some(record) = pending {
            match write_state(path, &record) {
                Ok(()) => {
                    pending = None;
                    failing.store(false, Ordering::SeqCst);
                }
                Err(_) => failing.store(true, Ordering::SeqCst),
            }
        }
        let ok = pending.is_none();
        for ack in acks.drain(..) {
            let _ = ack.send(ok);
        }
    }
}
