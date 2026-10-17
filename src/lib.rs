//! Krypton - Solana Atomic Launcher & Liquidity Engine.
//!
//! Phase 1 delivers the base architecture:
//! - [`errors`]: the unified [`errors::LauncherError`] hierarchy.
//! - [`security`]: zeroizing secret containers and the secure wallet loader.
//! - [`config`]: strongly-typed configuration loaded from the environment.
//! - [`parsing`]: dependency-free, allocation-light parsing helpers.
//!
//! The remaining modules are empty scaffolding that later phases fill in.

#![forbid(unsafe_code)]
// Zero Panics Policy: enforced by the compiler for all non-test code.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

pub mod builder;
pub mod config;
pub mod errors;
pub mod monitor;
pub mod network;
pub mod parsing;
pub mod security;
pub mod storage;

pub use errors::{LauncherError, Result};
