#!/usr/bin/env bash
# Exercise extracted AND installed artifacts without reading any user profiles.
set -euo pipefail
[[ $# == 1 && "$1" == *.tar.gz ]] || { echo 'Usage: bash scripts/test-package.sh ARCHIVE.tar.gz' >&2; exit 2; }
archive="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
temporary="$(mktemp -d "${TMPDIR:-/tmp}/rowboat-package-test-XXXXXXXX")"
trap 'rm -rf -- "$temporary"' EXIT
tar -xzf "$archive" -C "$temporary"
package="$temporary/$(basename "$archive" .tar.gz)"
clean() {
  env -u ROWBOAT_URL -u KEEL_URL -u ROWBOAT_PASSWORD -u KEEL_PASSWORD \
    -u KEEL_DATA_DIR ROWBOAT_DATA_DIR="$temporary/state" "$@"
}
clean "$package/bin/rowboat" --version
clean "$package/bin/rowboatctl" --version
clean "$package/bin/rowboatctl" --database-file "$temporary/test.sqlite" --create query \
  --sql "CREATE TABLE portable(value TEXT); INSERT INTO portable VALUES ('portable-ok')" >/dev/null
clean "$package/bin/rowboatctl" --database-file "$temporary/test.sqlite" --read-only query \
  --sql 'SELECT value FROM portable' --format json-envelope | grep -q 'portable-ok'
# Spaces and desktop Exec metacharacters exercise the actual installer escaping.
prefix="$temporary/install space % dollar\$"
bash "$package/install.sh" --prefix "$prefix"
clean "$prefix/bin/rowboat" --version
clean "$prefix/bin/rowboatctl" --database-file "$temporary/test.sqlite" --read-only query \
  --sql 'SELECT value FROM portable' --format json-envelope | grep -q 'portable-ok'
if [[ "$(uname -s)" == Darwin ]]; then
  codesign --verify --strict "$prefix/Applications/Rowboat.app"
  clean "$prefix/Applications/Rowboat.app/Contents/MacOS/rowboat" --version
else
  if command -v desktop-file-validate >/dev/null; then
    desktop-file-validate "$prefix/share/applications/dev.bndnsmth.rowboat.desktop"
  fi
fi
echo 'Extracted and installed package smoke tests passed.'
