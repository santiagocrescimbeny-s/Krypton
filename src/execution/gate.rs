//! The dry-to-live barrier (Phase 7).
//!
//! Live trading is armed only when **every** one of these holds, checked in this
//! order, and the first failure is returned as a [`LiveError`] at startup:
//!
//! 1. `LIVE_TRADING=true`;
//! 2. `DRY_RUN=false` (setting both is deliberate: `DRY_RUN` defaults to `true`
//!    and a lone `LIVE_TRADING=true` is a contradiction, not a request);
//! 3. `LIVE_CONFIRM` equals the confirmation phrase;
//! 4. `MAX_SOL_PER_TX` and `DAILY_DRAWDOWN_SOL` are set;
//! 5. `LIVE_KEYPAIR_PATH` points to a regular file (not a symlink), small, and
//!    on Unix readable by its owner only (`0600` or stricter);
//! 6. the keypair in that file is **the configured wallet**, so a stale or
//!    wrong key file can never sign.
//!
//! Nothing here panics: a failed check is an error value, and the process
//! simply does not get a [`LiveExecutor`](super::live::LiveExecutor).

use std::path::Path;

use zeroize::Zeroizing;

use super::guards::GuardLimits;
use super::settings::LiveSettings;
use super::tip::TipPolicy;
use crate::errors::LiveError;
use crate::security::SecureWallet;

/// Largest keypair file accepted. A Solana CLI keypair is about 230 bytes.
pub const MAX_KEYPAIR_FILE_BYTES: u64 = 4_096;

/// The checked parameters live trading runs with.
#[derive(Debug, Clone, Copy)]
pub struct ArmedLive {
    /// Spending limits.
    pub limits: GuardLimits,
    /// Tip policy.
    pub tip_policy: TipPolicy,
}

/// Reads and validates a keypair file.
///
/// # Errors
/// [`LiveError::KeypairFile`] with a static reason; never the path or contents.
pub fn load_keypair_file(path: &Path) -> Result<SecureWallet, LiveError> {
    let reject = |reason| LiveError::KeypairFile { reason };
    let metadata = std::fs::symlink_metadata(path).map_err(|_| reject("cannot be read"))?;
    if metadata.file_type().is_symlink() {
        return Err(reject("is a symbolic link"));
    }
    if !metadata.is_file() {
        return Err(reject("is not a regular file"));
    }
    if metadata.len() == 0 || metadata.len() > MAX_KEYPAIR_FILE_BYTES {
        return Err(reject("has an unexpected size"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(reject("must be readable by its owner only (chmod 600)"));
        }
    }
    let bytes = Zeroizing::new(std::fs::read(path).map_err(|_| reject("cannot be read"))?);
    let text = std::str::from_utf8(&bytes).map_err(|_| reject("is not valid UTF-8"))?;
    SecureWallet::from_secret_str(text.trim()).map_err(|_| reject("is not a valid keypair"))
}

/// Runs the barrier. See the module documentation for the order of the checks.
///
/// Returns the keypair loaded from `LIVE_KEYPAIR_PATH` together with the checked
/// parameters.
///
/// # Errors
/// The first failed check as a [`LiveError`].
pub fn arm(
    dry_run: bool,
    settings: &LiveSettings,
    configured_wallet: &SecureWallet,
    slippage_bps: u16,
    tip_fallback_lamports: u64,
    tip_cap_lamports: u64,
) -> Result<(SecureWallet, ArmedLive), LiveError> {
    if !settings.live_trading {
        return Err(LiveError::NotArmed);
    }
    if dry_run {
        return Err(LiveError::DryRunEnabled);
    }
    if !settings.confirmed {
        return Err(LiveError::ConfirmationMissing);
    }
    let max_per_tx = settings
        .max_spend_per_tx_lamports
        .ok_or(LiveError::LimitMissing {
            name: super::settings::ENV_MAX_SOL_PER_TX,
        })?;
    let daily = settings
        .daily_drawdown_lamports
        .ok_or(LiveError::LimitMissing {
            name: super::settings::ENV_DAILY_DRAWDOWN_SOL,
        })?;
    let path = settings
        .keypair_path
        .as_deref()
        .ok_or(LiveError::KeypairPathMissing)?;
    let wallet = load_keypair_file(path)?;
    if wallet.pubkey() != configured_wallet.pubkey() {
        return Err(LiveError::KeypairMismatch);
    }
    let limits = GuardLimits::new(max_per_tx, daily, slippage_bps)?;
    let tip_policy = TipPolicy::new(tip_fallback_lamports, tip_cap_lamports)?;
    Ok((wallet, ArmedLive { limits, tip_policy }))
}
