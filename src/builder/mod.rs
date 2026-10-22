//! Atomic transaction builder: PDA derivation, `Create` + `Buy` instructions and
//! `v0` `VersionedTransaction` assembly (Phase 3).
//!
//! - [`constants`]: program ids, seeds, discriminators and curve defaults.
//! - [`pda`]: deterministic address derivation (bonding curve, associated
//!   bonding curve, Metaplex metadata, mint authority, ...).
//! - [`math`]: exact constant-product math for the creator's initial buy and
//!   for the sells of the exit manager (Phase 5).
//! - [`instructions`]: byte-exact instruction encoding.
//! - [`transaction`]: message compilation, size control and signing;
//! - [`alt`]: Address Lookup Table instructions, decoding and the checked
//!   [`LaunchLookupTable`] handle;
//! - [`sell`]: `sell_v3` transactions for the exit manager (Phase 5).

pub mod alt;
pub mod constants;
pub mod instructions;
pub mod math;
pub mod pda;
pub mod sell;
pub mod transaction;

pub use alt::LaunchLookupTable;
pub use instructions::{CreateOptions, LaunchMetadata};
pub use math::{BuyPlan, CurveParams, SellPlan, DEFAULT_TOTAL_FEE_BPS};
pub use pda::{CreateVariant, LaunchAddresses};
pub use transaction::{CompiledLaunch, FundingSnapshot, LaunchBuilder, LaunchParams, MintSigner};
