# Architecture

## Runtime and boundaries

Rowboat is a Rust 2024 Cargo workspace. `mise.toml` pins Rust 1.99.0; `Cargo.lock`
pins library resolution. GPUI Kit supplies the native GPUI application/runtime,
and gpui-omarchy supplies themed components and Omarchy theme following.

- **rowboat-core**: shared Database facade and internal adapters; SQLx PostgreSQL/
  MySQL sessions and an embedded SQLite native actor, type decoding, catalogs,
  SQL construction, browsing and transactional row changes.
- **rowboat-services**: persistence, Secret Service credential integration, OpenSSH
  forwarding, data transfer, SQL formatting, subprocess backup clients and bounded
  installed-agent orchestration. AI services receive bounded context values,
  never database handles; provider inference/authentication stays in external CLIs.
  Explicit trusted investigation may receive one active password-free profile
  for a temporary, read-only CLI target; this enables agent shell/read tools with
  instructions, not an enforced tool broker.
- **rowboat-admin**: typed schema operations and diffs, object catalogs, access
  management SQL, query completion/parameters/EXPLAIN, server metrics.
- **rowboat**: native application state and views. Database/subprocess work runs on
  a Tokio runtime, with completion messages delivered to GPUI.
- **rowboat-cli**: `rowboatctl` terminal entry point using the same libraries.
- **rowboat-tests**: independent PostgreSQL/MySQL/SQLite scenarios and source-boundary checks.

## Values and editing

Built-in engine adapters and scoped provider descriptors sit behind the shared
Database facade. See [ENGINE-ADAPTERS.md](ENGINE-ADAPTERS.md) for the ownership
boundary, extension procedure and SQLite scope, and [FEATURES.md](FEATURES.md)
for supported capabilities and limitations.

`Cell` explicitly separates SQL NULL, exact textual representations, and bytes.
This avoids routing large integers/decimals through floating-point JSON numbers.
Columns carry server type names. Data editing is a batch of inserts, updates and
deletes; updates/deletes must identify complete primary keys. Original values
provide optimistic concurrency checks. A conflicting row or failed statement
rolls the batch back.

Identifiers are quoted per engine. Browsing and editing validate target columns
against catalog metadata. Arbitrary SQL entered in the editor is intentionally
sent to the server as SQL; it is not treated as a filter value.

Connections are session-affine: explicit transaction operations must use the
same physical connection as queries. Network query cancellation uses a separate
server connection; SQLite interrupts its live native connection. Both preserve
operation-generation isolation and ownership through cleanup/finalization.

`rowboat_core::grid` provides checked rectangular selection, TSV clipboard codecs,
and atomic paste/fill/NULL-clear staging. Rowboat's lossless clipboard mode preserves
NULL, binary and escaped text; ordinary spreadsheet mode treats every pasted
field as text. The full original row and complete primary key remain attached
to edits, and compositing preserves other staged cells. No clipboard helper
executes SQL.

## State and secrets

The native window's `WorkspaceShell` retains one `Workbench` entity per open
connection workspace. Each entity owns its database/tunnel, receiver, session
generation, transactions, table result, staged edits and SQL tabs. Switching the
visible entity does not move these fields into another connection's state.
Completion routes include the workspace/session generation so reconnecting
invalidates old work. The shell aggregates close blockers across retained
workspaces rather than checking only the visible one.

Workspace AI is a non-modal right sidebar retained by each `Workbench`. The
UI-independent `rowboat_services::ai_context` DTO and the single
`assistant_context.rs` projection capture selection, editor revision, separate
result provenance, completeness and sharing policy. A message freezes context;
an edited preview is pinned until sent/discarded. Conversations and attachments
are memory-only and reset on connection/session/database identity changes.
Advice and trusted modes use the same bounded subprocess/stream lifecycle;
trusted tool events are accepted without rendering raw arguments/output.
Performance review retains its focused UI and can seed the sidebar without
sending a request. Context capture performs no database operations.

Profiles/history/snippets use one shared state model and serialized writer.
Recovery likewise has one lock/writer per window data directory, combining
workspace-owned SQL buffers and layout into one snapshot. Query tab identities
are allocated centrally. Closed workspace buffers may remain archived for
recovery. Recovery also retains bounded detached staged changes with
stale/ApplyStarted markers; revalidation is explicit. Live sessions, result sets
and passwords are not serialized. Native tests remain separate from these guarantees.

The application owns the Tokio runtime; workspace entities hold handles only.
Window close flushes queued metadata/recovery writes, explicitly stops both FIFO
writers, and waits for lock-release acknowledgements before removing the window.
Closing one workspace retains those shared writers for the other workspaces.
Runtime destruction itself does not synchronously wait for blocking workers on
the UI thread.

Connection profiles have no password field. State lives beneath `ROWBOAT_DATA_DIR`
or the native data directory (XDG on Linux, Application Support on macOS), with
the shared legacy fallback described in
[COMPATIBILITY.md](COMPATIBILITY.md). Atomic state writes use private permissions. Stored
query history and snippets remain local and may themselves contain sensitive
user-authored SQL.

Passwords are session memory or entries in the existing system keyring through
`secret-tool` on Linux or `security-framework`/Keychain on macOS. macOS CLI lookup
uses a private self-subprocess, terminated/reaped on cancellation; no password
is placed in process arguments. SQLite is bundled, and native TLS vendors
OpenSSL on Linux while using Apple's Security framework on macOS.
SSH uses the installed OpenSSH client with host verification and
an agent/key identity. No host tools are installed by the application.

SSH binds a private local Unix socket while retaining the original remote host
and port for TLS identity verification. The transport override is ephemeral and
excluded from profile serialization. SQLx uses that socket with native TLS on
both engines; cancellation connections inherit the same options. Tunnel guards
own the SSH child and socket directory. Explicit test known-hosts files never
update the user's trust store. Keyring integration tests use a separate D-Bus
session and disposable collection rather than querying the user's collection.

Backup clients can run inside already-cached Podman images. Credentials are
passed through private configuration files rather than command-line arguments.
Restore executes SQL from a user-selected dump; MySQL DDL can commit implicitly.

## Verification

`mise run test` runs unit checks. `mise run test:integration` starts isolated,
loopback-only rootless Podman databases and exercises live sessions. Integration
test fixtures use dedicated names and disposable data; they never discover or
reuse an existing development connection.

The native GUI must also be launched and inspected: compilation and core tests
do not establish that focus, input, layout, dialogs, or rendering work correctly.
See [TESTING.md](TESTING.md) for reproducible checks and native acceptance requirements.
