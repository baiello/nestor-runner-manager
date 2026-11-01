# Launch contract with `nestor-runner run-once`

`run-once-v1.json` is a vendored copy of the fixtures of launch contract v1, which nestor owns (`docs/run-once-contract.md` and `contract/run-once-v1.json`, KCA-734 §2): `argv`, descriptors, credential, drain, report lines, exit codes.

`nestor.rev` pins the nestor commit the copy comes from, and the runner the contract test launches.

- `core/tests/contract_fixtures.rs` checks the launcher and the report reader against it.
- `core/examples/run_once_double.rs`, the double of `run-once` used by `core/tests/process.rs`, refuses any launch that breaks it.
- `core/tests/contract_run_once.rs` launches the real `nestor-runner run-once` built at `nestor.rev` (`scripts/nestor-runner.sh`) and fails if this copy is not nestor's at that commit, byte for byte (R2, KCA-737).

## Where the contract test runs

nestor is private and this repository is public: nothing of this repository reads nestor, and nothing holds a token to do so. The contract test runs by hand, on a developer's Mac, from either side; no hosted CI runs it:

- here, `scripts/verify-local.sh --contract` (or the command of `core/README.md`) builds the runner at `nestor.rev`, with the developer's own git credentials or a local clone of nestor in `NESTOR_SOURCE`;
- in nestor, `make contract-check` (`ci/contract-check.sh` there) builds `nestor-runner` from the nestor checkout, clones this repository, public, without a token, at the commit nestor pins in its `contract/runner-manager.rev`, and runs `core/tests/contract_run_once.rs` with `NESTOR_RUNNER_UNPINNED=1`: the runner is the checkout's, not the one of `nestor.rev`; the fixtures must still equal this copy byte for byte.

A plain `cargo test` ignores the test.

Any change of the contract:

1. in nestor, changes `contract` and the runner; `make contract-check` then fails, the fixtures no longer being this copy;
2. here, one pull request copies `contract/run-once-v1.json` unchanged, pins its commit in `nestor.rev`, adapts the service, and passes the contract test locally;
3. in nestor, `contract/runner-manager.rev` pins that commit of this repository, and `make contract-check` passes.

**Read ahead of the contract (A7, KCA-749).** The service also accepts, on a `finished` report of outcome `failed` only, a `failure` code (`[a-z_]{1,64}`), such as `claude_not_signed_in`, to show that Claude Code is not signed in on the machine. `run-once` at `nestor.rev` does not send it: it needs a change of the contract in nestor (fixtures and runner) before it shows anything with the real runner.
