#!/bin/sh
# Signs the app, the command line tool or the service with the hardened runtime and the
# entitlements of signing/, then checks the result (contract v2 §6.B.5, A4, A6).
#
#   scripts/sign-macos.sh --identity "Developer ID Application: …" --team TEAMID \
#       --kind app|cli|service --identifier fr.kocoa.nestor.runner-manager[.cli] PATH
#
# Needs an Apple Developer certificate, which is not available yet (A6: to be validated by
# Brice). Without one, `--identity -` signs ad hoc for a local check of the configuration only:
# the Keychain entitlement is then left out (an ad hoc signature cannot carry it).
set -eu

identity="" team="" kind="" identifier="" path=""
while [ $# -gt 0 ]; do
  case "$1" in
    --identity) identity="$2"; shift 2 ;;
    --team) team="$2"; shift 2 ;;
    --kind) kind="$2"; shift 2 ;;
    --identifier) identifier="$2"; shift 2 ;;
    *) path="$1"; shift ;;
  esac
done
[ -n "$identity" ] && [ -n "$kind" ] && [ -n "$identifier" ] && [ -n "$path" ] || {
  echo "usage: $0 --identity ID --team TEAMID --kind app|cli|service --identifier ID PATH" >&2; exit 2; }

here="$(cd "$(dirname "$0")/.." && pwd)"
case "$kind" in
  app|cli) template="$here/signing/app.entitlements" ;;
  service) template="$here/signing/service.entitlements" ;;
  *) echo "unknown kind $kind" >&2; exit 2 ;;
esac
if grep -q "<key>com.apple.security.get-task-allow</key>\|<key>com.apple.security.cs\." "$template"; then
  echo "refusing: $template has a debugging or runtime exception entitlement" >&2; exit 1
fi

work="$(mktemp -d "${HOME}/.nrm-sign.XXXXXX")"
trap 'rm -rf "$work"' EXIT
if [ "$identity" = "-" ]; then
  # Ad hoc: no Keychain group (restricted entitlement), configuration check only.
  printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict/></plist>' > "$work/entitlements.plist"
else
  [ -n "$team" ] || { echo "--team is required with a real identity" >&2; exit 2; }
  # The service only accepts clients signed with a Developer ID certificate (F1).
  case "$identity" in
    "Developer ID Application:"*) ;;
    *) echo "refusing: sign with a \"Developer ID Application\" identity, not \"$identity\" (F1)" >&2; exit 1 ;;
  esac
  sed "s/TEAMID/$team/g" "$template" > "$work/entitlements.plist"
fi

codesign --force --options runtime --timestamp=none --identifier "$identifier" \
  --entitlements "$work/entitlements.plist" --sign "$identity" "$path"
"$here/scripts/verify-signature.sh" "$path"
