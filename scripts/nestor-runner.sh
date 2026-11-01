#!/bin/sh
# Builds the real `nestor-runner` at the nestor commit pinned in contract/nestor.rev, for the
# cross-repository contract test (core/tests/contract_run_once.rs, R2 of KCA-734 §5), and prints
# the directory the test reads through NESTOR_RUNNER_DIR:
#
#   <out>/nestor-runner        the binary, built with nestor's own toolchain and lock file
#   <out>/run-once-v1.json     nestor's fixtures of the launch contract, at that commit
#   <out>/rev                  the commit
#
#   scripts/nestor-runner.sh [<out>]          default out: core/target/nestor-runner/<rev>
#
# Source of nestor, in this order:
# - NESTOR_SOURCE=<checkout>: a local clone, which must be at the pinned commit, with apps/runner
#   and contract unchanged;
# - otherwise a fetch of the pinned commit only from NESTOR_REPOSITORY (default
#   https://github.com/baiello/nestor.git, a private repository), with git's own credentials:
#   those of a developer who can read nestor. It runs by hand only (scripts/verify-local.sh
#   --contract, core/README.md); no hosted CI runs it.
#
# An output directory whose `rev` already names the pinned commit is reused as it is.
set -eu

here="$(cd "$(dirname "$0")/.." && pwd)"
rev="$(tr -d ' \n' < "$here/contract/nestor.rev")"
case "$rev" in
  *[!0-9a-f]*|'') echo "contract/nestor.rev is not a commit: '$rev'" >&2; exit 1 ;;
esac
[ "${#rev}" -eq 40 ] || { echo "contract/nestor.rev must hold a full 40-digit commit: '$rev'" >&2; exit 1; }

out="${1:-$here/core/target/nestor-runner/$rev}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
if [ -x "$out/nestor-runner" ] && [ -f "$out/run-once-v1.json" ] && [ "$(cat "$out/rev" 2>/dev/null)" = "$rev" ]; then
  echo "$out"
  exit 0
fi

if [ -n "${NESTOR_SOURCE:-}" ]; then
  source="$(cd "$NESTOR_SOURCE" && pwd)"
  head="$(git -C "$source" rev-parse HEAD)"
  [ "$head" = "$rev" ] || { echo "NESTOR_SOURCE is at $head, not at the pinned $rev" >&2; exit 1; }
  if [ -n "$(git -C "$source" status --porcelain -- apps/runner contract docs/run-once-contract.md)" ]; then
    echo "NESTOR_SOURCE has changes in apps/runner, contract or the contract document" >&2; exit 1
  fi
else
  source="$out/source"
  rm -rf "$source"
  git init -q "$source"
  repository="${NESTOR_REPOSITORY:-https://github.com/baiello/nestor.git}"
  git -C "$source" fetch -q --depth 1 "$repository" "$rev" || {
    echo "cannot fetch nestor $rev from $repository: it is private; use git credentials that can read it, or NESTOR_SOURCE" >&2
    exit 1
  }
  git -C "$source" checkout -q --detach FETCH_HEAD
fi

# nestor's toolchain (apps/runner/rust-toolchain.toml) and lock file; a target directory of our
# own, so that no binary built elsewhere is ever picked up.
(cd "$source/apps/runner" && CARGO_TARGET_DIR="$out/target" cargo build --locked --bin nestor-runner)
cp "$out/target/debug/nestor-runner" "$out/nestor-runner"
cp "$source/contract/run-once-v1.json" "$out/run-once-v1.json"
printf '%s\n' "$rev" > "$out/rev"
echo "$out"
