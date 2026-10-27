# Features and limitations

Rowboat is a native database workbench and an independent CLI sharing the same
Rust database engine. Supported engines are PostgreSQL, MySQL and SQLite.
This page describes implemented capabilities, not full parity with other tools.

## Platform status

- **Linux:** x86-64/ARM64 native CI passes builds, tests, packaging, installation,
  mise GitHub-backend checks and isolated GUI startup. Native Wayland workflows
  have also been exercised locally.
- **macOS:** Intel/Apple Silicon native CI passes the same distribution checks,
  including `.app` packaging and GUI startup. The Apple Silicon GUI has also been
  tested on a real Mac. Native Keychain authorization/roundtrip, SSH/TLS and broader
  interactive acceptance remain outstanding. Bundles are ad-hoc signed, not notarized.
- **Windows:** not a launch target.

Linux CI targets glibc 2.35; GUI desktop libraries and graphics drivers remain
system dependencies. These archives are not universal static Linux binaries.
See [distribution](DISTRIBUTION.md) for the exact requirements and release gates.

## Workbench

| Area | Available | Important limits |
| --- | --- | --- |
| Connections | Saved/grouped profiles, concurrent workspaces, connection testing, portable import/export | No OS deep links or SQLite URI/in-memory profiles |
| Security | TLS/CA/hostname/mTLS, SSH key/agent tunnels, optional system keyring, password-free profile JSON | Read-only mode is not a replacement for restricted database roles |
| Workspaces | Independent sessions, transactions, SQL tabs, split editors, multiple windows, SQL/layout recovery | Result sets and live sessions are not recovered; recovered edits require revalidation |
| Navigation | Schema/object explorer, command palette, MySQL database/PostgreSQL schema contexts, FK navigation | Engine-specific tools depend on catalog permissions and supported object kinds |
| Grid | Virtualized rows, pagination, filters, column sizing, exact numeric local sorting, rectangular/row selection | Local text sorting does not reproduce database collation |
| Row editing | Insert, duplicate, update/delete, NULL/binary/multiline values, paste/fill, SQL review and explicit Apply | Complete original primary keys and optimistic conflict checks are required |
| Query-result editing | Explicit editing run for supported single-table direct projections, aliases and complete keys | Update-only; no joins, CTEs, expressions, temporary sources or caller transactions |
| SQL editor | Highlighting, token-aware formatting, named parameters, snippets/tabstops, contextual completion | No general semantic SQL validation or correlated/LATERAL/type inference |
| Execution | Current/Selection/All scopes, multiple streamed results, cancellation, partial-result/error reporting, error navigation | Ambiguous Current boundaries reject rather than expand; reliable source positions are engine-dependent |
| Plans | PostgreSQL/MySQL EXPLAIN and explicit ANALYZE; SQLite EXPLAIN QUERY PLAN | Optimizer costs are not elapsed milliseconds; ANALYZE can execute SQL |
| Query library | Named/updatable queries, folders, associations, searchable retained history, SQL files | History and SQL drafts may contain sensitive user-authored text |
| Pinned results | Immutable snapshots, exact key/column-mapped comparisons, changed/added/removed rows | Loaded rows only; partial results stay labeled; pins are memory-only and bounded |
| Relationships | Bounded table neighborhoods, composite/self FKs, search, pan/zoom, Data/Structure navigation | No saved diagram layouts or image export; incomplete discovery is labeled |
| Inspection | Whole-record search, expandable JSON, exact value/JSON Pointer copy, raw/binary inspection | Bounded trees/previews; original values remain distinct from staged drafts |
| Appearance | Native light/dark following, optional Omarchy integration, 22 embedded palettes, typography/shortcut preferences | Omarchy is not required; full accessibility/screen-reader support is not claimed |

Ordinary Run does not prepare editing metadata or replay SQL. Close never applies
database edits. SQL recovery does not automatically save a named query or an SQL
file. Unstaged form input and parameter values are not recoverable.

## Engine-specific tools

### PostgreSQL and MySQL

- Catalog-backed structure, indexes, keys, constraints, namespaces, views,
  routines, triggers and applicable sequences/materialized views.
- Reviewed schema operations, scoped same-engine schema/definition comparison,
  freshness checks, and explicit Apply. Unsupported attributes/dependencies block
  plans rather than being silently discarded. Snapshots do not lock external DDL.
- Users/roles, memberships, grants/revokes, account policy and effective-permission
  evidence. Unknown/incomplete privileges stay Unknown; ACL metadata cannot prove
  execution under RLS, definer or login restrictions.
- Builtin metric dashboards and explicit bounded read-only custom SQL cards.
  Sample values are memory-only; custom SQL does not run on a timer.
- Native-client or cached-Podman backup/restore. MySQL DDL can implicitly commit;
  restore and some object replacements are not atomic.

Primary live fixtures use PostgreSQL 18 and MySQL 8.4. Advanced catalog tools may
require newer server fields or elevated metadata privileges; broad version
compatibility and MariaDB support are not claimed. PostgreSQL reconstructed DDL
is not a full-fidelity `pg_dump`. Routine/definition editing uses bounded grammar.
SSH-backed backup currently requires a DNS hostname and the Podman runtime.

### SQLite

- Open-existing by default, explicit create, native file read-only enforcement,
  main/temp/attached catalogs, browsing, transactions and cancellation.
- Native online backup and in-place restore, streaming-import Apply, formatting,
  ordinary row editing and supported query-result updates.
- Reviewed create/rename/drop ordinary tables, add/rename/drop columns, supported
  indexes, and native schema/FK inspection with transactional freshness checks.
- Native EXPLAIN QUERY PLAN without invented runtime statistics.

No table-rebuild designer, whole-schema comparison, server roles/metrics, sampling
provider, URI profile or in-memory main profile is supported. Backups cover the
main file; use separate file profiles for attached databases.

## Data transfer and safety

CSV/JSON/SQL export and bounded CSV/JSONL/JSON-array import preserve NULL, empty
text, binary and exact numeric values. Preview is separate from transactional
Apply; file fingerprints prevent applying a modified source. Exports publish
atomically without overwriting existing destinations. Streaming array export and
additional formats remain future work. Trigger/rule/nontransactional or
temporary-shadowed import targets can be rejected for safety.

Cancellation owns one operation, not a different queued query. Completed earlier
statements may already have committed; cancellation does not imply rollback or
prove remote termination if the control transport is unavailable.

No plugin marketplace, embedded AI runtime, scheduled backups or durable metric
time-series store is included.

See the [user guide](DAILY-DRIVER.md), [CLI guide](AGENT-CLI.md),
[engine adapters](ENGINE-ADAPTERS.md), [SQLite tools](SQLITE-TOOLS.md),
[SQLite schema tools](SQLITE-SCHEMA.md), and [testing](TESTING.md).
