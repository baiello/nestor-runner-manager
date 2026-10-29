//! Shared core of Nestor Runner Manager.
//!
//! - [`admission`]: when a "one job" runner may be launched: mode, ceiling, scopes.
//! - [`supervisor`]: runner lifecycles, launches, drains, crash recovery, adoption after a restart.
//! - [`process`]: the launcher of real runner processes (`argv`, credential on a pipe).
//! - [`association`]: device authorization, machine key, disassociation and revocation.
//! - [`nestor`]: what the service needs from the Nestor API, and a fake server.
//! - [`secrets`]: secrets, the Keychain and the machine key.
//! - [`persist`]: the state file kept across restarts (no secret in it), validated when read.
//! - [`ipc`]: the local API over a Unix domain socket, with peer verification.
//! - [`mask`]: masking of secrets in everything the service logs.
//! - [`load`]: machine description and load measurements (measurements, never limits).
//! - [`sys`]: process identity, socket peers, code signing, keys (operating system calls).
//! - [`fake`]: launchers that start no real runner, for tests and the demo.
//! - [`demo`]: the simulated Nestor of `--simulate`.

pub mod admission;
pub mod association;
pub mod demo;
pub mod fake;
pub mod ipc;
pub mod load;
pub mod mask;
pub mod model;
pub mod nestor;
pub mod persist;
pub mod process;
pub mod secrets;
pub mod supervisor;
pub mod sys;
