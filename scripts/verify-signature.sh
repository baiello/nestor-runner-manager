#!/bin/sh
# Checks that a signed binary has the hardened runtime and no get-task-allow or runtime
# exception (A4). Exits non-zero otherwise.
set -eu
path="$1"
codesign --verify --strict "$path"
details="$(codesign -d --verbose=2 "$path" 2>&1)"
entitlements="$(codesign -d --entitlements - --xml "$path" 2>/dev/null || true)"
echo "$details" | grep -q "flags=.*runtime" || { echo "FAIL $path: no hardened runtime" >&2; exit 1; }
if echo "$entitlements" | grep -q "<key>com.apple.security.get-task-allow</key>\|<key>com.apple.security.cs\."; then
  echo "FAIL $path: debugging or runtime exception entitlement" >&2; exit 1
fi
echo "OK $path: hardened runtime, no get-task-allow"
