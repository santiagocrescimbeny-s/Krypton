//! Atomic transaction builder: PDA derivation, `Create` + `Buy` instructions and
//! `v0` `VersionedTransaction` assembly (Phase 3).
//!
//! - [`constants`]: program ids, seeds, discriminators and curve defaults.
//! - [`pda`]: deterministic address derivation (bonding curve, associated
//!   bonding curve, Metaplex metadata, mint authority, ...).
//! - [`math`]: exact constant-product math for the creator's initial buy.
//! - [`instructions`]: byte-exact instruction encoding.
//! - [`transaction`]: message compilation, size control and signing.

pub mod constants;
pub mod instructions;
pub mod math;
pub mod pda;
pub mod transaction;

pub use instructions::{CreateOptions, LaunchMetadata};
pub use math::{BuyPlan, CurveParams, DEFAULT_TOTAL_FEE_BPS};
pub use pda::{CreateVariant, LaunchAddresses};
pub use transaction::{CompiledLaunch, LaunchBuilder, LaunchParams, MintSigner};
