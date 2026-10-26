# Portable connections

The native connection manager and `rowboatctl profiles` share the same validated
profile format. A portable export contains connection metadata, never passwords.
Machine-local TLS certificate/key paths and SSH identity-file paths are omitted;
configure those paths on the receiving machine before connecting.

Network-only exports use `rowboat-connections` version 1; exports containing file
profiles use version 2. Imports also accept the legacy `keel-connections` version 1
envelope. Store loading preserves existing IDs; the
portable import flow still generates fresh IDs by default. Legacy environment
aliases, data-directory selection and the stable keyring namespace are documented
in [COMPATIBILITY.md](COMPATIBILITY.md). Compatibility handling does not modify saved endpoints or SQL.

## Native connection browser

Open **Connections** at the top right, or choose **New workspace** (the top-band
plus). Both open the same searchable, grouped browser. Search matches name, group,
engine and database/file target. Browsing or cancelling does not create a workspace,
connect, read passwords or execute SQL.

- **Open workspace** opens the selected profile; **Switch to workspace** activates
  its retained session, including a separate window. Existing SQL/results and
  transactions are preserved. An offline retained workspace is not reconnected.
- **New connection** and **Edit** open a temporary profile form. **Save** returns
  to the browser without connecting. **Save & Connect** opens a new owner or switches
  to an existing one; edits do not replace an existing physical session. Use
  **Current connection → Settings / reconnect** for session replacement.
- **Remove** requires confirmation and removes only saved configuration. Close the
  profile's retained workspace/form first. Database files, SQL recovery and history
  are retained. Network profiles offer optional system-keyring password deletion;
  if deletion fails, the saved profile stays. SQLite needs no keyring operation.
- **SQL draft workspace** explicitly opens or switches to the disconnected draft.
- **Import… / Export…** use the portable format below.

Saved connections are application-level; the sidebar is for the active database,
SQL editor, saved queries and history.

## Native connection checks and recovery

**Test connection** checks the current draft using a separate temporary connection.
It leaves saved profiles/passwords and the active workspace unchanged, and never
executes editor SQL. Tests are bounded to 20 seconds and are cancelled when the form
closes or its engine changes. SQLite tests open existing files only, even when the
draft selects **Create if missing**.

**Save & Connect** explicitly saves the draft before opening its workspace. Failed
handshakes expose connection recovery/settings. Clicking the selected engine leaves its fields intact;
switching engines and back restores that engine's in-memory draft.

If a physical session is unavailable and its transaction state cannot be verified,
**Connection → Settings / reconnect** offers **Save & Replace session**. Explicitly
acknowledge that previous outcomes may be unknown. Rowboat rechecks the old session,
blocks replacement if a live transaction is confirmed, and requires a durable SQL/
staged-evidence checkpoint before retirement. Session-bound edit proofs become
detached stale evidence; they cannot silently apply to the replacement. SQL and
parameters stay open in memory, and no SQL is replayed. Parameters are not written
to recovery files. Healthy active transactions still require Commit or Rollback.

## Terminal workflow

```sh
# List exact IDs and names.
target/release/rowboatctl connections

# Export all profiles, or select one with --profile <ID-or-unique-name>.
target/release/rowboatctl profiles export --output connections.json

# Validate and inspect an import without changing saved profiles.
target/release/rowboatctl profiles import connections.json --preview
target/release/rowboatctl profiles import connections.json

# Save a URL supplied on stdin; any URL password is discarded.
printf '%s' 'postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable' |
  target/release/rowboatctl profiles add-url --stdin --name 'Local PostgreSQL'

# Produce a password-free URL from a saved profile.
target/release/rowboatctl profiles url 'Local PostgreSQL'
```

`profiles add-url` can also read `ROWBOAT_URL`; `--read-only` marks the saved
profile read-only. `ROWBOAT_PASSWORD` supplies a connection password at execution
time. Remembering a password is an explicit GUI keyring operation.

Imports receive fresh profile IDs and preserve existing profiles. Duplicate
display names are permitted, so CLI lookup requires an exact ID when a name is
ambiguous. Exports refuse to overwrite existing destination files. Malformed
imports are rejected before updating saved state.

CLI profile mutations acquire the same lock as native SQL-buffer recovery.
When that data directory is open in the GUI, use the GUI import flow or close
the window first. This prevents the running application's cached state from
overwriting a CLI import. `ROWBOAT_DATA_DIR` can select a separate profile store.

A URL represents a direct database endpoint; the versioned JSON profile format
is used for richer connection settings. Importing a profile does not connect to
its endpoint or replace credentials for an existing profile.

URL generation rejects SSH and read-only profiles rather than producing a URL
that silently drops those settings. URLs retain explicit TLS certificate paths;
portable JSON is the default for sharing metadata between machines.

## Queries for scripts and agents

Discover the embedded CLI guide without opening a connection:

```sh
target/release/rowboatctl skills list
target/release/rowboatctl skills get core
target/release/rowboatctl skills get core --full
```

The agent-oriented result format is `query --format json-envelope`; use an explicit
deadline such as `--timeout 30s`. It preserves column ordinals and reports partial
results, truncation and structured errors. See [AGENT-CLI.md](AGENT-CLI.md) for the
versioned contract, timeout scope and exit codes. The existing `--format json`
examples below retain their original flat-object shape.

```sh
# A saved GUI connection; credentials use its keyring entry or ROWBOAT_PASSWORD.
target/release/rowboatctl --connection 'Development' --read-only \
  query --sql 'SELECT id, name FROM users LIMIT 10' --format json

# Network URL, or set ROWBOAT_URL; avoid embedding passwords in shell history.
target/release/rowboatctl --url 'postgres://app@localhost:5432/app' \
  query --file ./report.sql --format json

# SQLite, without networking or credentials.
target/release/rowboatctl --database-file ./project.sqlite --read-only \
  query --sql 'SELECT * FROM users LIMIT 10' --format json

# Progressive JSON events, bounded batches; check terminal Complete and exit code.
target/release/rowboatctl --connection 'Development' --read-only \
  query-stream --sql 'SELECT * FROM orders' --limit 50000
```

`query --format json` writes an array of row objects for one result set; multiple
sets use an array of those arrays. Text/numeric cells are strings (preserving numeric
precision), SQL NULL is JSON null, and blobs use `{"$binary":"hex"}`. Alias duplicate
column names when consuming object-shaped JSON. Default retained rows are 1,000
per result (`--limit`). Timing and diagnostics go to stderr, JSON goes to stdout.
`query-stream` emits typed result/batch/completion events rather than that flat
array. SQL is executed as supplied; row limits do not prevent SQL side effects.

## SQLite files

Choose **SQLite** in the connection form, select a path and **Open existing**.
A missing file is an error. **Create if missing** is an explicit separate choice;
it does not truncate an existing database. Read-only opens an existing file and
enforces write protection in the native connection. No password, TLS, SSH or
keyring access is needed.

```sh
# Open an existing file (paths with spaces work).
target/release/rowboatctl --database-file './project data.sqlite' ping
target/release/rowboatctl --database-file './project data.sqlite' tables

# Explicit creation; read-only and create cannot be combined.
target/release/rowboatctl --database-file './new.sqlite' --create ping
target/release/rowboatctl --database-file './project data.sqlite' --read-only tables

# Save a profile without opening or creating the database.
target/release/rowboatctl profiles add-file './project data.sqlite' --name 'Project SQLite'
```

File targets are represented by `file: {path, mode}` in portable v2 documents;
the essential database path is retained during export. Optional TLS/SSH path
omission still applies only to network profiles. SQLite URL and in-memory main
profiles are unsupported; use a filesystem path. `main`, `temp` and attached
database names are explorer scopes, not MySQL-style database switches.

## SSH and TLS identity

SSH connections retain the remote database host for `verify-full`, while the
transport uses a private local Unix socket. The endpoint is never serialized
into profiles. OpenSSH uses existing host trust and key/agent authentication;
it does not accept new host keys automatically.

The CLI can choose an explicit read-only trust file with
`--ssh-known-hosts /path/to/known_hosts` or `ROWBOAT_SSH_KNOWN_HOSTS`. This selects
that file instead of global known-hosts files. Live comparisons can independently
select a source trust file through `--source-ssh-known-hosts` or
`ROWBOAT_SOURCE_SSH_KNOWN_HOSTS`. Neither command modifies trust files.

Keyring password storage preserves Unicode and trailing newlines exactly. Linux
uses Secret Service through `secret-tool`, retaining the existing credential
attributes. macOS uses the native Security framework Keychain, with the service
`dev.bndnsmth.rowboat` and connection ID as account; credentials are not opted
into iCloud synchronization. Either backend rejects NUL and passwords of 8192
UTF-8 bytes or more rather than silently truncating them. Passwords are never
placed in process arguments, profile JSON or logs. CLI macOS lookups run in an
owned, cancellable subprocess so Keychain authorization cannot bypass setup
deadlines. Session-only passwords remain available if the keyring is unavailable.

For an isolated native keyring smoke test, build the debug GUI/CLI, start the
Podman PostgreSQL fixture, then run `bash scripts/test-keyring-gui.sh`. It opens
two successive windows under a private D-Bus/keyring: save a fixture password in
the first and verify startup retrieves it in the second. It requires existing
Secret Service tools and retains its disposable data under `.local/gui-keyring-*`
for inspection. No user's credential collection is queried.
