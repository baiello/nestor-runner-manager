#!/bin/sh
# The checks of this repository, run by hand on a developer's Mac before a push: no hosted CI runs
# them. Every pull request carries the result of this script on its head commit.
#
#   scripts/verify-local.sh               app and core
#   scripts/verify-local.sh --contract    also the contract test with the real `nestor-runner
#                                         run-once` (core/tests/contract_run_once.rs), which needs
#                                         read access to nestor (scripts/nestor-runner.sh), or
#                                         NESTOR_RUNNER_DIR set to a directory that script printed
#
# 1. app (apps/macos): swift build, swift test, arm64 release build, ad hoc signature check;
# 2. core: cargo fmt --check, cargo test (and with the dev-unsigned fallback), clippy -D warnings
#    (both), contract fixtures valid JSON, a release build refuses dev-unsigned (M1), ad hoc
#    signature check of the service;
# 3. with --contract only: the cross-repository contract test.
#
# The ad hoc signatures (`--identity -`) check the signing configuration only (hardened runtime, no
# get-task-allow); they install nothing, use no certificate and need no privilege.
set -eu

contract=0
for argument in "$@"; do
  case "$argument" in
    --contract) contract=1 ;;
    *) echo "usage: $0 [--contract]" >&2; exit 2 ;;
  esac
done

here="$(cd "$(dirname "$0")/.." && pwd)"
step() { printf '\n== %s\n' "$*"; }

echo "commit $(git -C "$here" rev-parse HEAD)"
if [ -n "$(git -C "$here" status --porcelain)" ]; then
  echo "warning: uncommitted changes or untracked files: the result is not that of the commit alone" >&2
fi

cd "$here/apps/macos"
step "app: swift build"
swift build
step "app: swift test"
swift test
step "app: release build (Apple Silicon)"
swift build -c release --arch arm64
app="$(swift build -c release --arch arm64 --show-bin-path)/NestorRunnerManager"
lipo -archs "$app"
step "app: signing configuration (ad hoc)"
"$here/scripts/sign-macos.sh" --identity - --kind app --identifier fr.kocoa.nestor.runner-manager "$app"

cd "$here/core"
step "core: cargo fmt --check"
cargo fmt --check
step "core: cargo test"
cargo test --locked
step "core: cargo test, development build with the unsigned fallback"
cargo test --locked --features dev-unsigned --test cli
step "core: clippy"
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features dev-unsigned -- -D warnings
step "core: contract fixtures are valid JSON"
python3 -m json.tool ../contract/run-once-v1.json >/dev/null
step "core: a release build cannot have the unsigned fallback (M1)"
if cargo build --release --locked --features dev-unsigned --bin nestor-runner-service; then
  echo "a release build with dev-unsigned compiled" >&2; exit 1
fi
"$here/scripts/release-service.sh"
step "core: signing configuration (ad hoc)"
"$here/scripts/sign-macos.sh" --identity - --kind service --identifier fr.kocoa.nestor.runner-service \
  target/release/nestor-runner-service

if [ "$contract" -eq 1 ]; then
  step "core: contract test with the real nestor-runner run-once"
  runner_dir="${NESTOR_RUNNER_DIR:-$("$here/scripts/nestor-runner.sh")}"
  NESTOR_RUNNER_DIR="$runner_dir" cargo test --locked --test contract_run_once -- --include-ignored
else
  step "core: contract test with the real nestor-runner run-once: not run (--contract)"
fi

printf '\nverify-local: OK at %s\n' "$(git -C "$here" rev-parse HEAD)"
