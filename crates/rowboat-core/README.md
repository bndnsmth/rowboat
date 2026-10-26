# Rowboat database core

SQLx PostgreSQL/MySQL sessions and a native SQLite adapter. `Database` is `Clone + Send + Sync`;
all public async methods return `Send` futures. Clones share one serialized,
connection-affine session (including SQL-entered transactions). Cancellation uses
a separate authenticated connection and cannot target the next queued operation.

## Stable API

TLS uses SQLx's native-tls backend with vendored OpenSSL on Linux and Apple's
Security framework on macOS. Live
PostgreSQL/MySQL tests cover CA validation, strict hostname validation, required
client certificates, and negative handshakes. This avoids SQLx 0.8's rustls
`verify-ca` error-variant mismatch while retaining strict `verify-full` behavior.

All fallible methods return `anyhow::Result` with operation/server error context.

```rust,ignore
Database::connect(&ConnectionProfile, password: &str) -> Result<Database> // async
db.engine() -> Engine
db.in_transaction() -> Result<bool>                                    // async
db.query(sql: &str, limit: usize) -> Result<Vec<QueryResult>>             // async
db.catalog() -> Result<Vec<TableInfo>>                                  // async
db.structure(&TableRef) -> Result<Structure>                            // async
db.browse(&TableRef, &BrowseRequest) -> Result<QueryResult>              // async
db.browse_filtered(&TableRef, &BrowseRequest, Option<&FilterExpr>) -> Result<QueryResult>
db.relationships(&TableRef) -> Result<Vec<ForeignKeyRelationship>>        // async
db.query_stream(&str, QueryStreamOptions) -> Result<QueryStream>         // async
db.import_rows(&TableRef, RowImportOptions) -> Result<RowImport>         // async
db.apply_changes(&TableRef, &[RowChange]) -> Result<u64>                 // async
db.begin() / db.commit() / db.rollback() / db.cancel() -> Result<()>     // async
```

SQL helpers: `quote_ident(Engine, &str) -> String`,
`qualified_table(Engine, &TableRef) -> String`, `literal(Engine, &Cell) -> String`,
`preview_changes(Engine, &TableRef, &[RowChange]) -> Result<String>`, and
`split_statements(Engine, &str) -> Result<Vec<String>>`.

## Behavior

See [engine adapters](../../docs/ENGINE-ADAPTERS.md) for ownership and provider
boundaries. Nested typed filters support AND/OR,
NULL-aware membership and ranges. FK navigation preserves ordered composite pairs.
Query streams provide bounded progressive schema/rows/result-end/terminal events;
explicit unlimited export disables total caps while preserving batch/queue limits.
`query()` collects this same guarded execution path. Cancelled `recv()` futures
retain terminal delivery for the next call.

Atomic imports exclusively own the session, bind typed rows through bounded sends,
and commit only on explicit `finish()`. Drop, startup-future cancellation, invalid
batches, idle timeout and SQL errors trigger rollback/drain before reuse. COMMIT
has an uncancellable phase and returns its actual outcome. Preflight rejects open
transactions, readonly profiles, unsupported targets, user triggers/rewrite rules,
and MySQL tables without provable trigger visibility. MySQL must be permanent
InnoDB: effective SHOW CREATE checks reject session-local temporary shadows even
when information_schema describes an underlying permanent InnoDB table.

- TLS modes: disable, prefer, require, verify-ca, verify-full; optional CA and
  client certificate/key files. SQL statement logging is disabled. Passwords are
  held only in connection options, including those needed for cancellation.
- Read-only profiles use both a token-based editor command policy and real
  server READ ONLY transactions. A whole editor batch is validated before any
  statement executes. SELECT/WITH/VALUES/TABLE/SHOW/DESCRIBE/EXPLAIN are admitted;
  writes embedded in otherwise readable SQL are stopped by the server. Explicit
  BEGIN/START always starts READ ONLY; commit/rollback, chaining and savepoints
  are supported. SET/RESET/DISCARD, READ WRITE, executable MySQL comments,
  `set_config`, SELECT INTO/file output and unclassified commands are rejected.
  Automatic read-only operation transactions roll back on success/error/cancel;
  user-owned transactions retain their normal explicit commit/rollback lifecycle.
  Parser settings are restored before each statement and read-only defaults after
  transaction controls, including when a callable routine changed session defaults.
  Grid mutation is also rejected client-side. This remains a workbench policy,
  not a database privilege boundary: use an appropriately restricted DB role;
  trusted server functions/extensions can have external effects beyond a local
  database transaction. Session customization commands are intentionally unavailable
  in read-only editor sessions.
  Admitted read statements must also pass the server's native single-statement
  prepare protocol before raw text-result execution; there is no raw fallback
  when preparation fails. This independently rejects hidden multi-command batches
  if the local lexer and server disagree. Savepoint controls are rebuilt from a
  canonical command and one dialect-quoted identifier (MySQL cannot prepare these
  controls); their original SQL text is never executed.
- Raw SQL batches use native multi-statement protocols and return every result.
  Results retain at most `min(limit, 100_000)` rows each, 256 result sets, and
  approximately 64 MiB of row allocations per call. Excess rows are drained and
  flagged `truncated`; the server still completes its query. `limit = 0` retains
  no rows. Result caps do not constrain server computation or one incoming cell.
- Browse defaults to 100 rows when `limit = 0`, fetches one extra for truncation,
  and uses native column types for ordering. Sort's boolean means ascending.
  Filters validate column names and bind values. LIKE wildcards in Contains and
  StartsWith are escaped as literal text. Complete primary keys provide default
  ordering where available.
- PostgreSQL uses canonical server text for arbitrary scalar, array, range,
  enum, composite and extension values; bytea remains bytes. MySQL raw text
  protocol retains numeric precision and dates; blobs/BIT/geometry remain bytes.
  Unknown binary representations are retained rather than failing the result.
  SQL NULL, text `NULL`, empty text and empty bytes remain distinct.
- Edits validate all column names and complete non-NULL original primary keys.
  Original values are compared byte-for-byte in their canonical display form
  (PostgreSQL uses schema-qualified deterministic `pg_catalog."C"` collation,
  including previews, while typed primary-key lookup remains intact);
  every change must affect exactly one row. PostgreSQL input uses typed casts
  from bound text; binary uses bytea bindings. MySQL uses bound values with server
  target-column conversion. InnoDB is required for MySQL edits.
- A batch owns a transaction or uses a unique savepoint in the caller's existing
  transaction. Transaction detection probes the server instead of trusting SQL
  parsing; MySQL `autocommit = 0` is respected. A failed batch rolls back all its
  edits, preserving an outer transaction.
- `in_transaction().await` probes the live connection rather than remembering
  SQL text. It returns true for PostgreSQL's failed transaction state until
  COMMIT/ROLLBACK, and reflects MySQL implicit DDL commits. It preserves existing
  transactions and savepoints; the GUI can call it after editor success or error.
- Catalog includes accessible schemas/tables/views (plus PostgreSQL materialized,
  partitioned and foreign tables), columns, primary keys, defaults, indexes,
  foreign-key actions and definitions.

## Current limitations

- PostgreSQL table CREATE definitions are reconstructions, not full-fidelity
  `pg_dump`: partition/inheritance/storage/policy/trigger/comment/grant details
  are not reproduced. MySQL uses native SHOW CREATE; its separate index summary
  uses `<expression>` for functional-index expressions (full definition remains
  in SHOW CREATE). Information-schema GROUP_CONCAT limits can shorten summaries.
- Empty result sets use native preparation metadata where already available
  (notably read-only execution); otherwise headings may be incomplete because
  SQLx raw streaming does not expose empty RowDescriptions. Queries are never
  re-executed for headings. Arbitrary query columns do not carry
  complete base-table key/nullability metadata; browse uses introspection.
- SQL splitting handles quoted strings/identifiers, PostgreSQL dollar quotes and
  nested block comments. It is not a procedural-language parser or MySQL
  DELIMITER interpreter. `query` sends the original batch directly, so native
  procedure definitions do not depend on the splitter.
  Shared lexer rules honor PostgreSQL CR/LF comment endings and continued E-string
  escape modes; MySQL `--` recognizes whitespace/control bytes and, like `#`, ends
  at LF (bare CR stays inside the comment). These differences are checked against
  both live server parsers. PostgreSQL backticks are not identifier quotes.
  Read-only commands unsupported by the server's prepared protocol are rejected.
- No automatic reconnect/retry, COPY/LOAD DATA streaming, notice collection,
  server-side paging cursor, or distributed transaction support. Reconnect would
  lose the session transaction. Call `cancel` instead of aborting a task mid-edit.
- SSH lifecycle belongs to the services layer: establish the tunnel, pass its
  local endpoint, and clear `profile.ssh` before `connect`.
- Dedicated TLS fixtures verify certificate-chain/hostname/mTLS scenarios on both
  engines. Certificate expiry/revocation and exhaustive cipher coverage are untested.

## Tests

`cargo test --locked -p rowboat-core` covers pure contracts and disposable SQLite
files. Network tests are opt-in and require the explicitly provisioned local
PostgreSQL/MySQL fixtures. They never discover user connections. See
[TESTING.md](../../docs/TESTING.md) for commands and fixture ownership rules.
