# Schema and access tools

SQLite's native schema operations and EXPLAIN QUERY PLAN viewer are described in
[SQLITE-SCHEMA.md](SQLITE-SCHEMA.md). Its operation-specific provider uses reviewed
transactional Apply; whole-schema comparison and server access tools remain
PostgreSQL/MySQL workflows.

Rowboat's schema operations use typed, dialect-aware SQL builders from
`rowboat-admin`. Identifiers are quoted as identifiers; column types are parsed
from the supported type vocabulary. Review SQL before applying it to the
connected server.

## Native workflow

Open a table's **Structure** tab for column/index/key/constraint actions and
table rename/drop. **Database tools** provides namespace and user/role tools.
Each mutation opens a form, then **Review SQL**, then **Apply reviewed change**.
Review is bound to the connection session and target table. Read-only, busy,
pending-row-change and open-transaction guards apply before management changes.

Use **Inspect constraints** or **Inspect privileges** for current server state.
Privilege inspection includes direct/PUBLIC PostgreSQL ACLs and memberships,
or MySQL account settings and SHOW GRANTS; it does not compute all effective
inherited access. Results display up to 200 rows each.

## Offline DDL preview

Create an operation file such as:

```json
[
  {
    "AddColumn": {
      "table": { "schema": "public", "name": "customers" },
      "column": {
        "name": "nickname",
        "data_type": { "VarChar": 80 },
        "nullable": true,
        "default": null
      }
    }
  }
]
```

Generate PostgreSQL SQL without opening a database connection:

```sh
target/release/rowboatctl ddl change.json --engine postgres
```

For MySQL, use the actual database name in `table.schema` and `--engine mysql`.
`rowboatctl diff current.json desired.json --engine postgres` compares arrays of
typed table designs. These commands generate SQL and do not apply it.

The complete enum/type reference is in `crates/rowboat-admin/API.md`. Supported
types are deliberately explicit: unknown server attributes cannot be inferred
from a text field and silently preserved by a simplified design model.

## Live schema comparison

### Native workflow

1. Connect source and destination in separate same-engine workspaces.
2. Activate the destination and choose **Compare schemas…**. Select the source
   workspace and explicitly enter both schema/database namespaces.
3. Compare while both sessions are idle, without transactions or pending row
   edits. Review object differences, blockers, warnings and SQL. Destination-only
   objects are preserved by default; drops require explicit opt-in.
4. Scroll to the apply controls, enter the destination namespace, and choose
   **Recheck destination & apply reviewed plan**. Review binding includes both
   sessions, their revisions, namespace mapping and drop policy.

The destination snapshot is checked again immediately before execution. An
intervening change rejects the plan; refresh the comparison to review it again.
This is stale detection, not a schema lock against concurrent external DDL.
PostgreSQL uses a dedicated transaction and rolls back on error. MySQL DDL can
partially commit; the destination catalog refreshes after execution.

The default scope covers supported table DDL, not row data, ownership/grants or
sequence values. Explicit scopes also compare advanced MySQL columns, supported
object attributes, and bounded existing view queries/scalar routine bodies.
Unsupported definitions block the complete plan rather
than emitting partially faithful SQL. Exact-name matching does not infer renames.
Copy report JSON includes the full comparison; the dialog displays at most 200
object summaries/warnings/blockers.

For **View query / Function body / Procedure body**, supply an exact same-name
identity and native routine signature. Review both original definitions, mapping,
dependency evidence, SQL and recovery plan. **Recheck BOTH endpoints & apply reviewed
plan** rejects changed source as well as destination evidence and preserves the
destination's supported attributes/ACLs. PostgreSQL custom operators visible in
the search path and custom casts anywhere in the database block conservatively.
MySQL routine replacement remains non-atomic; completed steps and uncertain
outcomes stay explicit. These scopes do not synchronize a whole dependency graph.

### CLI preview

`schema-snapshot --schema <name>` reads supported definitions and native catalog
evidence as JSON. `compare-live` opens independent source and destination
connections and generates a review-only migration for the destination:

```sh
ROWBOAT_SOURCE_PASSWORD=source-password ROWBOAT_PASSWORD=destination-password \
  target/release/rowboatctl --connection 'Destination' compare-live \
    --source-connection 'Source' \
    --source-schema public --destination-schema public
```

Use exact profile IDs when names are duplicated. URLs can be supplied through
`ROWBOAT_SOURCE_URL` and `ROWBOAT_URL`. `--format sql` emits migration SQL; the default
JSON report includes object differences, warnings and blockers. `--output`
creates a private new file and refuses overwrite. Both schemas must use the
same database engine and their namespace mapping is explicit.

Destination-only tables/columns/constraints/indexes are retained unless
`--drop-destination-only` is explicitly supplied. Unsupported definitions that
prevent a faithful migration block SQL generation; blockers produce a nonzero
exit status, including when a JSON report was successfully written. Neither
command applies DDL. A snapshot is a point-in-time observation, not a lock on
the source or destination schema.

## Execution semantics

- PostgreSQL table DDL can usually be transactional. Database creation/deletion
  is a separate server operation and cannot run in a PostgreSQL transaction.
- MySQL DDL and account-management commands may commit implicitly. A multi-step
  plan can therefore partially apply; inspect the refreshed server state after
  an error before retrying.
- `MODIFY COLUMN` on MySQL requires a complete column definition. Specialized
  attributes such as generated expressions, auto-increment, collation, comments
  and timestamp update expressions must not be silently lost by a form that
  cannot represent them.
- Dropping a table, column or namespace can discard data and dependencies.
  Generated plans mark destructive steps; dependency errors remain server errors.
- Privilege changes require a server account authorized to perform them.
  Password-changing SQL stays in memory and must not enter history, snippets,
  recovery buffers or ordinary query tabs.

Live verification uses only the disposable Podman fixtures. Production
connections are never discovered or used by automated tests.
