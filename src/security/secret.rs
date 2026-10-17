//! A string that wipes itself from RAM when dropped and never prints itself.

use std::fmt;

use serde::{Deserialize, Deserializer};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Owned secret text (e.g. the `PRIVATE_KEY` environment value).
///
/// - Zeroized on drop (`ZeroizeOnDrop`).
/// - Not `Clone`, so copies cannot appear by accident.
/// - `Debug` is redacted; there is no `Display` and no `Serialize`.
/// - Deserializes straight from a string, *moving* it in (no intermediate copy).
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps an owned string. The caller's `String` is moved, not copied.
    #[must_use]
    pub fn new(secret: String) -> Self {
        Self(secret)
    }

    /// Borrows the secret text. Keep the borrow short-lived and never log it.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// `true` when the secret is empty or whitespace only.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}
