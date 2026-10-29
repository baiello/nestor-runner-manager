#!/bin/sh
# Builds the release binary of the service, refusing the development-only unsigned fallback (M1):
# fails if the dev-unsigned feature is asked for or enabled by default, and checks that the
# binary it built rejects --allow-unsigned-clients as a usage error (exit 64).
#
#   scripts/release-service.sh [cargo build options…]
set -eu

here="$(cd "$(dirname "$0")/.." && pwd)"
for argument in "$@"; do
  case "$argument" in
    *dev-unsigned*|--all-features) echo "refusing: the dev-unsigned feature is for development only (M1)" >&2; exit 1 ;;
  esac
done
if grep -Eq '^default *=.*dev-unsigned' "$here/core/Cargo.toml"; then
  echo "refusing: dev-unsigned is a default feature in core/Cargo.toml (M1)" >&2; exit 1
fi

cd "$here/core"
cargo build --release --locked --bin nestor-runner-service "$@"
binary="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/nestor-runner-service"

status=0
"$binary" --allow-unsigned-clients --help >/dev/null 2>&1 || status=$?
if [ "$status" -ne 64 ]; then
  echo "FAIL $binary: --allow-unsigned-clients was not rejected (exit $status, expected 64)" >&2; exit 1
fi
echo "OK $binary: release build, --allow-unsigned-clients rejected"
