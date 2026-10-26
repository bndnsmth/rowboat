#!/usr/bin/env bash
set -euo pipefail
umask 022

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
platform="$(uname -s)"
case "$platform" in
  Linux|Darwin) ;;
  *) echo 'Packaging currently supports Linux and macOS.' >&2; exit 1 ;;
esac
target="$(rustc -vV | sed -n 's/^host: //p')"
[[ -n "$target" && "${CARGO_BUILD_TARGET:-$target}" == "$target" ]] || {
  echo 'Package on a native runner; cross-compiled binaries cannot be audited here.' >&2; exit 1;
}
binary_dir="${CARGO_TARGET_DIR:-$root/target}/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}release"
package_id="$(cargo pkgid --locked -p rowboat)"
version="${package_id##*#}"
version="${version##*@}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+[a-zA-Z0-9.+-]*$ ]] || {
  echo 'Cannot determine the Rowboat package version.' >&2; exit 1;
}
[[ -z "${ROWBOAT_RELEASE_TAG:-}" || "$ROWBOAT_RELEASE_TAG" == "v$version" ]] || {
  echo 'The release tag must equal v followed by the Cargo workspace version.' >&2; exit 1;
}
case "${1:-}" in
  '') cargo build --locked --release -p rowboat -p rowboat-cli ;;
  --no-build) [[ $# == 1 ]] || exit 2 ;;
  *) echo 'Usage: bash scripts/package.sh [--no-build]' >&2; exit 2 ;;
esac
name="rowboat-${version}-${target}"
dist="$root/.local/dist"
mkdir -p "$dist"
stage="$(mktemp -d "$dist/.package-XXXXXXXX")"
trap 'rm -rf -- "$stage"' EXIT
package="$stage/$name"
mkdir -p "$package/bin" "$package/share/applications" "$package/share/icons/hicolor/scalable/apps" "$package/docs"
for binary in rowboat rowboatctl; do
  install -m755 "$binary_dir/$binary" "$package/bin/$binary"
  expected="rowboatctl $version"
  if [[ "$binary" == rowboat ]]; then expected="Rowboat $version"; fi
  [[ "$("$package/bin/$binary" --version)" == "$expected" ]] || {
    echo "$binary does not match the workspace version; rebuild before packaging." >&2; exit 1;
  }
  if [[ "$platform" == Linux ]]; then
    ldd "$package/bin/$binary" | sed -E 's/ \(0x[[:xdigit:]]+\)//g' > "$package/docs/$binary-libraries.txt"
    if grep -Eq 'not found|lib(ssl|crypto)\.so' "$package/docs/$binary-libraries.txt"; then
      echo "Unresolved libraries or non-vendored OpenSSL in $binary" >&2; exit 1;
    fi
  else
    otool -L "$package/bin/$binary" | sed "1s|.*|$binary:|" > "$package/docs/$binary-libraries.txt"
    # No Homebrew/MacPorts/build-machine dylibs may escape into a release. There
    # are no bundled dylibs, so unresolved @rpath dependencies are errors too.
    if awk 'NR > 1 && $1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\// { bad = 1 } END { exit !bad }' \
      "$package/docs/$binary-libraries.txt"; then
      echo "Non-system macOS library dependency in $binary" >&2; exit 1;
    fi
  fi
done
install -m755 packaging/install.sh "$package/install.sh"
if [[ "$platform" == Linux ]]; then
  install -m644 packaging/dev.bndnsmth.rowboat.desktop "$package/share/applications/"
  install -m644 packaging/dev.bndnsmth.rowboat.svg "$package/share/icons/hicolor/scalable/apps/"
else
  app="$package/Rowboat.app"
  mkdir -p "$app/Contents/MacOS"
  mv "$package/bin/rowboat" "$app/Contents/MacOS/rowboat"
  # Keep only one GUI executable, and let mise launch it from its native bundle.
  # The relative symlink stays entirely inside the extracted installation.
  ln -s ../Rowboat.app/Contents/MacOS/rowboat "$package/bin/rowboat"
  sed -e "s/@VERSION@/${version%%[-+]*}/g" -e "s/@RELEASE@/$version/g" \
    packaging/macos/Info.plist > "$app/Contents/Info.plist"
  # Ad-hoc signing makes the bundle structurally valid on Apple Silicon. This
  # is NOT Developer ID signing or notarization; see docs/DISTRIBUTION.md.
  codesign --force --sign - "$app"
  codesign --verify --strict "$app"
fi
install -m644 LICENSE README.md "$package/"
install -m644 docs/*.md "$package/docs/"
install -m644 crates/rowboat/assets/themes/NOTICE.txt "$package/docs/THEME-NOTICES.txt"
if [[ "$platform" == Linux ]] && command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$package/share/applications/dev.bndnsmth.rowboat.desktop"
fi
epoch="${SOURCE_DATE_EPOCH:-0}"
[[ "$epoch" =~ ^[0-9]+$ ]] || { echo 'SOURCE_DATE_EPOCH must be an integer.' >&2; exit 1; }
# Normalize archive order/owners/times with the native GNU tar or BSD tar. Build
# reproducibility is separate; signing/toolchain inputs can change the binaries.
if [[ "$platform" == Linux ]]; then
  tar --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@$epoch" -C "$stage" -cf - "$name" | gzip -n > "$dist/$name.tar.gz.new"
else
  stamp="$(date -u -r "$epoch" '+%Y%m%d%H%M.%S')"
  TZ=UTC find "$package" -exec touch -h -t "$stamp" {} +
  (
    cd "$stage"
    find "$name" -print | LC_ALL=C sort | COPYFILE_DISABLE=1 tar --format=ustar \
      --uid 0 --gid 0 --uname root --gname root --no-recursion -T - -cf -
  ) | gzip -n > "$dist/$name.tar.gz.new"
fi
mv "$dist/$name.tar.gz.new" "$dist/$name.tar.gz"
(
  cd "$dist"
  if [[ "$platform" == Linux ]]; then
    sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
  else
    shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
  fi
)
printf 'Package: %s/%s.tar.gz\n' "$dist" "$name"
