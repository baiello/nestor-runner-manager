//! Shared core of Nestor Runner Manager.
//!
//! - [`admission`]: the ceiling of tasks run at the same time, and the pause.
//! - [`supervisor`]: runner lifecycles, pool sizing, anti-duplicate launches, crash recovery.
//! - [`persist`]: the state file kept across restarts (no secret in it).
//! - [`ipc`]: the local API over a Unix domain socket.
//! - [`load`]: machine description and load measurements (measurements, never limits).
//! - [`fake`]: launchers that start no real runner, for tests and the demo.
//! - [`demo`]: the simulated Nestor queue of `--simulate`.

pub mod admission;
pub mod demo;
pub mod fake;
pub mod ipc;
pub mod load;
pub mod model;
pub mod persist;
pub mod supervisor;
