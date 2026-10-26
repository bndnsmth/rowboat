# rowboat-admin integration API

Typed object exploration, schema/access review, EXPLAIN and metrics providers
share the core database contracts. See [schema tools](../../docs/SCHEMA-TOOLS.md)
and [engine adapters](../../docs/ENGINE-ADAPTERS.md) for capability boundaries.

## Live-to-live comparison — stable integration contract

New additive module `live_diff`; existing `ddl`/`diff` APIs remain compatible.

```text
snapshot_schema(db: &Database, schema: &str) -> Result<SchemaSnapshot> [async]
compare_snapshots(source: &SchemaSnapshot, destination: &SchemaSnapshot,
                  mapping: &NamespaceMapping, options: CompareOptions)
                  -> Result<LiveComparison>
compare_live(source: &Database, destination: &Database,
             mapping: &NamespaceMapping, options: CompareOptions)
             -> Result<LiveComparison> [async]
destination_is_current(db: &Database, comparison: &LiveComparison)
                       -> Result<bool> [async]
```

`NamespaceMapping { source: String, destination: String }` is mandatory.
`CompareOptions { drop_destination_only: bool }` defaults to false for tables
**and their columns/constraints/indexes**. Cross-engine comparisons error.
`LiveComparison` exposes `plan: DdlPlan`, `objects: Vec<ObjectDifference>`,
`warnings: Vec<String>`, `blockers: Vec<String>`, `is_blocked()` and immutable
`original_destination() -> &SchemaSnapshot`. Any blocker clears the entire plan.
No apply API. Call the async stale check immediately before an explicitly reviewed
apply; it is a detection check, not an atomic lock or guarantee against later DDL.

Snapshots retain native catalog evidence alongside supported typed table/column/
PK/FK/index/UNIQUE/CHECK definitions, MySQL rich column metadata, and a connection
origin. Unsupported attributes/dependencies are surfaced, never treated as absent.
Internal FK namespaces are remapped explicitly; expressions/custom types needing
namespace rewriting are blocked instead of guessed.

### JSON and stale checks

`SchemaSnapshot`, `TableSnapshot`, `MySqlTableOptions`, `LiveComparison`, and all
public difference/summary fields implement `Serialize`. Comparison JSON includes
`original_destination`, even though Rust callers only receive an immutable accessor.
Connection origin contains server/database/user identity and relevant parse settings;
it never contains passwords, connection URLs, SSH identities, or local TLS key paths.
Catalog definitions can contain application literals and should be treated as schema
metadata, not automatically as public information. Snapshots are in-process trusted
inputs, not a deserializable migration authorization format.

`destination_is_current` re-inspects the original namespace and compares its native
catalog evidence, typed definitions, and origin. Ordinary row contents and sequence/
AUTO_INCREMENT counters are excluded. It can conservatively invalidate for catalog
changes outside executable-plan scope. A different engine returns false; catalog
errors return `Err`, never a successful freshness assertion. The destination schema
must already exist. Collection is multi-query and not a transactional DDL snapshot;
neither collection nor freshness check closes the concurrent-DDL check/apply race.

Invalid mappings/cross-engine inputs and unavailable/incompatible/truncated catalogs
return `Err`. Successfully inspected unsupported features produce explicit blockers
and **zero executable statements**. Callers should render blockers separately from
object summaries; a `Blocked` object entry is not guaranteed for every global blocker.
Preserved objects are explicitly marked `Preserve`; additive convergence does not
mean the two complete schemas are identical. Names are matched literally, with no
inferred renames.

### Supported live-DDL subset

Verified with PostgreSQL 18 and MySQL 8.4. Current PostgreSQL catalog queries require
PostgreSQL 15+ fields; other versions are not verified. MySQL 8.0 compatibility is
not claimed by these live-comparison tests; MariaDB is not supported.

- Ordinary PostgreSQL heap tables and MySQL InnoDB/Dynamic tables; create/drop,
  append/drop columns and supported type/default/nullability changes.
- Supported `DataType` built-ins, literal/default expressions accepted by the
  inspector, named PKs, simple internal FKs with ordered columns and actions,
  ordinary ascending/default btree indexes, named UNIQUE and enforced CHECKs.
- MySQL rich columns retain unsigned, AUTO_INCREMENT, charset/collation, comment,
  ON UPDATE and visibility attributes supported by `mysql_column_definition`.
  Ordinary catalog defaults remain literals, not new CONVERT expressions.
- New MySQL tables explicitly specify charset/collation/comment and create all
  secondary keys atomically with the table, including AUTO_INCREMENT support keys.
- FK removal precedes key/column edits; desired FKs are restored after all tables
  and keys exist. All obsolete keys/indexes are dropped before replacements,
  including PostgreSQL schema-wide PK/UNIQUE/index-name transfers.

### Explicit limits / blockers

- No data, grants, ownership, routines, views, custom types, sequence values,
  identity/generated columns, partitioning, inheritance, or trigger migration.
  Detected unsupported objects and table dependencies block the complete plan,
  including unsupported destination-only objects requested for deletion.
- Expression/prefix/partial/descending/included/non-btree/invisible indexes and
  nondefault index collations/opclasses/storage/comments are blocked. PostgreSQL
  NULLS NOT DISTINCT, deferrable/unvalidated/unenforced/period constraints,
  non-simple FK MATCH and ON DELETE SET NULL/DEFAULT column subsets are blocked.
- PostgreSQL table/column comments, ACLs, RLS/policies, nondefault storage/options,
  tablespaces, clustering, replica identity and explicitly named NOT NULL
  constraints outside the expected default naming are not modeled. Creator
  default table ACLs block generation. Ownership is evidence only, not compared
  for migration equivalence or copied to the destination.
- Custom expression dependencies and external FKs/views/rules are blocked where
  catalog evidence identifies them. Dynamic SQL/application dependencies are not
  discoverable. Raw expressions are lexically validated, not proven pure.
- Column reordering or insertion before an existing source column is blocked.
  Existing MySQL table-option changes and AUTO_INCREMENT changes without a stable
  support key throughout the staged ALTER sequence are blocked.
- MySQL ENUM/SET, DATETIME, unsupported display-width syntax/generated columns and
  unknown EXTRA attributes are rejected by the rich-column loader. Backslash-based
  defaults/checks/comments requiring SQL-mode-dependent rewriting are blocked.
  Nonzero `lower_case_table_names` is blocked; case-insensitive child-name collisions
  and schema-wide FK/CHECK name collisions are checked.
- MySQL requires direct global SELECT/TRIGGER/SHOW VIEW/EXECUTE visibility to prove
  inbound dependency visibility; schema-only or role-derived grants are not inferred.
  PostgreSQL inspection requires schema USAGE and table SELECT. Apply privileges
  are not asserted. Differing recorded parse settings block expression transfer.

Plans remain review-only. Type casts, existing data, constraint validity, privileges,
locking cost, and engine-specific implicit commits can still cause execution failure.
See [TESTING.md](../../docs/TESTING.md) for disposable live regression fixtures.

## Schema and access operations

Existing public struct fields remain compatible. New review-only APIs:

- `ddl::CheckDesign { name, expression }`, `UniqueDesign { name, columns }`.
  `DdlOperation::{AddCheck {table,check}, DropCheck {table,name},
  AddUnique {table,unique}, DropUnique {table,name}}`. Existing
  `AddPrimaryKey` / `DropPrimaryKey` remain supported.
- Targeted `DdlOperation::SetColumnDefault {table,name,default: Option<DefaultValue>}`
  on both engines; `SetColumnNullable {table,name,nullable}` and
  `SetColumnType {table,name,data_type,using: Option<String>}` on PostgreSQL.
- MySQL: `ddl::mysql_column_definition(&Database,&TableRef,name)` loads a
  `MySqlColumnDefinition` preserving supported type/default/unsigned/auto-increment/
  charset/collation/comment/ON UPDATE/visibility attributes. Modify its desired
  fields, then preview `DdlOperation::ModifyMySqlColumn {table,definition}`.
  Unsupported/generated definitions error instead of silently dropping metadata.
  Legacy `AlterColumn` remains a full replacement with an explicit lossy warning;
  GUI should use the new preload/modify path or targeted default edit.
- `ddl::constraints(&Database,&TableRef) -> Result<Vec<QueryResult>>` reads current
  PK/UNIQUE/FK/CHECK constraint names, kinds and definitions. All three fields
  return Text/Null, never binary-marked text. MySQL FK definitions include quoted
  local columns, referenced schema/table/columns and ON UPDATE/ON DELETE actions.
  MySQL column-list aggregation remains subject to server group_concat_max_len.
- `access::MySqlUserOperation::AlterPolicy {account,policy: MySqlAccountPolicy}`:
  optional resource limits, password expiration, failed-login attempts, temporary
  lock duration, and account lock. Existing create/password/rename/drop variants
  remain. `mysql_account_info(&Database,&MySqlAccount)` reads non-secret settings.
- `PgRoleOperation::Patch {name,options: PgRolePatch}` changes only selected role
  flags/connection limit/valid-until. `SetPassword {name,password}` changes only
  password. Existing full-options create/alter and membership variants remain.
- `access::pg_grants(&Database,role)`, `pg_role_info(&Database,role)`,
  `pg_role_memberships(&Database,role)` return real query results. `pg_grants`
  includes direct and PUBLIC object ACL grants; inherited memberships are listed
  separately. Existing `mysql_grants` reads SHOW GRANTS.

### Field details

`MySqlColumnDefinition { column: ColumnDesign, unsigned: bool, auto_increment:
bool, character_set: Option<String>, collation: Option<String>, comment: String,
on_update_current_timestamp_precision: Option<u8>, invisible: bool }`.
The loader errors on generated columns, unsupported types (including ENUM/SET,
DATETIME, tinyint display-width syntax), unknown EXTRA attributes and backslash
comments. It preserves MySQL's special CURRENT_TIMESTAMP default form. It does
not silently approximate unsupported definitions. Modify plans are full replacements;
concurrent schema changes between load and apply are not automatically detected.

`MySqlAccountPolicy { resources: MySqlResourceLimits, password_expiration:
Option<MySqlPasswordExpiration>, failed_login_attempts: Option<u16>,
password_lock_time: Option<MySqlPasswordLockTime>, locked: Option<bool> }`.
`MySqlResourceLimits` has four optional u32 fields: `max_queries_per_hour`,
`max_updates_per_hour`, `max_connections_per_hour`, `max_user_connections` (0
means the server-defined unlimited/default behavior). Expiration variants:
`ExpireNow`, `Default`, `Never`, `IntervalDays(u16)`. Lock-time variants:
`Days(u16)` and `Unbounded`. None leaves the existing setting unchanged. Policy
features target MySQL 8.0.19+; INVISIBLE columns require MySQL 8.0.23+.
`mysql_account_info` includes failed-login and lock-time metadata, strictly
decodes its known textual fields, and never selects password/authentication hashes.

`PgRolePatch` has optional bool fields `login`, `superuser`, `create_database`,
`create_role`, `inherit`, `replication`, `bypass_rls`; optional i32
`connection_limit`; optional String `valid_until` (timestamp input or `infinity`,
always safely quoted). Empty patches/policies error. Existing full `Alter`
semantics remain; GUI partial edits should use `Patch`.

`grant_sql` escapes MySQL database-name `_`, `%`, and backslash pattern bytes
in addition to identifier quoting. It assumes `@@global.partial_revokes=OFF`.
Use `grant_sql_with_mysql_mode(engine, op, MySqlDatabaseGrantMode::LiteralNames)`
when partial_revokes is ON; `WildcardPatterns` is the explicit OFF variant.
`mysql_grants` now returns strict UTF-8 textual grants, including binary-marked
SHOW GRANTS metadata.

`plan.review(engine) -> PlanReview { implicit_commit, requires_outside_transaction,
destructive_statement_count }` provides structured review metadata. Use alongside
warnings and per-statement reasons; it does not execute or authorize application.

## Contract

All DDL/access/schema-comparison builders return **review-only SQL** and never
apply it. `DdlPlan { statements, warnings }` contains per-statement `sql`,
`destructive`, and `reason`; `plan.sql()` joins SQL and `plan.is_empty()` tests
whether there are statements. Warnings may exist on an empty plan (column order).
All dynamic identifiers are quoted with core's dialect helpers. Core's
`Engine`, `TableRef`, `Cell`, `Database`, and `QueryResult` are used directly.
Public functions return `anyhow::Result` unless they only select a static SQL query.

`rowboatctl` integrates these APIs. This crate is a library with no
duplicate CLI and no apply endpoint.

## Schema designer — `ddl`

```rust
use rowboat_admin::ddl::*;
use rowboat_core::{Engine, TableRef};

let column = ColumnDesign {
    name: "display name".into(),
    data_type: DataType::parse(Engine::Postgres, "varchar(120)")?,
    nullable: true,
    default: None,
};
let plan = preview(Engine::Postgres, &[DdlOperation::AddColumn {
    table: TableRef { schema: "public".into(), name: "people".into() },
    column,
}])?;
// Present plan.sql(), destructive flags and warnings for review.
# Ok::<(), anyhow::Error>(())
```

`TableDesign { table, columns, primary_key, indexes, foreign_keys }` is fully
serializable. `ColumnDesign` has name/type/nullability/default. Named primary
keys and indexes use `Vec<String>` columns; indexes also carry `unique`.
`ForeignKeyDesign` carries named local/referenced columns, `TableRef` target,
and typed `ReferentialAction` values for update/delete.

`DdlOperation` supports:
- Create/drop/rename table (PostgreSQL explicit cascade option).
- Add/drop/rename/alter column (complete replacement definition for MySQL).
- Create/drop/rename index.
- Add/drop named primary keys and foreign keys.
- Add/drop CHECK and UNIQUE constraints, targeted column attributes, and explicit
   full MySQL column-definition replacement (see schema operations above).

`DataType` is a typed enum, **never raw DDL**. Its parser supports common
integer/boolean/text/char/varchar/decimal/real/double/date/time/timestamp/JSON/
UUID/binary types, with dialect and size validation. PostgreSQL arrays and
explicitly quoted `Custom(TableRef)` types are supported. Renderers revalidate
deserialized variants. Arbitrary type text, enum declarations, identity,
auto-increment, generated columns, collation, partitioning and index expressions
are not modeled. PostgreSQL decimal precision is limited to 255 in this model.

`DefaultValue::Literal(Cell)` safely renders values. MySQL non-null literal
defaults use expression parentheses (requires MySQL 8.0.13+).
`DefaultValue::RawExpression(String)` is **explicit trusted SQL**: validation
rejects comments, statement separators outside quotes, NUL, and unbalanced
delimiters. It does not prove semantic validity or function purity. No implicit
raw SQL is accepted in types/identifiers/default literals.

`AlterColumn` replaces type/nullability/default. PostgreSQL emits three ALTER
statements; conversions needing `USING` require manual SQL review. MySQL does
not preserve attributes absent from this legacy model and emits a prominent LOSSY
warning. Prefer the new targeted variants or the MySQL load/modify path. PostgreSQL
`SetColumnType` accepts a trusted, lexically validated optional USING expression.
MySQL DDL can implicitly commit. MySQL rename destinations are schema-qualified
so renaming a table outside the current database does not move it inadvertently.

## Schema comparison — `diff`

`compare(engine, current: &[TableDesign], desired: &[TableDesign]) -> Result<DdlPlan>`

Exact-name matching; deterministic order. Drops affected inbound/outbound FKs
first, changes columns/PKs/indexes, creates tables, then restores FKs. New cyclic
FK relationships are supported. Rejects duplicate tables, dangling desired FKs
to explicitly removed tables, and missing referenced columns in supplied tables.
Equal schemas produce no SQL; column-order-only differences produce a warning.
Renames are never guessed. The complete affected FK graph must be supplied.

This compares typed snapshots, **not arbitrary server DDL**. It does not import
core's lossy string-based `Structure` into editable snapshots or compare routines,
views, grants, triggers, external dependencies, or unmodeled attributes. Review
is required, especially for column conversion and rebuilding constraints/indexes.

## Object browser and inspection — `objects`

- `browse_objects(&Database, ObjectKind) -> Result<Vec<DatabaseObject>>`
- `object_sql(engine, kind) -> Result<String>`
- `connection_info(&Database)`, `server_activity(&Database)`,
  `namespaces(&Database)`, `schemas(&Database) -> Result<Vec<QueryResult>>`
- Corresponding static query selectors: `connection_info_sql`,
  `server_activity_sql`, `namespaces_sql`.
- `namespace_sql(engine, &NamespaceOperation) -> Result<DdlPlan>` builds
  create/rename/drop database/schema. MySQL rename is rejected, rather than
  inventing unsupported SQL. MySQL SCHEMA is a DATABASE alias.

`ObjectKind`: Function, Procedure, Trigger, View, MaterializedView, Role, Sequence.
MySQL rejects MaterializedView and Sequence. `DatabaseObject` has kind, optional
schema, name, optional definition/owner/details. PostgreSQL routine overloads
are distinguished by identity arguments in `details`; trigger details name the
table. MySQL routine/trigger/view definitions are bodies, not full CREATE scripts.
MySQL Role lists accounts from `mysql.user` (users and roles share this catalog);
its `schema` field is the host. No authentication hashes/passwords are selected.

All queries use live server results; permissions may restrict visible rows or
definitions. Errors propagate. Catalog binary-marked textual fields decode strict
UTF-8, not lossy bytes. Object browsing errors on truncation (100,000 row cap).
Activity/namespace raw results retain core's truncation metadata (10,000 cap).

## Users, roles and grants — `access`

- `mysql_user_sql(&MySqlUserOperation, MySqlStringMode) -> Result<DdlPlan>`:
  create/password/lock/unlock/rename/drop users, typed `MySqlAccount {user, host}`.
  Choose `BackslashEscapes` or `NoBackslashEscapes` to match the executing
  connection's sql_mode. `mysql_sql_mode(&Database)` queries it.
- `mysql_grants(&Database, &MySqlAccount) -> Result<Vec<QueryResult>>` reads
  SHOW GRANTS; no mutation.
- `pg_role_sql(&PgRoleOperation) -> Result<DdlPlan>`: create/alter/rename/drop,
  clear password, grant/revoke membership (optional admin option).
  `PgRoleOptions` explicitly sets login/superuser/create-db/create-role/inherit/
  replication/bypass-RLS; optional connection limit. ALTER is a full attribute
  replacement, not a partial patch. Defaults are non-superuser, no login, inherit.
- `grant_sql(engine, &GrantOperation) -> Result<DdlPlan>` validates typed
  `Privilege`, `GrantTarget`, and dialect-matched `Grantee`. PostgreSQL database,
  schema, table, sequence and MySQL global/database/table grants/revokes supported.
  Grant option is supported for GRANT. No routine signatures, dynamic MySQL
  privileges, ownership reassignment, grant-option-only revoke, or role deletion
  dependency cleanup is inferred.
  MySQL database scope patterns require the partial_revokes mode noted above.

Credential operation enums deliberately do not implement Debug/Serialize.
Returned password SQL contains plaintext: do not log or persist these previews.
No account/grant builders execute operations.

## Editor helpers — `query`

- `expand_named(engine, sql, &BTreeMap<String, Cell>) -> Result<String>` expands
  ASCII `:name` placeholders to dialect-safe literal SQL. Strings, quoted
  identifiers, line/block comments, PostgreSQL nested comments/dollar strings/
  E-strings/`::casts` are preserved. Missing values and malformed quoting error.
  PostgreSQL line comments end at CR or LF; native MySQL `--`/`#` comments end
  at LF (including CRLF), with bare CR remaining comment text. MySQL `--`
  requires a following whitespace/control byte. MySQL/MariaDB executable
  comments are rejected. This returns rendered SQL, not a
  prepared statement; values cannot stand in for identifiers or SQL fragments.
- `expand_named_with_options(..., SqlLexOptions)` supports MySQL
  NO_BACKSLASH_ESCAPES and ANSI_QUOTES. Default expansion assumes neither flag;
  PostgreSQL assumes standard_conforming_strings=on (core sets it on connect).
- `explain_sql(engine, sql)` validates one explainable statement and produces
  JSON-format plain EXPLAIN; `explain(&Database, sql)` runs it. Never adds ANALYZE.
  EXPLAIN uses its own span-based single-statement guard, independent of core's
  batch splitter. It rejects quote boundaries that differ across MySQL ANSI_QUOTES/
  NO_BACKSLASH_ESCAPES or PostgreSQL standard_conforming_strings settings. Use
  unambiguous quoting (explicit PostgreSQL E-strings where needed). Bare CR inside
  a MySQL line comment is conservatively rejected; use LF or CRLF. Raw default
  expression lexing uses default MySQL string modes. Server-side
  planning functions remain server behavior; this is not a SQL sandbox.
- `completion_candidates(&Database, prefix) -> Result<Vec<CompletionCandidate>>`
  fetches real catalog tables/views and per-table columns plus SQL keywords.
  Candidates expose label, quoted insert_text, kind, table and optional data_type.
  Fetch/cache off the UI thread; one structure query per table. Errors propagate.
  Prefix matching is schema-aware; this is not a SQL parser or alias resolver.

## Metrics — `metrics`

`sample(&Database) -> Result<MetricsSample>` queries pg_stat_database for the
current PostgreSQL database or MySQL SHOW GLOBAL STATUS for server-wide metrics.
The sample contains engine, Unix timestamp, scope, optional stats-reset timestamp,
`Metric {name, value: u64, unit, kind: Counter|Gauge}`, and real raw QueryResults.
Null/absent values are omitted, never fabricated as zero. `metrics_sql(engine)`
and `decode_sample(engine, raw, timestamp)` are public. A sample is not a rate;
UI rate calculations must account for sampling time and counter resets (MySQL
uptime is included). No fake charts, background poller, or history store.

## Tests

`cargo test --locked -p rowboat-admin` covers quoting, dialect-specific DDL,
type/default validation, parameters, comparison ordering, access SQL, plans and
metrics decoding. Live tests require the disposable fixtures described in
[TESTING.md](../../docs/TESTING.md); no saved connections are discovered.
