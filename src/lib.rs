//! Krypton - Solana Atomic Launcher & Liquidity Engine.
//!
//! Phase 1 delivers the base architecture:
//! - [`errors`]: the unified [`errors::LauncherError`] hierarchy.
//! - [`security`]: zeroizing secret containers and the secure wallet loader.
//! - [`config`]: strongly-typed configuration loaded from the environment.
//! - [`parsing`]: dependency-free, allocation-light parsing helpers.
//! - [`logging`]: the `tracing` subscriber (`info` by default, nothing sensitive).
//!
//! Later phases add [`storage`] (Pinata/IPFS metadata), [`builder`] (atomic
//! `Create` + `Buy` transaction, lookup tables) and [`network`] (Jito Block
//! Engine client and tip). [`monitor`] (Phase 5) holds the real-time pieces:
//! the Geyser stream supervisor ([`monitor::geyser`]), bonding-curve decoding
//! ([`monitor::curve`]), the multi-tranche exit engine ([`monitor::exit`]) and the
//! Jito-backed sell submitter ([`monitor::submit`]).

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
pub mod logging;
pub mod monitor;
pub mod network;
pub mod parsing;
pub mod security;
pub mod storage;

pub use errors::{LauncherError, Result};
