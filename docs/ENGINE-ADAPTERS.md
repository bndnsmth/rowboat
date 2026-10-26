# Engine adapters

Rowboat uses built-in, statically dispatched adapters behind `Database`. The GUI
and CLI consume Rowboat values, streams, metadata and reviewed operations; SQLx
connections and native SQLite handles stay private.

```text
GUI / CLI
  ├── Database + session-bound metadata / editing contracts
  │     └── core adapters: PostgreSQL / MySQL / SQLite
  ├── administration operation providers
  └── services: profile transport / dialect / backup providers
```

## Boundaries

| Shared responsibility | Engine/provider responsibility |
| --- | --- |
| Workspaces, scheduling, bounded delivery, exact result binding | Connection setup, native execution/decoding/interruption |
| Staging, Review → Apply, complete-key/conflict requirements | Locking, transaction state, identity evidence and native value predicates |
| Typed metadata requests and completion scope analysis | Actual namespace resolution, catalog SQL and lexical dialect rules |
| Typed operation plans, review and rendering | SQL generation, supported object kinds and execution semantics |

Core implementation lives under `crates/rowboat-core/src/adapters/`. Small
PostgreSQL/MySQL setup modules and shared network adapter implementation files
preserve existing behavior without a giant public driver trait. SQLite owns its
native actor and storage-class evidence. `Database` remains the public facade.

`QueryStreamOptions.cancellation` distinguishes reusable native sessions from
disposable CLI queries. The default preserves the session; `RetireSession` stops
and retires the operation's exact leased session on cancellation/failure. MySQL's
adapter uses `KILL CONNECTION` for disposable sessions because `KILL QUERY` can
allow later statements in an already-submitted batch to execute. PostgreSQL and
SQLite retain native interruption. Queued cancelled operations cannot terminate
the active operation, and retired sessions reject subsequent work without replay.

`Engine::descriptor()` describes connection kind, namespace behavior, dialect,
object kinds and installed optional tool providers. Provider availability does
not establish current-user permission. Admin `operation_support` and reviewed
`DdlPlan::execution_semantics` supply operation-specific behavior; consumers must
not infer transaction policy from an engine-name comparison.

GUI and CLI completion use `completion_catalog`, `completion_column_names` and
the verified snapshot API. These are bounded and physical-session-affine. The
MySQL snapshot path performs no extra query when its provenance is still valid.
Do not replace the names-only operation with full table structure inspection.

Ordinary GUI query execution uses `query_stream` without editing preflight.
Explicit editing uses a separate required-capture stream: unsupported/failed source
capture must reject before submitting the original SQL. Best-effort capture APIs
remain distinct and must not be used for an action promising a safe editable SELECT.
An earlier plain result cannot gain provenance from asynchronous metadata inspection;
editing binds a new, explicitly requested execution and its exact core snapshot.

## Adding an engine

1. Register its identity and scoped descriptor. Implement a private adapter that
   supplies execution, decoding, metadata, transaction state, cancellation and
   cleanup semantics through the existing facade.
2. Represent its actual connection target. Existing file/network profiles are
   distinct; creating a missing SQLite file requires explicit intent.
3. Register coherent lexical rules for completion and supported service tools.
   Optional administration/backup providers may return typed unsupported reasons.
4. Implement safe editing evidence for supported result shapes. Display strings
   alone are not row identity: native storage classes, source incarnation and
   exact execution binding can matter. Return an explicit read-only reason when
   proof is unavailable.
5. Verify shared query/browse/staging workflows through the public facade and
   preserve existing session ownership, cancellation and network-round-trip tests.

Adding the engine should primarily affect its adapter, descriptor/registration,
target presentation and tests. Shared grids, tabs and reviewed staging should
continue using the same contracts. Structured server tools are separate feature
providers, rather than prerequisites for basic engine support.

## SQLite provider scope

The SQLite implementation supports file open/create/read-only, SQL execution and
streaming, main/temp/attached catalogs, table browsing and verified row updates.
Atomic streaming-import Apply uses engine-private import target,
begin/lock/insert/rollback hooks. The shared bounded producer and explicit Finish
protocol remain authoritative. SQLite online backup/restore uses a blocking native
adapter primitive with a filesystem service provider; FFI does not escape adapters.
The SQLite formatter is a token-preserving service provider, not query execution.

Engine-owned SQLite native DDL and schema inspection providers use
opaque session/file/attachment-bound reviews and exclusive transactional Apply.
The administrative lease uses adapter-owned BEGIN IMMEDIATE, single-statement
validation and transaction-state checks; only explicit Finish commits. Supported
actions are narrower than the schema slot and never imply table reconstruction.
`SchemaComparison` stays separate and unsupported for SQLite.

The SQLite EXPLAIN provider owns native four-column plan decoding and tree bounds;
the shared viewer receives exact native rows and no invented metrics. Read-only
preparation may authorize DML compilation, but native statement classification
must prove EXPLAIN/read-only before stepping. Actual writes remain prohibited.

Server roles/metrics and whole-schema comparison still have explicit
unsupported-provider responses. Ordinary SQL DDL remains executable.
Backups cover the main file; attached databases require their own file profiles.

See [FEATURES.md](FEATURES.md), [SQLITE-TOOLS.md](SQLITE-TOOLS.md),
[SQLITE-SCHEMA.md](SQLITE-SCHEMA.md) and [TESTING.md](TESTING.md) for capability
limits and reproducible checks. This document defines the ownership boundary.
