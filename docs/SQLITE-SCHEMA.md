# SQLite schema tools and query plans

SQLite uses the workbench's table designer, Structure actions and SQL plan viewer.
These tools operate on the connected file and its explicit `main`, `temp` or
attached database namespace. Platform status and limits: [FEATURES.md](FEATURES.md).

## Reviewed schema changes

Use **More tools → Database tools → Create table** for the table designer, or select
a table and open **Structure → Columns & indexes…** for supported actions. Choose
**Review SQL**, inspect the generated statements, then **Apply reviewed change**.
Review preparation does not mutate the schema. **Inspect schema / indexes / FKs…**
opens native catalog inspection, including on read-only connections.

Supported native operations:

- Create, rename and drop ordinary tables.
- Add, rename and drop columns using SQLite's native ALTER TABLE.
- Create and drop ordinary or unique indexes.
- Inspect native definitions, columns, indexes/index terms and foreign keys.

New-table constraints are compiled as part of CREATE TABLE where supported.
Changing an existing column's type/default/nullability or adding/dropping existing
constraints requires table reconstruction and is not offered by this provider.
Index renaming, server namespace DDL, virtual/shadow/internal table changes and
whole-schema comparison are also outside this scope. Dependency/version/object
checks can reject an otherwise supported operation.

Live schema changes require SQLite 3.37 or newer and supported Unix/VFS file
identity evidence. `journal_mode=OFF`, `query_only`, `legacy_alter_table` and
`writable_schema` modes reject rather than being silently changed. DROP TABLE
rejects inbound foreign keys, and unresolved view/trigger dependencies can also
block drops. DROP INDEX requires an explicit owned index, not a constraint
autoindex. Multi-step changes that rename a table may need a second review before
addressing its new name; this is not a schema-migration graph planner.

Apply retains the opaque reviewed plan. It acquires exclusive session ownership,
starts `BEGIN IMMEDIATE`, and checks the actual session, file/attachment and catalog
against the review before executing anything. A changed target rejects the stale
review. Failure or cancellation rolls back through the owned worker; commit happens
only after successful completion. Existing caller transactions and read-only
sessions are rejected for mutations. Native ALTER preserves SQLite's own dependency
and attribute handling rather than copying rows into a simplified replacement table.

SQL text and row-edit evidence are separate from schema review. After schema changes,
inspect refreshed structure/results; old row-edit proofs must not be reused and
editor SQL is never automatically replayed.

## Query-plan inspection

Choose **Explain query** for the captured SQL. Rowboat sends only SQLite's
`EXPLAIN QUERY PLAN`, not the underlying SELECT/INSERT/UPDATE/DELETE. The viewer
shows an expandable forest using the actual node IDs/parent relationships and
preserves the complete native detail strings, including unfamiliar plan operations.

SQLite plan output describes strategy, such as scans, index searches and temporary
B-trees. It does not provide actual runtime measurements or comparable optimizer
costs; Rowboat does not invent them. SQLite's statistics-changing `ANALYZE` is not
an execution-profile feature and is not invoked by this viewer.

Copy retains native rows `{id, parent, aux, detail}` in their original order. Empty
native plans are displayed honestly. Malformed, truncated or excessive plans fail
rather than appearing complete: limits are 1,000 rows, 128 tree levels and 1 MiB
combined detail text.

The same provider is available for terminal inspection:

```sh
rowboatctl --database-file ./project.sqlite --read-only explain \
  "SELECT id, name FROM users WHERE name = 'Ada'" --format tree
```

Use `--format json` for the structured report or `--format raw` for native rows.
The installed command's `--help` describes exact flags.

Backup/restore, import and formatting guarantees are documented separately in
[SQLITE-TOOLS.md](SQLITE-TOOLS.md).
