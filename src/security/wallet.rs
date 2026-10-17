//! Secure keypair loading.
//!
//! Accepts the two formats Solana tooling produces:
//! - a Base58 string of the 64-byte keypair (Phantom / Solflare export), or
//! - a JSON byte array `[12,34,...]` (`solana-keygen` `id.json` content).
//!
//! The 64 raw bytes (32-byte seed || 32-byte public key) live in
//! [`SecretKeyBytes`], which implements `ZeroizeOnDrop`. All intermediate
//! buffers created while decoding are wrapped in `Zeroizing`.
//!
//! # Honest limits of zeroization
//! Rust may move or copy values (e.g. when passing `[u8; 32]` by value to an
//! external API), and the OS environment block that held the original string
//! cannot be wiped portably. This module wipes every buffer *it* owns and keeps
//! the number of copies to the minimum; it cannot make guarantees about copies
//! made inside third-party code or by the operating system (swap, core dumps).
//! A fresh [`Keypair`] produced by [`SecureWallet::keypair`] relies on the
//! `ed25519-dalek` signing key zeroizing itself on drop.

use std::fmt;

use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::errors::WalletError;
use crate::parsing::{decode_byte_array, ByteArrayError};

/// Length of the secret seed in bytes.
pub const SEED_BYTES: usize = 32;
/// Length of the public key in bytes.
pub const PUBLIC_KEY_BYTES: usize = 32;
/// Length of a full Solana keypair (seed || public key).
pub const KEYPAIR_BYTES: usize = SEED_BYTES + PUBLIC_KEY_BYTES;

/// Upper bound on accepted input characters (a Base58 keypair is ~88 chars, a
/// whitespace-heavy JSON array well under 1 KiB). Guards against pathological
/// input before any decoding work.
const MAX_SECRET_INPUT_CHARS: usize = 2048;

/// Decode scratch space. Larger than [`KEYPAIR_BYTES`] so an over-long Base58
/// string is reported as a length error instead of a buffer error.
const BASE58_SCRATCH_BYTES: usize = 128;

/// The raw 64 keypair bytes, wiped from memory when dropped.
///
/// Not `Clone`, not `Copy`; `Debug` is redacted.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretKeyBytes([u8; KEYPAIR_BYTES]);

impl SecretKeyBytes {
    /// Borrows the raw bytes. Callers must not copy them into non-zeroizing
    /// storage.
    #[must_use]
    pub fn expose(&self) -> &[u8; KEYPAIR_BYTES] {
        &self.0
    }
}

impl fmt::Debug for SecretKeyBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKeyBytes(<redacted>)")
    }
}

/// A loaded signing identity: zeroizing secret bytes plus the (public) address.
pub struct SecureWallet {
    secret: SecretKeyBytes,
    public_key: [u8; PUBLIC_KEY_BYTES],
}

// The only sensitive field, `secret`, zeroizes itself on drop; `public_key` is
// public by definition. This marker documents (and lets callers assert) that
// dropping a `SecureWallet` leaves no secret behind.
impl ZeroizeOnDrop for SecureWallet {}

impl SecureWallet {
    /// Parses a Base58 string or a JSON byte array.
    ///
    /// The format is detected from the first non-whitespace character: `[`
    /// selects JSON, anything else Base58.
    ///
    /// # Errors
    /// See [`WalletError`]. No error variant echoes any part of the input.
    pub fn from_secret_str(secret: &str) -> Result<Self, WalletError> {
        let trimmed = secret.trim();
        if trimmed.is_empty() {
            return Err(WalletError::EmptySecret);
        }
        if trimmed.len() > MAX_SECRET_INPUT_CHARS {
            return Err(WalletError::InputTooLarge);
        }

        let bytes = if trimmed.starts_with('[') {
            decode_json_keypair(trimmed)?
        } else {
            decode_base58_keypair(trimmed)?
        };
        Self::from_keypair_array(bytes)
    }

    /// Builds a wallet from exactly 64 raw keypair bytes.
    ///
    /// # Errors
    /// [`WalletError::InvalidLength`] if `bytes.len() != 64`, or
    /// [`WalletError::PublicKeyMismatch`] if the halves are inconsistent.
    pub fn from_keypair_bytes(bytes: &[u8]) -> Result<Self, WalletError> {
        match <[u8; KEYPAIR_BYTES]>::try_from(bytes) {
            Ok(array) => Self::from_keypair_array(Zeroizing::new(array)),
            Err(_) => Err(WalletError::InvalidLength {
                expected: KEYPAIR_BYTES,
                actual: bytes.len(),
            }),
        }
    }

    fn from_keypair_array(bytes: Zeroizing<[u8; KEYPAIR_BYTES]>) -> Result<Self, WalletError> {
        let raw: &[u8; KEYPAIR_BYTES] = &bytes;
        let seed = Zeroizing::new(copy_chunk::<SEED_BYTES>(raw, 0));
        let claimed_public = copy_chunk::<PUBLIC_KEY_BYTES>(raw, SEED_BYTES);

        // Re-derive the public key from the seed and require it to match the
        // second half of the input: catches truncated / corrupted / spliced keys.
        let derived = Keypair::new_from_array(*seed);
        let derived_bytes = Zeroizing::new(derived.to_bytes());
        let derived_public = copy_chunk::<PUBLIC_KEY_BYTES>(derived_bytes.as_slice(), SEED_BYTES);
        if derived_public != claimed_public {
            return Err(WalletError::PublicKeyMismatch);
        }

        Ok(Self {
            secret: SecretKeyBytes(*bytes),
            public_key: claimed_public,
        })
    }

    /// The wallet's public address.
    #[must_use]
    pub fn pubkey(&self) -> Pubkey {
        Pubkey::new_from_array(self.public_key)
    }

    /// The wallet's public key as raw bytes.
    #[must_use]
    pub fn pubkey_bytes(&self) -> [u8; PUBLIC_KEY_BYTES] {
        self.public_key
    }

    /// Borrows the zeroizing secret container.
    #[must_use]
    pub fn secret(&self) -> &SecretKeyBytes {
        &self.secret
    }

    /// Materializes a short-lived [`Keypair`] for signing.
    ///
    /// Prefer creating it right before signing and dropping it immediately
    /// afterwards, so the extra in-memory copy lives as briefly as possible.
    #[must_use]
    pub fn keypair(&self) -> Keypair {
        let seed = Zeroizing::new(copy_chunk::<SEED_BYTES>(self.secret.expose(), 0));
        Keypair::new_from_array(*seed)
    }
}

impl fmt::Debug for SecureWallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecureWallet")
            .field("pubkey", &self.pubkey().to_string())
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Copies `N` bytes of `src`, starting at `offset`, into a new array.
///
/// Infallible by construction (missing bytes stay zero), so callers need
/// neither slice indexing nor `unwrap`. Callers always pass inputs whose
/// length they have already validated.
fn copy_chunk<const N: usize>(src: &[u8], offset: usize) -> [u8; N] {
    let mut out = [0u8; N];
    for (dst, byte) in out.iter_mut().zip(src.iter().skip(offset)) {
        *dst = *byte;
    }
    out
}

fn decode_json_keypair(text: &str) -> Result<Zeroizing<[u8; KEYPAIR_BYTES]>, WalletError> {
    let mut buffer = Zeroizing::new([0u8; KEYPAIR_BYTES]);
    match decode_byte_array::<KEYPAIR_BYTES>(text, &mut buffer) {
        Ok(()) => Ok(buffer),
        Err(ByteArrayError::Malformed) => Err(WalletError::InvalidJsonArray),
        Err(ByteArrayError::WrongLength { actual }) => Err(WalletError::InvalidLength {
            expected: KEYPAIR_BYTES,
            actual,
        }),
    }
}

fn decode_base58_keypair(text: &str) -> Result<Zeroizing<[u8; KEYPAIR_BYTES]>, WalletError> {
    let mut scratch = Zeroizing::new([0u8; BASE58_SCRATCH_BYTES]);
    let decoded_len = match bs58::decode(text).onto(scratch.as_mut_slice()) {
        Ok(len) => len,
        Err(bs58::decode::Error::BufferTooSmall) => return Err(WalletError::InputTooLarge),
        Err(_) => return Err(WalletError::InvalidBase58),
    };

    if decoded_len != KEYPAIR_BYTES {
        return Err(WalletError::InvalidLength {
            expected: KEYPAIR_BYTES,
            actual: decoded_len,
        });
    }
    Ok(Zeroizing::new(copy_chunk::<KEYPAIR_BYTES>(
        scratch.as_slice(),
        0,
    )))
}
