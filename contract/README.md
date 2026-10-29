# Launch contract with `nestor-runner run-once`

`run-once-v1.json` is a vendored copy of the fixtures of launch contract v1, which nestor owns (`docs/run-once-contract.md` and `contract/run-once-v1.json`, KCA-734 §2): `argv`, descriptors, credential, drain, report lines, exit codes.

`nestor.rev` pins the nestor commit the copy comes from, and the runner the contract test launches.

- `core/tests/contract_fixtures.rs` checks the launcher and the report reader against it.
- `core/examples/run_once_double.rs`, the double of `run-once` used by `core/tests/process.rs`, refuses any launch that breaks it.
- `core/tests/contract_run_once.rs` launches the real `nestor-runner run-once` built at `nestor.rev` (`scripts/nestor-runner.sh`) and fails if this copy is not nestor's at that commit, byte for byte (R2, KCA-737). It runs in the `contract` job of the CI.

Any change of the contract bumps `contract` in nestor and is copied here unchanged, with `nestor.rev`, in one pull request whose contract job passes.
