# core

Future shared core in Rust: the local service the interfaces of every operating system talk to. It will supervise the existing Nestor runner (without rewriting it), talk to Nestor, and keep each runner's credential in the operating system's secure storage. How the interfaces reach it (a command-line tool with JSON output or a local IPC) is still to be decided.

Nothing is here yet; the macOS app runs on simulated data until this core exists. See `docs/architecture.md`.
