# Launch contract with `nestor-runner run-once`

`run-once-v1.json` is a vendored copy of the fixtures of launch contract v1, which nestor owns (`docs/run-once-contract.md` and `contract/run-once-v1.json`, KCA-734 §2): `argv`, descriptors, credential, drain, report lines, exit codes.

- `core/tests/contract_fixtures.rs` checks the launcher and the report reader against it.
- `core/examples/run_once_double.rs`, the double of `run-once` used by `core/tests/process.rs`, refuses any launch that breaks it.

Any change of the contract bumps `contract` in nestor and is copied here unchanged. The nestor revision the copy comes from will be pinned in `nestor.rev`, and the cross-repository test that launches the real binary (R2, KCA-737) will fail if the two copies differ.
