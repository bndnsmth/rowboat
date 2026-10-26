# Built-in CLI agent guide

Discover the installed guide from root or query help:

```sh
rowboatctl --help
rowboatctl query --help
rowboatctl skills list
rowboatctl skills list --json
rowboatctl skills get core
rowboatctl skills get core --full
```

`core` is the built-in database exploration/execution skill. `get` defaults to a
short Markdown quickstart; `--full` emits the complete guide. `list` is concise
human-readable text; `--json` emits an array with `id`, `description` and `command`.
Unknown IDs exit nonzero and list available IDs. Successful skill commands write
only to stdout.

Both Markdown documents are compiled into `rowboatctl` with `include_str!`; no
skill files, network retrieval, profile store, database, tunnel or keyring are
needed at runtime. Dispatch occurs before connection selection or credential
resolution. Invalid connection URL/credential environment values and an unusable
state directory do not prevent reading the skill. Help preserves connection
environment-name hints (such as `ROWBOAT_URL`) while hiding their values;
invalid/non-UTF-8 values do not block help or skill output.

## Agent execution contract

Use `--read-only` for exploration, inspect `tables`, `structure` and
`relationships`, then execute a small projected/predicated query. Prefer:

```sh
rowboatctl --database-file ./project.sqlite --read-only query \
  --sql 'SELECT id, name FROM users ORDER BY id LIMIT 10' \
  --format json-envelope --limit 10 --timeout 30s --connect-timeout 10s
```

Adapt identifiers to the inspected database. The embedded full guide explains
the version-1 envelope, positional cells/duplicate columns, exact numeric strings,
NULL/binary encoding, stdout/stderr separation, partial results, truncation,
deadlines and exit statuses. `query --format json` is the unchanged legacy
row-object output. `query-stream` remains NDJSON core events with terminal
`Complete`, rather than gaining an envelope mode.

### Deadlines, retention and incomplete output

- Query/query-stream defaults: `--timeout 0` (disabled), `--connect-timeout 10s`.
  Durations accept unsigned integer `ms`, `s`, `m` up to seven days; bare `0` or
  zero with a unit disables. Invalid durations exit 2 on stderr, without JSON.
- Setup includes SQL-file reads, profile/password resolution, tunnel and database
  handshake. The query deadline starts after setup and includes execution, stream
  draining, JSON encoding and output backpressure. Prefer `--timeout 30s`.
- Failure/cancellation cleanup waits at most 2 seconds; terminal error encoding
  and output have a further 2-second bound; runtime shutdown waits at most 100 ms.
  An outer process deadline should allow for those extra bounds. Pipe writes are
  nonblocking; uninterruptible kernel filesystem I/O has no hard deadline guarantee.
- Buffered `query`: default 1,000 rows/result, absolute clamp 100,000/result,
  16 MiB/cell, 32 MiB/batch, and **64 MiB aggregate** emitted row bytes across
  results. Over-budget rows are truncated while supplied SQL continues. A separate
  64 MiB collector budget includes retained rows and schema metadata; overflow
  cancels with `response_limit` (exit 1), preserving deliverable evidence.
- `query-stream` does not collect results: default 10,000 rows/result, 64 MiB
  aggregate emitted row bytes, 256 rows/batch, and 1 MiB batch/cell bounds.
  `--unlimited` removes total row/byte caps, retaining batch/cell bounds.
- Retention budgets are not process RSS limits. Event/encoding buffers add
  overhead; JSON escaping and binary hex can expand output. Encoding/output can
  exhaust the deadline even when retained data fits the byte budget.
- Broken/stalled output may leave no complete JSON or terminal NDJSON event.
  An interrupted fragment is not followed by a second document/error line. Reject
  incomplete output and inspect process status; terminal error output is best-effort.

Exit codes: 0 success, 1 execution/input/connection/output/retention failure,
124 timeout, 130 SIGINT, 143 SIGTERM, 2 argument/usage error. Envelope errors have
`code` and `message`; streaming errors also carry `code`. Timeout/cancellation
stream events set `cancelled: true`; setup/input failure can also emit an error.

For unfinished envelope results, `rows_seen`, `rows_affected`, `elapsed_ms` and
`truncated` are unknown (`null`); `rows_returned` remains known and `complete`
is false. `complete` records a statement's ResultEnd, not transaction commit.
Earlier results/writes may precede a later failure. Require successful process
exit, envelope `ok/status`, and every result's completion/truncation checks.
Only events actually received from the driver appear in the response. Server or
driver buffering can leave `results` empty after an earlier statement committed;
missing result metadata is not evidence that a write did not happen.

SQL executes immediately; ordinary queries do not use an Apply step. One
invocation is one session. Transactions and attached/temporary scopes cannot
span invocations. Cancellation is not evidence of rollback and uncertain writes
must not be automatically retried. Missing SQLite files are errors unless
creation is explicitly requested with `--create`.

Query/query-stream use disposable-session cancellation. MySQL terminates that
exact connection rather than only its current statement, because `KILL QUERY`
can allow later statements in an already-submitted batch to run. PostgreSQL and
SQLite use native interruption; stopped sessions are retired. Cancellation still
depends on backend reachability, and bounded CLI exit alone is not confirmation
that an unreachable backend stopped.

## Maintenance and checks

Canonical shipped text:

- [`core.md`](../crates/rowboat-cli/skills/core.md): quickstart.
- [`core-full.md`](../crates/rowboat-cli/skills/core-full.md): complete guide.
- [`skills.rs`](../crates/rowboat-cli/src/skills.rs): command model and offline output.

Keep guidance aligned with the installed CLI, especially query output and
deadline contracts. `cargo test -p rowboat-cli --test pass15_skills` exercises
offline subprocess behavior, help discovery, invalid IDs and real SQLite examples
against disposable files. It does not access saved user profiles or host servers.

See [CONNECTIONS.md](CONNECTIONS.md) for profile/security details and
[SQLITE-TOOLS.md](SQLITE-TOOLS.md) for backup/restore and import guarantees.
