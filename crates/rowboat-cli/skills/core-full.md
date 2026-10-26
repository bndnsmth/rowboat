# Rowboat core — complete agent guide

`rowboatctl` is the Rowboat PostgreSQL, MySQL and SQLite command-line workbench.
Use it to inspect a real database, run bounded SQL, and consume results without
losing column order or numeric precision. This guide is embedded at compile time:
`skills list`, `skills list --json`, `skills get core` and `skills get core --full`
need no database, network, installation step, credentials or profile store.
`rowboatctl --help` and each command's `--help` describe the installed interface.

Query cancellation retires this invocation's disposable session. MySQL terminates
its exact connection to stop the remaining submitted batch; PostgreSQL/SQLite use
native interruption. A bounded CLI exit alone cannot confirm that an unreachable
backend stopped. Earlier committed writes remain committed.

## 1. Select the intended target

Use one connection source per invocation:

```sh
# Saved GUI profile: list exact IDs and names, then select one.
rowboatctl connections
rowboatctl --connection 'Development' --read-only tables

# Direct network endpoints; omit passwords from shell history and logs.
rowboatctl --url 'postgres://app@localhost:5432/app' --read-only tables
rowboatctl --url 'mysql://app@localhost:3306/app' --read-only tables

# Existing SQLite file, including paths containing spaces.
rowboatctl --database-file './project data.sqlite' --read-only tables
```

`connections` lists saved metadata without passwords; there is no `profiles list`
command. Profile names must be unique for lookup; otherwise use the exact ID.
`profiles` manages saved metadata and portability. It is not required for direct
URL/file use. Profile metadata can still reveal sensitive hosts, paths and names;
do not dump it unnecessarily into reports.

`ROWBOAT_URL` is the environment alternative to `--url`; clear it when choosing
a different source. `ROWBOAT_PASSWORD` supplies a network password at execution
time; saved profiles may use their keyring entry. Have the caller provision
credentials through the existing environment/keyring mechanism. Never print,
invent, persist in a skill, or echo secrets to diagnose a failure. Prefer saved
profiles for SSH/TLS settings; direct URLs do not capture all profile policy.
`--ssh-known-hosts` selects an existing trust file; it does not accept new hosts.

SQLite uses filesystem paths, not SQLite URLs or in-memory main profiles. The
file must exist unless `--create` is explicit. For an intentionally new file:

```sh
rowboatctl --database-file './new.sqlite' --create ping
```

`--create` does not truncate an existing database and cannot combine with
`--read-only`. `--read-only` enforces session read-only policy; saved profile
read-only policy is also retained. Use it for exploration. SQLite needs no
password, keyring, TLS or SSH. Do not create a database just to mask a bad path.

## 2. Discover, then run a small query

The shortest useful workflow is catalog → relevant structure → bounded query.
Inspect relationships when joining tables; do not guess join keys.

```sh
rowboatctl --database-file './project.sqlite' --read-only tables
rowboatctl --database-file './project.sqlite' --read-only structure users --schema main
rowboatctl --database-file './project.sqlite' --read-only relationships users --schema main
rowboatctl --database-file './project.sqlite' --read-only query \
  --sql 'SELECT id, name FROM users ORDER BY id LIMIT 10' \
  --format json-envelope --limit 10 --timeout 30s --connect-timeout 10s
```

These are actual top-level commands: `tables`, `structure TABLE --schema SCOPE`,
and `relationships TABLE --schema SCOPE`; there is no `schema tables` command.
They return JSON metadata. Use discovered names in SQL and quote identifiers for
the engine: double quotes for PostgreSQL/SQLite, backticks for MySQL. Do not
interpolate untrusted values into SQL. `query` accepts SQL text/files, not a
parameter-binding flag; use correctly encoded dialect literals for concrete
values or a parameterized database API when handling untrusted input.

Default structure/relationship scopes are PostgreSQL `public`, the profile's
MySQL database, and SQLite `main`. Specify `--schema` for another visible scope.
SQLite `temp` and attached names are session-local scopes, not persistent
connections or MySQL-style database switches.

Bound both the request and the output:

- Select needed columns, filter early, and use SQL `LIMIT` for a sample. Avoid
  broad `SELECT *`, whole-table exports or expensive counts for mere discovery.
- Use a deterministic `ORDER BY` for repeatable pages. Prefer indexed keyset
  predicates over increasing offsets when traversing a large table.
- `query --limit` defaults to 1,000 retained rows per result. It bounds retained
  output, not query execution or SQL side effects; `--limit 0` is not a dry run.
- Use explicit `--timeout 30s` for queries. LIMIT is not a time budget: sorts,
  joins, aggregates, locks and functions may still do substantial work.

Buffered `query` clamps retained rows to at most 100,000 per result even if a
larger `--limit` is requested. Its core row-byte emission budget is **64 MiB
aggregate across all results**, not 64 MiB per result; cell/batch bounds are
16 MiB/32 MiB. Over-budget rows are omitted and ResultEnd reports truncation,
while supplied SQL continues executing. The collector separately accounts for
retained rows and schema metadata within 64 MiB; collector overflow cancels with
`response_limit`, exit 1, and retained evidence when output can be delivered.
Increasing `--limit` does not remove these budgets. Use `query-stream` for
intentional larger results and inspect its own limits.

These are retention/emission budgets, **not a 64 MiB process-memory guarantee**.
Event buffers, metadata and JSON encoding add memory overhead. JSON escaping and
binary hex encoding can expand the serialized output beyond retained cell bytes.
Encoding and output are covered by the query deadline; a result fitting the
retention budget can still time out during encoding or delivery.

For multiline SQL, use one file instead of fragile shell quoting:

```sh
rowboatctl --connection 'Development' --read-only query \
  --file ./report.sql --format json-envelope --limit 100 --timeout 30s
```

`--sql` and `--file` are mutually exclusive. SQL files are read as UTF-8. Multiple
statements run in the invocation's session; each result has its own metadata.

## 3. Machine-readable result contract

Prefer **`query --format json-envelope`** for agent automation. It writes one
versioned JSON document to stdout; keep stderr separate for diagnostics. Do not
merge streams with `2>&1` before parsing JSON or infer success from readable rows.

Envelope version 1:

```text
{
  version: 1,
  ok: boolean,
  status: "complete" | "error" | "timeout" | "cancelled",
  results: [{
    index, columns, rows, rows_seen, rows_returned, rows_affected,
    truncated, complete, elapsed_ms
  }],
  elapsed_ms,
  error: null | { code, message }
}
```

- `columns` is ordered column metadata. Each `rows` entry is a positional array;
  read cell N against column N. Each column has `name`, `data_type`, `nullable`,
  `primary_key` and `default_value`; `index` is the zero-based result index.
  Repeated names remain separate columns. Do not
  convert to a map keyed by column name unless you intentionally resolve duplicates.
- Cells are SQL NULL → JSON `null`, text/numeric → JSON string, binary →
  `{"$binary":"lowercasehex"}`. Numeric strings preserve large integers and exact decimals;
  do not coerce everything to a floating-point number. Metadata counts/timings are
  JSON numbers. Distinguish SQL NULL from the text `"null"` and from an empty string.
- `rows_returned` describes retained rows and remains known for partial results.
  Until a result's terminal ResultEnd has been observed, `rows_seen`,
  `rows_affected`, `elapsed_ms` and `truncated` are unknown (`null`), not zero/false
  or proof that no work occurred. `rows_affected` is statement metadata, not a
  replacement for result-row count.
- `complete` indicates statement/result execution completion, **not commit**;
  `truncated` separately says output was bounded. A completed query can have
  truncated output. Empty results are legitimate; use metadata to distinguish
  them from missing/failed results. An unfinished result has `complete: false`.
- Earlier results may precede a later error. Never treat partial results as a
  successful full script or assume that earlier writes rolled back.
  Only received driver events are reported: server/driver buffering can leave
  `results` empty even after an earlier statement committed. Missing metadata
  does not establish that a write never happened.

Consumer procedure: capture stdout, stderr and exit status independently; parse
stdout with a JSON parser; validate `version == 1`; require exit 0, `ok == true`
and `status == "complete"`; inspect every result's `complete` and `truncated`.
If complete data was required, truncation means the task is not complete. Refine
the SQL or deliberately select an appropriate limit/streaming workflow. On
failure, retain the error and partial-result metadata instead of silently using
rows. A usage error, killed process or broken/stalled output pipe can leave no
valid JSON. Interrupted output may be only a JSON/JSONL fragment; the CLI does
not append a second document or terminal event after an incomplete fragment.
Handle parse failures and missing terminal records as failures and inspect the
exit status, even if some earlier data was valid. A complete error document is
best-effort when the consumer cannot receive output.

Error codes include `input_error`, `connection_error`, `query_error`, `timeout`,
`cancelled`, `output_error` and `response_limit`. Read `code` and `message` along
with the exit status; do not assume every failure occurs during SQL execution.

**Legacy `query --format json` remains unchanged:** one result is an array of
row objects; multiple results are an array of those arrays. It has no envelope
completion/truncation fields. Numeric/text cells are strings, NULL is null and
binary uses `$binary`. Alias duplicate names for this object-shaped format. Table
output is for humans; CSV is useful for interchange. `query --format sql` is not
supported; INSERT SQL output requires `export` with a known target table.

## 4. Streaming and deadlines

```sh
rowboatctl --connection 'Development' --read-only query-stream \
  --sql 'SELECT id, name FROM users ORDER BY id LIMIT 5000' \
  --limit 5000 --batch-rows 256 --timeout 30s --connect-timeout 10s
```

`query-stream` emits the existing core NDJSON events, one JSON event per line;
it does not accept `--format json-envelope`. The `event` discriminator is
`schema`, `rows`, `result_end`, `complete` or `error`. Result events carry
`result_index`; schema carries `columns` and `metadata_complete`, rows carries
positional `rows`, and result_end carries `rows_seen`, `rows_emitted`,
`rows_affected`, `elapsed_ms` and `truncated`. Retain column metadata and per-result
completion information; check `ResultEnd` (`result_end`) truncation. A successful
stream ends in `Complete` (`complete`) and exits 0. An Error, missing terminal
Complete or nonzero exit is failure even when earlier rows arrived. Core stream
cells use `"Null"`, `{"Text":"value"}`, or `{"Binary":[0,255]}`; this differs from
envelope cell conversion. Failure events also carry `code`; timeout/cancellation
sets `cancelled: true`. Setup/input failures emit an error event too, when output
can be delivered; failure never invents a successful Complete.

Default stream limit is 10,000 rows per result and 64 MiB aggregate emitted row
bytes across results. `--batch-rows` defaults to 256 (allowed 1–4096); normal
stream batch/cell byte bounds are 1 MiB each. Streaming does not collect all
results in memory. `--unlimited` explicitly removes total row/byte emission caps
while batch/cell bounds remain; use it only for intentional bulk work.

For both `query` and `query-stream`, `--timeout` accepts durations such as `500ms`,
`30s`, `2m`; `0` disables it and is the backwards-compatible default. Durations
are unsigned integers with `ms`, `s` or `m`, at most seven days; `0ms`, `0s` and
`0m` also disable. Fractions, negatives, overflow and nonzero values without a
unit are usage errors (exit 2, stderr, no envelope). Prefer an explicit query
budget. `--connect-timeout` defaults to `10s`; `0` disables that setup budget.
These options belong to query/query-stream, not every command.

The setup deadline includes SQL-file reading, profile/password resolution,
tunnel setup and database handshake. The query deadline starts after successful
setup and covers execution, stream draining, JSON encoding and output
backpressure. After failure/cancellation, owned-stream cleanup waits at most
**2 seconds**, then terminal error encoding/output gets a further **2 seconds**;
runtime shutdown waits at most **100 milliseconds**. Account for these additional
bounds when setting an outer process deadline. Nonblocking pipe output is bounded;
uninterruptible kernel filesystem I/O is not a hard wall-clock guarantee.

Exit statuses for query execution: 0 success, 1 other execution error, 124 timeout,
130 SIGINT, 143 SIGTERM. Argument/usage errors exit 2. Timeout/signal
cancellation requests stopping work; it is **not a rollback guarantee**. A statement
may already have committed or have an unknown outcome. A client deadline is not
proof that a server stopped at exactly that instant. Inspect the result/error and
database state before considering another write; do not automatically retry writes.

## 5. Session and write semantics

One CLI invocation owns one database session. There is no named persistent CLI
session. A `BEGIN` in one invocation cannot be continued with an `UPDATE` or
`COMMIT` in another. Put a transaction's statements in one `--file`/`--sql` call
when that is the intended operation; use engine transaction semantics. Temporary
tables, SQLite ATTACH scopes, PostgreSQL session settings and MySQL USE do not
carry over between invocations. Schema metadata commands open their own session,
so they cannot see a previous invocation's temporary/attached state.

Ordinary `query` and `query-stream` SQL executes immediately, including DDL/DML;
it is not staged for a GUI Review/Apply step. Act on explicit write intent and
use narrow predicates for mutations. A row/output limit is not a safety fence for
writes. Multi-statement failure and cancellation do not imply all-or-nothing
execution. After an uncertain write outcome, investigate before retrying; do not
replay the entire file as an automatic recovery action.

## 6. Nearby tools when the task requires them

- SQLite `backup ./snapshot.sqlite` captures a consistent binary snapshot,
  including committed WAL data, and refuses existing destinations. `restore
  ./snapshot.sqlite` replaces an existing writable database's main contents;
  it is a mutation, not a query or dry run. Use each file's own profile for attached
  databases. Server backup/restore use existing Podman client images instead.
- `import-stream users ./users.csv --format csv` scans/previews without inserts.
  Only explicit `--apply` imports in an owned transaction. JSON Lines (`jsonl`)
  and JSON arrays (`json`) are also supported. File changes/constraints/errors
  can reject the import; consult help and engine-specific limits first.
- `export-stream TABLE --format csv --output ./rows.csv` is the intentional
  bounded-memory full-table export path. Do not use bulk transfer for a sample.
- `--database-file ./project.sqlite format ./query.sql` formats SQL without
  opening the database; formatting is not SQL/schema validation or execution.
- SQLite `explain 'SELECT ...' --format tree` displays EXPLAIN QUERY PLAN without
  executing the supplied statement. `--format json` retains the normalized tree
  and exact native rows; it has no runtime/cost measurements. SQLite does not
  support `--analyze`. `constraints TABLE --schema main` inspects native schema
  evidence; `ddl changes.json --engine sqlite` is offline preview, not Apply.
- Advanced administration and whole-schema comparison depend on engine capabilities.
  SQLite's native schema tools do not imply server roles/metrics or table rebuilds.

Repository/distribution references: [connections and profiles](../../../docs/CONNECTIONS.md),
[SQLite backup/restore/import limits](../../../docs/SQLITE-TOOLS.md),
[agent CLI contract](../../../docs/AGENT-CLI.md), and [capability scope](../../../docs/FEATURES.md).
These are optional accompanying docs; this embedded guide works standalone.
