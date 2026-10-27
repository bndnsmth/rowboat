# Linux and macOS distribution

One product/version, two executables: `rowboat` is the native GUI and
`rowboatctl` is the independent CLI. Neither requires the other to be running.
Omarchy, Rust, Cargo and mise are **not runtime requirements**. Windows is not a
launch target.

## Release artifacts

`.github/workflows/distribution.yml` targets four native builds:

| Target | Build environment | Archive |
| --- | --- | --- |
| Linux x86-64 | Ubuntu 22.04 container on x86-64 runner | `rowboat-VERSION-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | Ubuntu 22.04 container on ARM64 runner | `rowboat-VERSION-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Intel | macOS 15 Intel runner | `rowboat-VERSION-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | macOS 15 ARM64 runner | `rowboat-VERSION-aarch64-apple-darwin.tar.gz` |

Each archive has a SHA-256 sidecar and a single enclosing directory containing:

```text
bin/rowboat
bin/rowboatctl
install.sh
docs/
LICENSE
README.md
share/...         # Linux desktop launcher and icon
Rowboat.app/...   # macOS Finder application bundle
```

SQL assets and palettes are embedded. No developer checkout, user profiles,
passwords, query history or private themes are packaged. Native library mappings
are recorded in `docs/*-libraries.txt`.

Release builds use Cargo's `strip = "symbols"` for both executables on Linux and
macOS. This removes ordinary symbol tables as well as debug information, reducing
installed sizes without removing symbols required for dynamic linking. macOS
bundle signing happens after the stripped executable is packaged. Release crash
backtraces can be less informative; the current workflow does not retain separate
matching debug-symbol artifacts.

## Install using mise

Install **both `rowboat` and `rowboatctl`** from the
[published GitHub releases](https://github.com/bndnsmth/rowboat/releases):

```sh
mise use -g github:bndnsmth/rowboat@0.1.0
rowboatctl --help
rowboatctl skills get core
rowboat
```

The GitHub backend picks the OS/CPU archive, verifies the published checksum,
strips its enclosing directory and exposes `bin/`. Both commands are installed
together. The CLI is independent of the GUI and includes an offline agent guide:
`rowboatctl skills get core --full`. No custom mise plugin or crates.io publication
is required.

For explicit archive selection, see `packaging/mise.example.toml`. Its
`strip_components = 1` and `bin_path = "bin"` also document the archive contract.
Mise does **not** run `install.sh`, create a Linux desktop launcher, or copy the
macOS app into Applications. Use the manual installer for those integrations.
Draft releases are deliberately invisible to mise.
Mise's default 24-hour minimum release age can also hide a newly published release
from `@latest`; an explicit version pin installs it without weakening that policy.

## Manual installation

Download the appropriate archive and its `.sha256` asset from GitHub Releases,
not GitHub's automatically generated source archives. Verify before extracting:

```sh
# Linux:
sha256sum --check rowboat-VERSION-TARGET.tar.gz.sha256
# macOS:
shasum -a 256 --check rowboat-VERSION-TARGET.tar.gz.sha256

tar -xzf rowboat-VERSION-TARGET.tar.gz
cd rowboat-VERSION-TARGET
bash install.sh
```

Both platforms install commands beneath `~/.local/bin` and documentation beneath
`~/.local/share/doc/rowboat`. Add that bin directory to PATH if needed; the
installer does not edit shell configuration or install system packages.

- **Linux:** desktop entry/icon go beneath `~/.local/share`. The desktop entry
  uses an absolute executable path, independent of the desktop's PATH.
- **macOS:** also installs `Rowboat.app` into `~/Applications`. Alternatively,
  copy the extracted app bundle to Applications yourself.
- **Custom prefix:** `bash install.sh --prefix /absolute/path` confines all files
  to that prefix, including macOS `PREFIX/Applications/Rowboat.app`. Custom Linux
  prefixes may require desktop search-path configuration.

Reinstalling updates Rowboat's installed files without deleting profiles or
history. State defaults to `$XDG_DATA_HOME/rowboat` (normally
`~/.local/share/rowboat`) on Linux, and `~/Library/Application Support/rowboat` on
macOS. `ROWBOAT_DATA_DIR` and the existing legacy fallback still apply.

## Compatibility and external tools

### Linux

CI builds both architectures against **glibc 2.35** (Ubuntu 22.04), not a rolling
Arch/Omarchy build host. This is a compatibility floor, not proof that every
newer distribution works. Alpine/musl is not supported by these archives.
Locally produced archives inherit the local machine's libc baseline instead.

SQLite is bundled and OpenSSL is vendored; the CLI does not require separately
installed database libraries or an OpenSSL shared library. The GUI still needs
a working X11/Wayland desktop, fonts, graphics drivers/Vulkan, XCB and xkbcommon.
On Debian/Ubuntu these include `libxcb1`, `libxcb-xkb1`, `libxkbcommon0`,
`libxkbcommon-x11-0`, `libfontconfig1`, fonts and an appropriate Vulkan driver.
Check `ldd bin/rowboat` and the included library report. This is **not** a fully
static binary or an AppImage.

Vendored security-library fixes require rebuilt Rowboat releases; they are not
picked up by replacing the receiver's system OpenSSL package.

Remembered passwords use the existing Secret Service integration (`secret-tool`
and an unlocked desktop keyring), retaining the legacy attributes. Session-only
passwords work without that optional helper. System CA certificates are needed
for normal server TLS verification.

### macOS

The build deployment target and app bundle minimum are **macOS 13.0**. CI runs on
macOS 15; earlier versions and real Apple/Intel desktop behavior still need
native acceptance before being advertised as verified. Packaging rejects
Homebrew/MacPorts/build-machine dylibs: only `/usr/lib` and `/System/Library`
dependencies are allowed.

Passwords use Apple's Keychain directly through the `security-framework` crate,
with no `secret-tool`, Homebrew or OpenSSL installation. They are not opted into
iCloud synchronization. GUI and CLI share the service/account identity; macOS
may request permission for each executable. A private subprocess makes CLI
Keychain lookup cancellable without placing passwords in argv or logs.

**Signing status:** bundles currently receive ad-hoc signatures only. Developer
ID signing, notarization and stapling are not configured. Downloaded bundles may
be blocked by Gatekeeper; ad-hoc signing must not be presented as notarization.
Do not remove quarantine attributes or disable security protections as part of
installation. A frictionless public macOS launch requires the signing work.

### Optional database tools on both platforms

SSH forwarding uses the installed OpenSSH client and existing trusted host keys.
Native PostgreSQL/MySQL backup/restore needs their client programs. Podman mode
requires Podman and cached client images (including a working Podman machine on
macOS); this container path is not yet verified on macOS. Database servers and
these optional tools are not bundled. SQLite backup needs no external client.

## Build and test locally

Use the project mise-managed toolchain on the native target OS:

```sh
mise install
mise run package
mise run package:test .local/dist/rowboat-VERSION-TARGET.tar.gz
mise run lint:distribution
```

The stock mise GitHub backend is also tested against a disposable loopback
release API, including wrong-platform asset candidates and a real SQLite query:

```sh
ROWBOAT_DISTRIBUTION_ARCHIVE="$PWD/.local/dist/rowboat-VERSION-TARGET.tar.gz" \
  mise run test:distribution
```

This uses isolated mise config/cache/install directories; it neither publishes
to GitHub nor reads or changes the user's mise configuration. The explicit
`distribution-tests` feature keeps this artifact-dependent test out of normal
workspace and database integration runs.

Linux build dependencies are enumerated in the workflow. macOS needs Xcode's
command-line tools/SDK. The package script uses native GNU/BSD tar, audits
dependencies, normalizes archive metadata and writes checksums. Cross-packaging
is rejected; use native runners. Nothing is installed or published by packaging.

## GitHub release process and remaining launch gates

Pull requests, pushes to `main`, and manual workflow runs build/test all four
targets and save Actions artifacts. The workflow runs formatting, all-target
checks, strict Clippy, normal workspace tests, script/workflow lint, extracted
and temporary-prefix installation tests, the real mise backend against a
loopback release API, and an isolated native GUI startup
check. External PostgreSQL/MySQL/SSH tests stay explicitly opt-in: CI never
discovers user connections. The startup check is not a full visual/input test.

For a release, update `[workspace.package].version` in the root `Cargo.toml` and
refresh the lockfiles, commit the changes, and push a matching `vVERSION` tag.
Both application crates inherit this version; GUI/CLI `--version`, archive names
and macOS bundle metadata derive from it. Packaging rejects a mismatched tag
before building and verifies both packaged binaries' version output. Only
after all four jobs pass does the workflow assemble a **draft** GitHub release
with all archives and checksums. A maintainer must review and publish it; no
automatic public release is configured. Build jobs have read-only permissions,
external actions are commit-pinned, and tool/build caches are not restored into
release builds.

Before public launch: run this workflow successfully, verify on clean supported
machines, exercise real GUI/keyring/SSH/TLS behavior on macOS, and complete Apple
Developer ID signing/notarization. Linux distro packages/AppImage, macOS DMG,
updaters, artifact attestations and Windows remain separate work. Review platform
acceptance and signing status before publishing a release.
