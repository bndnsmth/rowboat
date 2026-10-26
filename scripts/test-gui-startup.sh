#!/usr/bin/env bash
# CI-only native startup check in an isolated desktop/session. No user databases.
set -euo pipefail
[[ $# == 1 ]] || { echo 'Usage: bash scripts/test-gui-startup.sh GUI_BINARY' >&2; exit 2; }
temporary="$(mktemp -d "${TMPDIR:-/tmp}/rowboat-gui-test-XXXXXXXX")"
pid=''
cleanup() {
  if [[ -n "$pid" ]]; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  rm -rf -- "$temporary"
}
trap cleanup EXIT
env -u ROWBOAT_URL -u KEEL_URL -u ROWBOAT_PASSWORD -u KEEL_PASSWORD \
  -u KEEL_DATA_DIR ROWBOAT_DATA_DIR="$temporary/state" \
  "$1" > "$temporary/startup.log" 2>&1 &
pid=$!
sleep 5
if ! kill -0 "$pid" 2>/dev/null; then
  cat "$temporary/startup.log" >&2
  echo 'The GUI exited during native startup.' >&2
  exit 1
fi
echo 'Native GUI remained running through the startup smoke check.'
