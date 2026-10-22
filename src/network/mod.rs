//! High-speed routing: the Jito Block Engine client and tip instructions.
//!
//! Phase 4. Pre-execution simulation (RPC `simulateTransaction`) and live
//! `Global`/`fee_config` validation (audit finding #21) build on this module.

pub mod jito;

pub use jito::{
    assemble_launch_transaction, build_jito_tip_instruction, tip_account_for, BundleId,
    InflightStatus, JitoClient, JitoTip, LaunchBundleInputs, JITO_TIP_ACCOUNTS,
};
