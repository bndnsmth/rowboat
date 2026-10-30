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

`ai_performance` covers tabbed draft settings, reviewed-context-only submission,
automatic untouched-preview updates, preservation of edited/redacted context,
stale/cancelled response rejection, follow-up review and suggestion tabs that never
execute SQL. It uses disposable SQLite and fake/nonexistent executables. Services and CLI AI
tests exercise all three stream adapters, incompatible installations, explicit
sharing consent, connection-disable policy, bounded output, cancellation, private
temporary-file cleanup and database-environment removal. They never contact a
user's installed agent, model account or saved connection. See [AI assistance](AI.md)
for the separate real-account acceptance boundary.

Codex fixtures replay its nonfatal disabled-host startup notice followed by valid
advice, while decoder regressions still reject actual tool events, unknown item
errors and failed turns. Provider-switch GUI tests verify draft-only override
clearing and preservation when reselecting the same provider.

`ai_ux` checks the top-right global entry, welcome-without-SQL-or-sharing behavior,
sidebar docking/narrow overlay and pointer resize, plus shared dialog keyboard
resizing, maximize/restore,
window-size clamping and retained preference drafts. The existing connection-form
keyboard regression also traverses the shared resize/maximize footer and keeps
Tab/Shift+Tab within the modal. Passive advice-formatting tests keep incomplete
SQL fences non-actionable and render remote image/link text without activation.

`assistant` checks snapshot provenance, opt-in selected values, explicit trusted
consent/revocation, editable pinned previews, bounded conversation follow-ups,
new SQL tabs without execution, Ctrl+Enter input ownership and stale-response
rejection after a session change. Service tests verify all three trusted tool
configurations/decoders, opt-in gating, private exact-target read-only profiles
and tool-payload exclusion. The CLI `ai` regression launches fake agents for all
three providers; those agents use the actual built CLI against an explicitly
created disposable SQLite file, verify SELECT output and reject a write. No
installed agent/model account or user connection is used. The SQLite target matrix
covers open-existing/create-if-missing profiles with both absolute and relative
paths. Service tests check missing files are never created and TLS/SSH paths retain
their original working-directory resolution (not live TLS/SSH connections).

The sidebar regression also checks the 40px single-row Context toolbar,
mouse/keyboard re-expansion, sharing/draft/pinned-preview preservation, automatic
collapse on Send, and pending activity throughout partial responses and cancellation.
Focused performance review checks the same activity indicator's pending/completed
lifecycle. Native acceptance should compare waiting frames from a delayed fake
agent to verify motion, then check that the indicator disappears on completion.
With reduced motion enabled it should remain a static activity indicator instead.

Sidebar regressions also preserve edited/redacted previews across repeated Review
context clicks and closing/reopening, and preserve explicit sharing exclusions
instead of reapplying global defaults on every reopen.
Activation cases start with AI off or the connection blocked, cancel/confirm the
trust warning, then click the actual rendered Send button and verify a completed
fake-agent response. Separate cases cover advice-only activation without tools,
unchanged unrelated connection blocks and preferences becoming read-only during
confirmation. Enabling alone must not invoke the agent or execute SQL.
An isolated native Wayland check also started with AI off, confirmed investigation,
observed the checked toggle/enabled Send button, then clicked Send and received a
fake-agent response after a real read-only query returned `id = 7` from disposable SQLite.

Native AI acceptance should use an isolated `ROWBOAT_DATA_DIR`, a disposable
SQLite file and a fake agent. Inspect the welcome, context review, completed
advice/SQL card and resized views; verify no agent invocation occurs before
**Analyze query**, and no suggested SQL is executed. Do not restart an existing
user preview or copy its saved profiles into automated test fixtures.

For native sidebar acceptance, also exercise non-modal work, Ctrl+Enter Send,
context review, trusted consent, a fake-agent read-only CLI investigation, SQL
cards and resizing. After Send, expand **Context** to access the sharing/trust
controls again. Build both `rowboat` and `rowboat-cli` so investigation can
resolve `rowboatctl`. Slow virtual-keyboard typing enough for native event
processing; missing automated input is not evidence of a successful send.

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

`navigation_recovery` covers reconnect entry points, retained SQL/parameters,
transaction guards, unchanged saved profiles, recovery acknowledgement and no SQL
replay using disposable SQLite, including failed-attempt retry, missing-file and
read-only recovery-storage guards. Presentation-only fixtures cover shared paging,
collapse and loaded-object search. Its opt-in sidebar case uses only the explicit
MySQL/PostgreSQL fixtures, creates unique schemas, expands real routine catalogs,
opens/copies definitions, searches by keyboard and rejects stale scope replies.
A procedure body increments a guard table; inspection must leave that counter at
zero. Native visual acceptance remains separate; do not restart a user's running
preview to exercise these changes. A separate opt-in reconnect case verifies new
server sessions, memory-only credentials and MySQL's active database without
changing a saved default.

```sh
cargo test --locked --manifest-path crates/rowboat/editor-tests/Cargo.toml --target-dir target navigation_reconnect -- --test-threads=1
cargo test --locked --manifest-path crates/rowboat/editor-tests/Cargo.toml --target-dir target navigation_sidebar_live_routines_search_inspection_and_scope_isolation -- --ignored --test-threads=1
```

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
