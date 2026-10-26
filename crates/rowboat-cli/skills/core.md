# Rowboat core — agent quickstart

Use `rowboatctl` for PostgreSQL, MySQL and SQLite exploration and SQL. This guide
is embedded in the binary and works offline. Full contract and examples:
`rowboatctl skills get core --full`.

## Shortest exploration workflow

Choose the user's target: `--connection 'Development'` for a saved profile,
`--url 'postgres://app@localhost:5432/app'` for a direct endpoint, or
`--database-file './project.sqlite'` for an existing SQLite file. List saved
IDs/names with `rowboatctl connections` (not `profiles list`). Use an exact ID
if names are ambiguous. Do not print credentials or put passwords in SQL/logs.

```sh
rowboatctl --database-file './project.sqlite' --read-only tables
rowboatctl --database-file './project.sqlite' --read-only structure users --schema main
rowboatctl --database-file './project.sqlite' --read-only relationships users --schema main
rowboatctl --database-file './project.sqlite' --read-only query \
  --sql 'SELECT id, name FROM users ORDER BY id LIMIT 10' \
  --format json-envelope --limit 10 --timeout 30s --connect-timeout 10s
```

Inspect real columns first; adapt `users`, `id`, `name` and the schema to the catalog.
Use `query --file ./report.sql` instead of `--sql` for a SQL file. Add predicates,
project only needed columns, and use SQL `LIMIT` plus a CLI retention `--limit`.
The CLI limit does not prevent SQL side effects or bound server work.

## Consume results correctly

Keep stdout (data) separate from stderr (diagnostics). Parse `json-envelope` as
JSON; check process exit status, `version == 1`, `ok`, `status`, every result's
`complete` and `truncated`. Completion means statement completion, not commit;
unfinished result metadata can be unknown (`null`). Rows are positional arrays
matching `columns`, so
duplicate column names are preserved. Numeric/text cells are strings, NULL is
null, and binary is `{"$binary":"hex"}`. Keep numeric precision.

`query --format json` remains the legacy row-object format. `query-stream` emits
NDJSON events, not envelopes; require a terminal `Complete`, successful exit,
and check per-result truncation. Query timeout defaults to disabled (`0`);
prefer `--timeout 30s`. Connection timeout defaults to `10s`. Exit 124 means
timeout, 130 SIGINT, 143 SIGTERM, 1 execution error; parse/usage errors also fail.

Ordinary SQL executes immediately; it is not staged for Apply. Write only with
explicit write intent. One invocation is one session: `BEGIN` cannot span CLI
invocations. Cancellation is not a rollback guarantee; never automatically retry
writes with an uncertain outcome. Missing SQLite files are errors; only explicit
`--create` permits creation. Use command `--help` for exact options.
