//! Dry-run simulation (Phase 6).
//!
//! - [`market`]: a deterministic bonding-curve market (seeded trade flow, price
//!   impact, slot clock) that encodes its state as real `BondingCurve` account
//!   data, so the production decoder reads it;
//! - [`harness`]: the lifecycle runner. It drives the production builders, the
//!   stream supervisor and the exit manager against that market, with the
//!   Geyser stream and the Jito Block Engine replaced by in-memory stand-ins.
//!
//! Nothing in this module can reach the network: it imports no HTTP client, no
//! Block Engine client and no socket. See the safety notes in [`harness`].

pub mod harness;
pub mod market;
