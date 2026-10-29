# core

Future shared core in Rust: the local service the interfaces of every operating system talk to. It will supervise the existing Nestor runner (without rewriting it), talk to Nestor, keep each runner's credential in the operating system's secure storage and expose an authenticated local IPC.

Nothing is here yet; the macOS app runs on simulated data until this core exists. See `docs/architecture.md`.
