# SQLite everyday tools

Rowboat opens existing filesystem databases by default. These tools use the
built-in SQLite provider; no SQLite CLI, Podman image, network or password is needed.

## Consistent backup and restore

```sh
target/release/rowboatctl --database-file ./project.sqlite --read-only \
  backup ./project-snapshot.sqlite

# Restore into an existing writable database; replaces its main contents.
target/release/rowboatctl --database-file ./project.sqlite \
  restore ./project-snapshot.sqlite
```

Backup captures a consistent committed snapshot, including WAL pages, using
SQLite's online-backup API. It validates the source and atomically publishes a
private binary file; an existing destination is never overwritten. A snapshot is
an ordinary SQLite database and can be opened as a separate connection.
Existing destination `-wal`, `-shm` or `-journal` entries are also rejected and
preserved, so unrelated recovery sidecars cannot override the new image.

Restore integrity-checks a pinned source snapshot before changing the target.
SQLite applies the entire image in one transaction, preserving the target inode
and permissions. Errors before commit leave its contents intact. Existing external
WAL readers may finish reading their old snapshot; new reads see the restored
contents. Old Rowboat query-edit proofs are invalidated even for an identical image.

In the native application, choose **Back up database** or **Restore database** from
the tools menu. SQLite uses binary-file selection and an exact full-path restore
confirmation. Finish transactions and staged edits first. Same-file workspaces are
coordinated and old results are cleared; editor SQL is never replayed automatically.

Scope and operational bounds:

- `main` only. Back up attached databases through their own file profiles.
- Source and restore target must already exist; `--create` does not change this.
  An empty placeholder is not an initialized snapshot. SQL scripts are not snapshots.
- Read-only profiles can back up but cannot restore. `--image` is server-only.
- Final-path symlinks and hard-linked databases are rejected; use the real file.
  Parent-directory aliases (including macOS's standard temporary paths) are resolved.
- WAL restore requires matching page sizes. A busy writer fails within the
  five-second contention budget. Total native work is bounded to sixty seconds at
  SQLite progress/page boundaries; kernel filesystem calls are not interruptible.
- A cancelled worker completes native rollback/cleanup. Cancellation after the
  atomic commit boundary can leave a complete restore or published backup; it never
  implies a partial image. Await the operation's definitive result before retrying.

## Streaming CSV, JSON Lines and JSON arrays

```sh
# Full-file scan and bounded preview, no inserts.
target/release/rowboatctl --database-file ./project.sqlite \
  import-stream users ./users.csv --format csv

# Explicit Apply: re-read the fingerprinted file, commit only after successful EOF.
target/release/rowboatctl --database-file ./project.sqlite \
  import-stream users ./users.csv --format csv --apply

target/release/rowboatctl --database-file ./project.sqlite \
  import-stream users ./users.jsonl --format jsonl --apply
target/release/rowboatctl --database-file ./project.sqlite \
  import-stream users ./users.json --format json --apply
```

The native table import flow uses the same bounded scan, preview and explicit Apply.
File changes invalidate the preview; editing a displayed sample cannot substitute
different rows. Only explicit successful completion commits. Cancellation, malformed
input, constraints, producer abandonment and timeouts roll back the owned transaction.
Existing transactions must finish first. Names are quoted and values are bound;
NULL, text and binary remain distinct, with SQLite's declared column affinity.
Imports use `INSERT OR ABORT`: table-level REPLACE/IGNORE/FAIL policies cannot
silently replace existing rows or partially accept the file.

Ordinary main/temp/attached tables are supported. Views, virtual/shadow/internal
tables, target triggers (including TEMP triggers), supplied generated/hidden columns
and `journal_mode=OFF` are rejected. Omitted generated/default columns work normally.
These limits preserve the atomic import guarantee. CLI `--schema` defaults to `main`;
session-attached targets are available through the shared live-session API.

## SQLite-aware formatting

```sh
# Engine selected from a file target; no connection or file creation occurs.
target/release/rowboatctl --database-file ./project.sqlite format ./query.sql
```

The native SQL formatter uses the current connection's dialect. SQLite formatting
changes whitespace while preserving literal/comment bytes, identifier spelling,
operators, parameters and trigger bodies. It is idempotent and never prepares or
executes SQL. Malformed lexical input returns an error and leaves the editor intact;
formatting is not full SQL grammar/schema validation or automatic syntax repair.

See [FEATURES.md](FEATURES.md) for limits and [TESTING.md](TESTING.md) for checks.
Structured schema tools and EXPLAIN use the separate native providers described
in [SQLITE-SCHEMA.md](SQLITE-SCHEMA.md); server administration is unsupported.
