# Launch contract with `nestor-runner run-once`

`run-once-v1.json` is a vendored copy of the fixtures of launch contract v1, which nestor owns (`docs/run-once-contract.md` and `contract/run-once-v1.json`, KCA-734 §2): `argv`, descriptors, credential, drain, report lines, exit codes.

`nestor.rev` pins the nestor commit the copy comes from, and the runner the contract test launches.

- `core/tests/contract_fixtures.rs` checks the launcher and the report reader against it.
- `core/examples/run_once_double.rs`, the double of `run-once` used by `core/tests/process.rs`, refuses any launch that breaks it.
- `core/tests/contract_run_once.rs` launches the real `nestor-runner run-once` built at `nestor.rev` (`scripts/nestor-runner.sh`) and fails if this copy is not nestor's at that commit, byte for byte (R2, KCA-737).

## Where the contract test runs

nestor is private and this repository is public: no CI of this repository reads nestor, and none asks for a token to do so. The contract test runs in **nestor's** CI instead (`.github/workflows/ci-contract.yml` there, KCA-750):

1. A pull request of nestor that touches `apps/runner/**`, `docs/run-once-contract.md` or `contract/**` starts it.
2. It builds `nestor-runner` from that pull request, then clones this repository, public, without a token, at the commit nestor pins in its `contract/runner-manager.rev`.
3. It runs `core/tests/contract_run_once.rs` with `NESTOR_RUNNER_UNPINNED=1`: the runner is the pull request's, not the one of `nestor.rev`; the fixtures must still equal this copy byte for byte.

Here, `cargo test` ignores the test; a developer who can read nestor runs it locally (`core/README.md`).

Any change of the contract:

1. in nestor, changes `contract` and the runner; its contract job then fails, the fixtures no longer being this copy;
2. here, one pull request copies `contract/run-once-v1.json` unchanged, pins its commit in `nestor.rev`, adapts the service, and passes the contract test locally;
3. in nestor, `contract/runner-manager.rev` pins that commit of this repository, and the contract job passes.
