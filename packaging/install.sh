#!/usr/bin/env bash
set -euo pipefail

source_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="${HOME}/.local"
custom_prefix=false
case "${1:-}" in
  --prefix)
    [[ $# == 2 && -n "$2" ]] || { echo 'Usage: bash install.sh [--prefix /absolute/path]' >&2; exit 2; }
    prefix="$2"
    custom_prefix=true
    ;;
  --help|-h)
    echo 'Usage: bash install.sh [--prefix /absolute/path]'
    echo 'Installs both binaries and desktop integration. Default: ~/.local'
    echo 'macOS: also installs Rowboat.app into ~/Applications (or PREFIX/Applications).'
    exit 0
    ;;
  '') [[ $# == 0 ]] || exit 2 ;;
  *) echo 'Usage: bash install.sh [--prefix /absolute/path]' >&2; exit 2 ;;
esac
[[ "$prefix" == /* && "$prefix" != *$'\n'* && "$prefix" != *$'\r'* ]] || {
  echo 'Prefix must be an absolute path without line breaks.' >&2; exit 2;
}
mkdir -p "$prefix"
prefix="$(cd "$prefix" && pwd)"
mkdir -p "$prefix/bin" "$prefix/share/licenses/rowboat" "$prefix/share/doc/rowboat/docs"
for binary in rowboat rowboatctl; do
  install -m755 "$source_dir/bin/$binary" "$prefix/bin/$binary"
done
install -m644 "$source_dir/LICENSE" "$prefix/share/licenses/rowboat/LICENSE"
install -m644 "$source_dir/README.md" "$prefix/share/doc/rowboat/README.md"
install -m644 "$source_dir"/docs/*.md "$prefix/share/doc/rowboat/docs/"

if [[ "$(uname -s)" == Darwin ]]; then
  applications="${HOME}/Applications"
  if "$custom_prefix"; then applications="$prefix/Applications"; fi
  mkdir -p "$applications"
  ditto "$source_dir/Rowboat.app" "$applications/Rowboat.app"
  printf 'Installed Rowboat into %s\nApp: %s/Rowboat.app\n' "$prefix" "$applications"
  exit 0
fi
mkdir -p "$prefix/share/icons/hicolor/scalable/apps"
install -m644 "$source_dir/share/icons/hicolor/scalable/apps/dev.bndnsmth.rowboat.svg" \
  "$prefix/share/icons/hicolor/scalable/apps/dev.bndnsmth.rowboat.svg"

# Desktop Exec has its own escaping in addition to desktop string escaping.
# env keeps the executable path in the argument list: GLib's initial executable
# lookup otherwise sees an unexpanded %% when the install prefix contains %.
executable="$prefix/bin/rowboat"
executable="${executable//\\/\\\\\\\\}"
executable="${executable//\"/\\\\\"}"
executable="${executable//\`/\\\\\`}"
executable="${executable//\$/\\\\\$}"
executable="${executable//%/%%}"
mkdir -p "$prefix/share/applications"
while IFS= read -r line; do
  case "$line" in
    Exec=*) printf 'Exec=/usr/bin/env "%s"\n' "$executable" ;;
    *) printf '%s\n' "$line" ;;
  esac
done < "$source_dir/share/applications/dev.bndnsmth.rowboat.desktop" \
  > "$prefix/share/applications/dev.bndnsmth.rowboat.desktop"
chmod 644 "$prefix/share/applications/dev.bndnsmth.rowboat.desktop"
if command -v update-desktop-database >/dev/null; then
  update-desktop-database "$prefix/share/applications"
fi
printf 'Installed Rowboat into %s\nLaunch: %s/bin/rowboat\n' "$prefix" "$prefix"
