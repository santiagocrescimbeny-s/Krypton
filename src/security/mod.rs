//! Memory-safety primitives: zeroizing secret containers and the secure wallet.

pub mod secret;
pub mod wallet;

pub use secret::SecretString;
pub use wallet::{SecretKeyBytes, SecureWallet};
