# Application services

`rowboat-services` owns persistence, credentials, SSH, transfers, formatting and
backup clients. APIs are exported at the crate root and use `anyhow::Result`.
GUI/CLI callers consume these contracts rather than implementing driver mechanics.

## State and query library

- `Store::open` resolves the shared native data directory; `Store::open_at` uses
  an explicit directory. `load` returns defaults only for an absent state file;
  malformed state is preserved and reported. `save` validates and writes atomically
  with private permissions.
- `AppState` holds password-free profiles, bounded history, saved queries,
  snippet templates, preferences, filters and dashboard definitions. New metadata
  fields use serde defaults. Samples and live sessions are not persisted.
- `query_library` handles stable saved-query identities, folders/associations,
  search, and SQL document fingerprints. External file changes block replacement
  rather than overwriting another writer's work.
- Query history and snippets are sensitive user-authored text; password-free
  profile serialization does not make SQL safe for publication.

Store selection, aliases and old-data behavior are documented in
[COMPATIBILITY.md](../../docs/COMPATIBILITY.md). GUI and CLI mutation writers must
honor the existing session lock; Store alone is not a concurrent-writer coordinator.

## Credentials

`credential_get`, `credential_set` and `credential_delete` use Secret Service on
Linux and native Keychain on macOS. Synchronous operations belong off the GUI
thread. `CredentialLookup` provides an owned asynchronous lookup with explicit
`cancel().await` termination/reaping. Mac lookup uses a private self-subprocess,
so a blocking authorization call cannot outlive CLI setup deadlines.

Passwords never enter argv or profile JSON. Unicode, empty strings and trailing
newlines retain their exact bytes; NUL and values of 8192 UTF-8 bytes or more are
rejected. Linux retains its legacy credential attributes. macOS does not opt
entries into iCloud synchronization. Do not enumerate or migrate user keyrings.

## Profiles and URLs

`parse_connection_url` returns a validated profile plus a separate memory-only
password; its Debug is redacted and serialization omits the password.
`connection_url` never emits passwords and rejects SSH/read-only profiles whose
policy a URL cannot preserve. Supported network schemes are PostgreSQL/MySQL;
SQLite profiles use explicit file targets.

`export_connections` omits local TLS/identity paths by default. Explicit export
options may retain paths, never their contents. Import validates the entire
document and generates fresh IDs unless preserving them is explicitly requested.
Merge Reject/Skip/Replace policy is deliberate and atomic on error. These pure
functions do not open a Store or read credentials. See
[CONNECTIONS.md](../../docs/CONNECTIONS.md) for formats and supported URL fields.

## Completion and snippets

`CompletionCatalog::new` validates bounded typed metadata. `complete_sql` returns
scope-aware candidates and UTF-8 replacement ranges without executing SQL.
`SnippetTemplate` / `expand_snippet` produce literal text and linked tabstop
ranges. The GUI owns insertion, focus and navigation. Saved queries and snippet
templates remain separate concepts.

## Transfers and formatting

`export_csv`, `export_json`, `export_sql`, `import_csv` and `import_json` provide
in-memory conversions. Lossless CSV reserves `\N` for SQL NULL and
`\x` plus hex for binary, escaping an initial text backslash. JSON uses null,
exact textual numeric representations and `$binary` values. Ambiguous duplicate
columns/keys and inconsistent row shapes are rejected.

Streaming `preview_import`, `apply_import` and `export_query` use explicit
`TransferOptions` / `TransferControl`. The preview seals the input fingerprint;
editing display samples cannot authorize different input. Import owns core's
exclusive `RowImport` transaction and requests Finish only after complete input
validation. Progress is not proof of commit. Cancellation before Finish rolls
back; once finalization starts, callers await its actual result.

Exports publish atomically without overwriting existing files and clean partial
output on failure/cancellation. SQLite formatting preserves token identity and
does not execute SQL; invalid input retains the original editor buffer.

## SSH and backup clients

`Tunnel::open` / `open_with_options` return an owned tunnel guard and an ephemeral
profile. The profile preserves the remote host/port, clears `ssh`, and sets a
private Unix-socket transport. Keep the guard alive while using that profile;
never serialize the transport. `close` terminates and reaps the SSH process.

Host verification uses existing OpenSSH trust. An explicit known-hosts file is
read-only; neither path accepts new hosts automatically. Native SQLx TLS still
validates the original database hostname, including cancellation connections.

`backup` / `restore` support native PostgreSQL/MySQL clients or already-cached
Podman client images. Credentials use private temporary files, not arguments.
Backup refuses existing destinations. Restore requires deliberate confirmation
and rejects read-only profiles. PostgreSQL restore uses one transaction; MySQL
DDL can partially commit.

Podman SSH backup uses an operation-owned loopback-to-Unix relay while retaining
the original DNS TLS identity. IP-literal identities and native-runtime SSH
backup are unsupported. The caller owns the tunnel; the operation owns its relay
and client container. SQLite uses built-in backup/restore instead of these tools.

See [SQLite tools](../../docs/SQLITE-TOOLS.md),
[engine adapters](../../docs/ENGINE-ADAPTERS.md), and [testing](../../docs/TESTING.md).
