//! High-speed routing: the Jito Block Engine client and tip instructions
//! (Phase 4), the Yellowstone gRPC adapter (Phase 8) and the RPC blockhash source
//! of the production bridge ([`rpc`]; the `tonic` link in `grpc` is behind the
//! `grpc` feature).
//!
//! Phase 4. Pre-execution simulation (RPC `simulateTransaction`) and live
//! `Global`/`fee_config` validation (audit finding #21) build on this module.

#[cfg(feature = "grpc")]
pub mod grpc;
pub mod jito;
pub mod rpc;
pub mod yellowstone;

pub use jito::{
    assemble_launch_transaction, build_jito_tip_instruction, tip_account_for, BundleId,
    InflightStatus, JitoClient, JitoTip, LaunchBundleInputs, JITO_TIP_ACCOUNTS,
};
