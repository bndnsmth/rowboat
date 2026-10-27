# Testing

Use the Rust toolchain pinned in the root `mise.toml`. Tests must never discover
or reuse saved user connections. Database integration tests target only the
explicit disposable fixtures below.

## Local checks

```sh
mise install
mise run fmt
mise run check
mise run lint
mise run lint:distribution
mise run test
```

Normal workspace tests cover parsers, exact values, SQL builders, SQLite files,
persistence, cancellation and CLI subprocesses. Network tests are ignored unless
explicitly requested. A passing build is not evidence of correct native GUI
focus, rendering, dialogs or platform integration.

## Headless GUI regressions

The separate manifest enables GPUI's in-memory TestPlatform against actual
application modules without opening desktop windows:

```sh
cargo test --locked --manifest-path crates/rowboat/editor-tests/Cargo.toml --target-dir target -- --test-threads=1
cargo clippy --locked --manifest-path crates/rowboat/editor-tests/Cargo.toml --target-dir target --all-targets -- -D warnings
cargo fmt --manifest-path crates/rowboat/editor-tests/Cargo.toml -- --check
```

Its additional network cases also remain opt-in. TestPlatform success does not
replace native keyboard, rendering, connection and file-dialog acceptance.

`connection_polish` covers inline validation/focus and revealing advanced errors,
real disposable SQLite test success/failure, stale-feedback invalidation, original
host/live-schema context, and clicking/dragging the results scrollbars without SQL
execution. `connection_navigation` covers URL import and live PostgreSQL database
switching; `pass8_tests` retains the MySQL/namespace navigation regressions.
`pass20_connections` and `close_lifecycle_tests` cover the empty-main-window
Connections default, archived-only recovery, restored drafts/explicit startup SQL,
last-workspace close/move, satellite commands/rehosting, browser/form cancellation,
and guarded shutdown with writer acknowledgements. These use injected writers and
disposable recovery/SQLite data, never saved user connections.

## Disposable server fixtures

Requires rootless Podman and the cached `postgres:18` / `mysql:8.4` images. Scripts
check ownership before reusing/removing labeled containers. They never install
a host database or use a saved Rowboat connection.

| Fixture | PostgreSQL | MySQL | Other |
| --- | --- | --- | --- |
| Base | `127.0.0.1:15435` | `127.0.0.1:13309` | Database `rowboat` |
| TLS | `localhost:15433` | `localhost:13307` | Generated private CA/client/server files |
| SSH/TLS | `15434` | `13308` | SSH `127.0.0.1:12222` |

Fixture-only password: `rowboat-test`. PostgreSQL uses `postgres`; the base MySQL
account is `rowboat` (some administration tests require the disposable root user).
These values are public test data, not production credentials.

```sh
mise run test:integration
mise run db:down
```

`test:integration` starts the explicit fixtures and runs ignored workspace tests
serially. Inspect the scripts' prerequisites before running it. TLS/SSH fixtures
retain private generated files under ignored `.local/` directories for reuse;
do not commit them or publish raw logs.

## Credentials

The Linux Secret Service test creates a private D-Bus session and disposable
GNOME keyring, never the user's collection. It requires existing
`dbus-run-session`, `gnome-keyring-daemon`, `gdbus` and `secret-tool`:

```sh
cargo test --locked -p rowboat-services --test keyring_live -- --ignored
```

macOS helper-protocol/cancellation tests use disposable subprocesses without
reading the user's Keychain. Native private-Keychain roundtrip and authorization
UI acceptance remain release requirements.

## Distribution tests

```sh
mise run package
mise run package:test .local/dist/rowboat-VERSION-TARGET.tar.gz
ROWBOAT_DISTRIBUTION_ARCHIVE="$PWD/.local/dist/rowboat-VERSION-TARGET.tar.gz" mise run test:distribution
```

Archive tests execute extracted/temporarily installed commands and real SQLite
queries. The mise test uses an isolated config/cache/install and loopback-only
release API with wrong-platform candidates; it does not contact or publish a
GitHub release. Its explicit Cargo feature keeps it out of ordinary server tests.

CI also checks isolated native GUI startup. Before launch, manually exercise
connection lifecycle, SQL/transactions, edits/review, recovery, keyring, dialogs,
keyboard navigation, narrow windows and light/dark themes on supported machines.
See [DISTRIBUTION.md](DISTRIBUTION.md) for platform verification/signing limits.

## Public reports and assets

Never attach real profiles, passwords, TLS/SSH private keys, query history or
unreviewed SQL/screenshots. The launch demo is explicitly fictional and separate
from test fixtures. Generated reports, binaries, databases and launch ZIP bundles
belong under ignored `.local/` output, not source control.
