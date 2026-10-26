#!/usr/bin/env bash
set -euo pipefail
umask 077
# Optional native smoke fixture. Uses existing tools and a private D-Bus/keyring;
# never enumerates or modifies the user's credential collection.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
for tool in dbus-run-session gnome-keyring-daemon secret-tool gdbus; do command -v "$tool" >/dev/null; done
if [[ "${1:-}" != --inside ]]; then
  [[ $# == 0 ]] || { echo 'Usage: bash scripts/test-keyring-gui.sh' >&2; exit 2; }
  mkdir -p "$root/.local"
  state="$(mktemp -d "$root/.local/gui-keyring-XXXXXXXX")"
  mkdir -m700 "$state"/{data,runtime,config,control,home,rowboat}
  # Preserve the compositor endpoint while isolating the Secret Service runtime.
  wayland="${WAYLAND_DISPLAY:-}"
  if [[ -n "$wayland" && "$wayland" != /* ]]; then wayland="${XDG_RUNTIME_DIR:?}/$wayland"; fi
  env -u KEEL_DATA_DIR ROWBOAT_DATA_DIR="$state/rowboat" ROWBOAT_URL='postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable' \
    "$root/target/debug/rowboatctl" profiles add-url --name 'Isolated keyring PostgreSQL'
  printf 'Native keyring fixture: %s\n' "$state"
  exec env -u DBUS_SESSION_BUS_ADDRESS -u GNOME_KEYRING_CONTROL -u GNOME_KEYRING_PID \
    -u ROWBOAT_PASSWORD -u ROWBOAT_URL -u KEEL_PASSWORD -u KEEL_URL -u KEEL_DATA_DIR \
    ROWBOAT_KEYRING_GUI_FIXTURE="$state" ROWBOAT_DATA_DIR="$state/rowboat" \
    XDG_DATA_HOME="$state/data" XDG_RUNTIME_DIR="$state/runtime" XDG_CONFIG_HOME="$state/config" \
    HOME="$state/home" WAYLAND_DISPLAY="$wayland" NO_AT_BRIDGE=1 \
    dbus-run-session -- bash "$root/scripts/test-keyring-gui.sh" --inside
fi
state="${ROWBOAT_KEYRING_GUI_FIXTURE:?}"
printf '%s' rowboat-disposable-keyring-unlock | gnome-keyring-daemon --foreground --components=secrets \
  --unlock --control-directory "$state/control" > "$state/daemon.log" 2>&1 &
daemon=$!
cleanup() { kill "$daemon" 2>/dev/null || true; wait "$daemon" 2>/dev/null || true; }
trap cleanup EXIT
ready=false
for ((attempt=0;attempt<100;attempt++)); do
  if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
    --method org.freedesktop.DBus.NameHasOwner org.freedesktop.secrets | grep -q true; then
    ready=true; break
  fi
  sleep .1
done
[[ "$ready" == true ]] || { echo 'Private keyring did not become ready.' >&2; exit 1; }
printf '%s\n' 'First window: open the saved connection, enter rowboat-test, enable Remember, and connect.'
"$root/target/debug/rowboat"
printf '%s\n' 'Restarting without ROWBOAT_PASSWORD; saved-profile startup must retrieve the isolated keyring entry.'
"$root/target/debug/rowboat" --connection 'Isolated keyring PostgreSQL'
printf '%s\n' 'Both windows closed. Fixture data remains under the printed .local directory for inspection/cleanup.'
