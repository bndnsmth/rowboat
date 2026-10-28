# Daily-driver tools

This guide describes implemented APIs and command workflows. Platform status,
limits and test procedures are in [FEATURES.md](FEATURES.md) and [TESTING.md](TESTING.md).

## Native workflow entry points

### SQL, results and exploration

- **Run current** executes the statement under the caret. The **Current** line
  shows its source range; **Reveal current** selects that range in the editor.
  **Query** also provides explicit Selection and All actions. Existing Ctrl+Enter
  keeps selection-or-whole-buffer behavior, and its button label reflects which
  will run. Current uses a dialect-aware local boundary service with no database
  preflight. Ambiguous quoting, unsupported stored-program grammar and client
  directives produce an explanation rather than expanding execution scope.
- **Go to error** navigates a reliable PostgreSQL source position when the editor
  still matches the executed SQL. Parameter substitution or unavailable engine
  coordinates preserve the error message without guessing a location. Current
  understands PostgreSQL dollar quotes and SQLite trigger bodies; MySQL stored
  program definitions require an explicit complete selection without DELIMITER.
- **Save query** names a new draft or updates the same saved-query identity.
  **Save query as…** creates a separate copy. **Saved** supports Rename, folders,
  optional connection associations and SQL/name/folder search. Associations are
  descriptive; opening a saved copy does not connect or execute it.
- **History** searches all retained executions (up to 1,000), with connection,
  success/failure and inclusive UTC-date filters. Open loads SQL for review in its
  original connection workspace.
- **Open SQL file**, **Save file**, and **Save file as…** retain a real file path
  and modified/saved state. Save checks the opened file's fingerprint before
  replacement; external changes preserve both the external file and editor draft.
  Save As requires a new destination. Version-5 session recovery retains document
  identity and baselines without rereading files or running SQL on restart.
- **Pin result** retains an immutable database-result snapshot, including the
  captured run source and connection. Use **Name**, **Left**, **Right**, then
  **Map / compare pins** to choose column mappings and composite keys. Comparison
  uses loaded rows only, performs no query and highlights added/removed/changed
  rows and changed cells. Duplicate/NULL keys and incompatible types are rejected.
  Table pages and interrupted/retention-limited results are explicitly partial.
- Pins are memory-only, at most eight and 32 MiB per copy, within the workspace's
  configured shared result budget. Active results, pins and comparison work all
  count toward that budget. **Close comparison** releases its output; **Close pin**
  releases a snapshot after any cancelled worker finishes using it. Pinned values
  are read-only and exclude staged edits. Live database actions use the live view.
- Table **Diagram** opens its declared foreign-key neighborhood. Search the loaded
  catalog to add tables, select/expand neighbors, pan/zoom and open Data/Structure.
  Qualified names distinguish schemas; composite and self-referencing keys retain
  their column identities. The view is bounded to 24 tables/64 links. SQLite
  incoming discovery scans at most 32 other tables per expansion and labels partial
  metadata. Close cancels further work; an in-flight metadata read must drain.
  Reopen to refresh; diagram layout persistence/export are follow-up work.
- **Inspect record** opens a dockable whole-row panel. Search fields and values,
  expand JSON objects/arrays, copy exact values or JSON Pointer paths, and use the
  existing cell/hex inspector for raw data. It follows live row or pinned-snapshot
  selection and labels staged table/query values separately from original data.
  JSON trees are bounded to 64 KiB, 32 levels and 512 nodes; incomplete previews
  are labeled and full original copying remains available. SQL NULL, JSON null,
  empty text, exact numbers and binary values retain their meaning.

### Workspaces and general controls

- **Close** needs no confirmation when idle with no pending edits or transactions.
  SQL tabs recover automatically; this does not save a named query or write an SQL
  file. Closing never stages input or applies edits to the database.
- With pending row edits, **Keep drafts & close** stores recoverable edits locally;
  **Discard edits & close** removes their recovery records while keeping SQL.
  **Cancel** returns to your work. Unstaged form input and parameters cannot be
  recovered; their dialog explicitly warns before discarding them.
- Open transactions require confirmation and roll back on close. Busy operations
  and recovery failures remain guarded. **Close without saving**, when recovery
  is unavailable, can lose recent SQL/edits and leave older recovery records.
  See [FEATURES.md](FEATURES.md) for recovery limits.
- The top band switches retained **connection workspaces**; the tab strip beneath
  it switches **SQL documents**. The query-tab arrow lists every open document.
- Top-right **Connections** manages saved profiles. **New workspace** opens this
  browser first: search, Open/Switch, Edit, New, confirmed Remove and Import/Export.
  **Save** edits a profile without connecting; cancelling leaves workspaces alone.
  **SQL draft workspace** is the explicit disconnected option. See `CONNECTIONS.md`.
- Sidebar **Database** returns to the selected table; **SQL editor** returns to
  retained SQL and results. **Saved** and **History** are separate local lists.
  **Save query** keeps a local copy. Opening either kind of entry loads SQL for
  review without executing it; history retains the original connection owner.
  Snippet templates remain a separate feature under the Query menu.
- **Tools** and **Preferences** remain visible at the bottom of the sidebar.
  Preferences groups appearance, typography, result limits and keyboard overrides; its Save
  footer stays visible while the form scrolls.
- **Preferences → Appearance** offers **System (default)** plus 22 embedded stock
  themes. System follows Omarchy live when available, otherwise desktop light/dark.
  Explicit choices persist across launches; Save applies across all windows and
  Cancel keeps the previous choice. See [THEMES.md](THEMES.md).

- Live work has a theme-accent wave while awaiting results and a compact sweeping
  rail during row delivery. Existing rows and Cancel remain usable; indicators
  disappear when work finishes or is cancelled. Reduced-motion settings use a
  static indicator. These are activity cues, not estimated percent complete.

SQLite backup/restore, streaming-import Apply and token-preserving SQL formatting
are described in [SQLITE-TOOLS.md](SQLITE-TOOLS.md).

- The grid's **SQL/fetch** time measures query execution/result streaming, including
  network reads. **Finished … total** measures time until the workspace is ready,
  including state/context checks. Hover **Timing ⓘ** for first result schema,
  queue wait, source proof, execution/streaming, cleanup, transaction and namespace verification. These are
  different scopes; neither should be interpreted as server-only CPU time.
- SQL completion appears **inline as you type identifiers or a qualified dot**.
  **Enter adds a newline** unless you deliberately choose with Up/Down first.
  Tab immediately inserts the best candidate; first Down selects the first item,
  after which Enter accepts it. Typing again resets that deliberate selection.
  Shift+Enter adds a newline; Escape dismisses; Ctrl+Space opens suggestions with
  the same passive behavior. Spaces, blank lines and bulk paste stay quiet.
  Exact names and shorter prefix matches rank first within relevant kinds.
  Ctrl+Enter executes the query and closes suggestions. Suggestions never execute
  the SQL buffer. **Query → Refresh SQL suggestions** refreshes cached metadata.
- Ordinary **Run** streams results without preparing edit-source metadata. Grid
  results initially support inspection/copy; this does not prevent intentionally
  running write SQL in a writable connection. Editing is an explicit separate run
  that captures a new, safe result snapshot; it never silently replays a previous
  query or certifies old results using later metadata.
- After an explicit editing run, supported single-base-table SELECT results with
  the complete primary key can be edited: double-click a cell or use **Selection →
  Edit row**, stage, then **Review → Apply**. Query Review displays the generated
  UPDATE and key/original predicates immediately. Aliases map to real source
  columns. Apply does not rerun SQL; results become stale until explicitly refreshed.
- A query-result notice explains unsupported editing (missing keys, joins,
  expressions, caller transactions or unavailable source metadata). MySQL source
  identity metadata may require PROCESS privilege. Query-origin editing supports
  updates, not inserts/deletes. Saved query-edit recovery is evidence-only because
  execution proof is not persisted; it cannot silently become table recovery.
- Click a **row number** to select the whole row; Shift-click another row number
  to extend across rows. With grid focus, **Shift+Space** selects the row and
  **Shift+Up/Down** extends. Use **Selection → Copy selected range** to copy it.
  Clicking an ordinary cell returns to cell selection.
- The explorer's **Database** picker on MySQL changes the actual SQL database.
  **Open database in new workspace** creates a separate connection/profile using
  the current credentials in memory. Existing workspaces keep their sessions.
  In-session switches do not overwrite the original profile's saved default DB.
- PostgreSQL's **Schema** picker filters explorer objects within the connected
  database; it does not change SQL `search_path`. Both pickers include accessible
  empty contexts. Query results retain a **Run started** context caption.
- Drag the horizontal editor/results divider to give SQL or data more room.
  Layout is retained while the workspace is open, including moves between windows.
- **Query**, **Selection**, **Transfer**, **Connection**, and **Tools** menus
  group secondary actions. Tab to a menu and press Enter; arrows select and Escape
  dismisses. Run/Cancel and table Refresh/Add/Filter/Review remain directly visible.
- Click a cell or press **F6** to focus the grid; arrows navigate, Shift+arrows
  extend selection, and Enter edits writable table cells or inspects query cells.
  F6 toggles between query editor and visible results, selecting the first visible
  cell when needed. **PageUp/PageDown** move by the visible page; **Home/End** move
  within the row; **Ctrl+Home/End** reach the first/last cell. Shift extends the
  selection. Navigation scrolls the selected cell into view on both axes.
  Text fields retain their own keys. Escape from an inspector restores grid focus;
  cancelling the command palette restores the initiating editor/grid focus.
- Duplicate query-column names are sorted by their actual column position, so
  joined `id` columns can be sorted independently. Copy operates on the selected
  rectangle, including displayed staged values, rather than cloning every result.
- In a multiple-statement query, each result reports its own completion and
  affected-row count. A later error/cancellation does not imply an earlier write
  failed or was rolled back. Statement completion is not transaction commit;
  transaction controls and batch errors remain visible. SQL is never replayed
  automatically to recover a result.
- Review shows row keys and old→new values first. Expand **SQL & originals** for
  evidence; **Copy SQL** and full JSON copying retain complete values. Apply still
  checks the bound target and original values before writing.
- **Ctrl+Shift+P** opens the searchable command palette; **Ctrl+P** uses the same
  search for commands, tables, saved SQL and connections. Disabled actions stay
  visible. Saved queries open for review rather than executing immediately.
- **Ctrl+,** opens application preferences. Independent editor/grid font sizes, grid row height,
  page size, retained-query row limit and application shortcuts are stored in
  Rowboat's app-data directory. New limits apply on the next query/page refresh.
  Ordinary query results also have a configurable data budget: 256 MiB by default,
  adjustable from 16 to 512 MiB, shared across each workspace's retained query/table
  results, pins and comparison reservations. Query streams receive half the
  remaining allowance to leave delivery headroom. This is retained data accounting,
  not a process-RAM ceiling; allocation/rendering overhead adds memory. Explicit
  editing runs also enforce the smaller
  64 MiB proof budget. Retained rows are not the full result count when a row/byte
  limit is reached; use streaming export when all data is needed. Reading/draining
  the server response can continue after retention stops, so total query time can
  exceed time to first displayed results. Changing limits affects the next run.
- Table filtering supports nested AND/OR groups and typed comparisons, NULL,
  IN and BETWEEN. Editing a draft does not change displayed rows until **Apply
  filter**. Saved filters belong to their connection/table.
- **Object explorer** groups native objects by schema and kind. Inspect a full
  definition before selecting a supported typed edit, **Review generated SQL**,
  then **Apply reviewed object change**. Changed definitions invalidate review.
- **Metrics dashboards** stores named layouts and refresh intervals. **Sample
  now** takes a real sample; periodic refresh pauses for operations, transactions
  and inactive workspaces. Counter rates require two compatible samples;
  missing/reset intervals are shown explicitly. Samples are not persisted.
- **New custom SQL metric…** saves a connection-owned definition, disabled by
  default. Only explicit **Sample once** executes custom SQL; dashboard timers do
  not. Sampling rejects busy/active or unknown transactions, uses server read-only
  enforcement, caps output and execution time, and preserves exact numeric text.
  Values stay in memory. Read-only database transactions do not sandbox external
  effects of server functions.
- Inline grid drafts accept **Enter** for newline, **Ctrl+Enter** to stage and
  **Escape** to cancel. **Alt+PageUp/PageDown** switches result sets, including empty
  results. **Move out / Move here** keeps the live session, transaction, query and
  staged state while moving between windows.
- **Ctrl+Shift+Space** expands a saved snippet trigger; **Tab/Shift+Tab** moves
  forward/back through linked placeholders. Completion reloads the actual PG
  search path or MySQL current database on each explicit request.

Native acceptance evidence is recorded separately from these UI entry points.

## Compound filters and relationships

`rowboatctl browse <table> --schema <namespace> --filter filter.json` runs a
parameterized table browse with a nested expression. For example:

```json
{"Or":[
  {"And":[
    {"Leaf":{"column":"city","op":"Eq","value":{"Text":"Portland"}}},
    {"Leaf":{"column":"id","op":"Greater","value":{"Text":"1"}}}
  ]},
  {"Leaf":{"column":"city","op":"IsNull","value":"Null"}}
]}
```

`In` and `Between` are also supported. Filters accept typed values, not raw SQL.
Nesting, nodes, values and payload sizes are bounded. IN uses null-aware set
membership, with NOT IN its complement; BETWEEN rejects NULL endpoints. Use
`relationships <table> --schema <namespace>` for incoming/outgoing foreign keys
with exact ordered source/reference columns, including composite keys.

## Object definitions and reviewed changes

```sh
rowboatctl --connection 'Development' object-definition view active_customers --schema public
rowboatctl --connection 'Development' object-review view active_customers \
  --schema public --patch change.json --format sql
```

`change.json` contains a typed patch, for example:

```json
{"ViewQuery":{"query":"SELECT id, name FROM public.customers WHERE id > 1"}}
```

`object-review` only produces SQL/JSON for review. It does not execute the plan.
PostgreSQL overloaded routines require the exact catalog identity string through
`--signature`; triggers can specify their parent with `--table`. Ambiguity errors
rather than selecting an arbitrary object. MySQL inspection uses native SHOW
CREATE definitions, including creation settings, instead of substituting a body.

Supported edits preserve the original object's attributes: ordinary view query,
PostgreSQL SQL/plpgsql quoted routine body, routine comment/security, PostgreSQL
trigger enable state, and typed PostgreSQL sequence parameters. Additional typed
patches provide MySQL routine/trigger body replacement and PostgreSQL materialized
view refresh, index creation and supported query replacement:

```json
{"MySqlBody":{"body":"RETURN n + 2"}}
```

```json
{"MaterializedRefresh":{"concurrently":true,"with_data":true}}
```

MySQL replacement is a non-atomic DROP/CREATE workflow. JSON review includes the
manual recovery plan; native Apply reports acknowledged steps and whether the
object is present, absent or unknown after failure. Recovery is not automatic.
The supported body grammar accepts bounded DML/SET/RETURN blocks, not arbitrary
IF/LOOP/DECLARE/dynamic SQL. Creation settings, privileges and trigger order are
checked and preserved where supported. PostgreSQL materialized-view replacement
uses a transaction and blocks dependencies/attributes it cannot faithfully retain.
See [schema tools](SCHEMA-TOOLS.md) for supported operations and fidelity limits.

### Scoped schema comparisons

The ordinary `compare-live` remains a conservative full-table/schema comparison.
Two explicit scopes extend it:

```sh
rowboatctl --connection 'Destination' compare-live --source-connection 'Source' \
  --source-schema source_db --destination-schema target_db \
  --columns-table invoices --format sql
rowboatctl --connection 'Destination' compare-live --source-connection 'Source' \
  --source-schema source_schema --destination-schema target_schema \
  --object-kind sequence --object-name counter --format json
```

`--columns-table` compares advanced MySQL columns, including supported generated
expressions and ENUM/SET/DATETIME/TINYINT attributes, on an existing same-name
table. Destination-only columns, keys, and other objects are retained. Storage-mode
conversion, reordering, ambiguous expression rewriting and unknown attributes
block the plan. This does not claim whole-schema equivalence.

`--object-kind` compares supported routine comment/security, PostgreSQL trigger
enabled state, or sequence declaration attributes; bodies/queries and sequence
runtime values remain local to the destination. These CLI commands only review;
they do not execute changes.

Use `--definition-scope view-query` with `--object-kind view` to compare supported
view queries, or `--definition-scope routine-body` for supported functions/procedures:

```sh
rowboatctl --connection 'Destination' compare-live --source-connection 'Source' \
  --source-schema source_schema --destination-schema target_schema \
  --object-kind view --object-name active_customers --definition-scope view-query
```

The report includes dependency evidence, immutable endpoint snapshots, differences
and blockers. Only bounded proven namespace mapping is supported; function calls,
joins, dynamic SQL and unsupported dependencies block. Destination attributes are
retained. PostgreSQL routine signatures are exact catalog strings, including names
when present (for example `--signature 'n integer'`). CLI output remains review-only.
The GUI offers **View query / Function body / Procedure body**. Both original
definitions and dependency evidence are displayed; Apply rechecks both physical
sessions. PostgreSQL visible custom operators or any database-wide custom casts
conservatively block these scopes. MySQL routine replacement reports non-atomic
steps and a manual recovery plan. No automatic recovery replay occurs.

## Query plans

```sh
rowboatctl --connection 'Development' explain 'SELECT * FROM customers WHERE id=1' --format tree
rowboatctl --connection 'Development' explain 'SELECT * FROM customers WHERE id=1' --format json
```

Tree output displays native operations, relations, estimates and predicates.
JSON includes the normalized tree and complete original JSON, preserving exact
numeric representations and unknown fields. By default plain EXPLAIN is used;
estimated and actual fields remain distinct. The default `raw`
format preserves the previous CLI result-array output.

Use **`--analyze`** to execute a SELECT/WITH query once and collect actual rows,
loops and timing alongside estimates:

```sh
rowboatctl --connection 'Development' explain \
  'SELECT * FROM customers WHERE id=1' --analyze --format tree
```

PostgreSQL returns JSON; MySQL 8.4 returns TREE. Normalized JSON exposes
`mode`, `raw_format`, `raw_report`, `timing`, and per-node `estimates`/`actual`.
Native numeric spelling is retained. Cost units are not milliseconds, and node
timings must not be summed. ANALYZE executes functions too: side effects follow
the connection's real transaction/read-only rules, with no automatic rollback.
Explicit writes, data-changing CTEs, INTO and locking clauses are currently
rejected by this query-profiling entry point. Ctrl+C requests server cancellation.

Configure **Settings → AI**, then use the top-right sparkles button for the
resizable **Workspace AI** sidebar. Ask questions or author SQL using the selected
database/table/query/result context; values are excluded unless shared explicitly.
**Review context** allows redaction before Send. Suggestions open in new SQL tabs
without execution. The plan viewer retains **Ask AI** and **Ask in sidebar**.
**Allow agent investigation** is a separate per-connection trusted opt-in: it
enables agent shell/read tools with rowboatctl read-only instructions, not an
enforced sandbox. See [AI assistance](AI.md) for scope, privacy and CLI limitations.

## Effective access

```sh
rowboatctl --connection 'Development' principals --limit 100 --offset 0
rowboatctl --connection 'Development' access analyst --schema public --table customers
rowboatctl --connection 'MySQL' access analyst --host '%' --schema app \
  --table customers --column name --context default-roles
```

The matrix reports `Granted`, `Denied`, or `Unknown`, with direct/inherited/
PUBLIC/owner/superuser provenance where available. Role activation matters:
MySQL `principal` is direct-only; `default-roles` models login defaults;
`current-session` inspects the connected principal's enabled roles;
`available-roles` is a hypothetical union rather than a claim about login access.
PostgreSQL inherited access and SET ROLE availability are distinguished.
Namespace privileges accompany table/column requests. Object ACL rights do not
override row-security, login restrictions or security-definer semantics.

`access-review` takes the same account/object/context arguments plus
`--change change.json --format sql`. It generates a plan without applying it:

```json
{"object":{"Table":{"schema":"public","name":"customers"}},
 "privileges":["Select"],"revoke":true,"grant_option":false}
```

Revoking a direct grant can leave inherited or broader privileges effective.

## Contextual completion and snippet templates

```sh
rowboatctl --connection 'Development' complete-sql \
  'SELECT c.na FROM public.customers c' --caret 11 --schema public
rowboatctl snippet-expand customer_lookup --value column=name --value table=customers
```

`complete-sql` reads the live session search path and fetches its bounded catalog,
then returns candidates, the resolved path and a UTF-8 byte replacement range.
`--schema` adds a namespace without omitting earlier path entries that can shadow
its names. PostgreSQL uses resolved `current_schemas(true)`; MySQL uses the actual
current database. It recognizes aliases,
local query scopes, and supported CTE/derived-table projections; literals/comments
suppress suggestions. It never executes the supplied SQL. `--caret` defaults to
the end; `--limit` is 1–200. The original `complete PREFIX` command is retained.

Snippet templates are separate from literal SQL favorites. `${name:default}` or
`${1:default}` defines a placeholder, repeated names share the initial value,
`${0}` marks the final caret, and `$$` emits a literal dollar. Expansion returns
literal editor text and tabstop byte ranges; values are not SQL-escaped or run.
The offline `snippet-expand` command reads saved templates by id/name/trigger and
does not change app state.

## Custom scalar metrics

```sh
rowboatctl --connection 'Development' metric-sample 'SELECT count(*) FROM customers'
```

This explicit one-shot operation uses a server-enforced read-only transaction and
rejects busy sessions and caller transactions. It performs no retry/background
refresh or saved-state update. JSON distinguishes an exact decimal string, Empty,
and Invalid; execution errors exit unsuccessfully. Sampling retains at most two
rows/one column/64KiB output and has a five-second execution deadline. Oversized
wire packets may still be received before rejection. READ ONLY does not sandbox
function external/session effects. Saved custom card definitions contain no
samples and are disabled by default; sampling them is an explicit user action.

## Progressive query output

```sh
rowboatctl --connection 'Development' query-stream \
  --sql 'SELECT * FROM customers' --batch-rows 256 --limit 10000
```

Output is newline-delimited JSON events: `schema`, `rows`, `result_end`, followed
by `complete` on success or `error` on failure. **EOF alone is not success.**
An error can follow partial rows or earlier successful statements. Raw SQL
batches are not implicitly atomic. Ctrl+C requests cancellation; completed
server effects remain subject to normal transaction semantics.

Rows and byte totals are bounded by default; ResultEnd reports truncation.
`--unlimited` explicitly removes total emission limits while retaining bounded
batches/queues, maximum cell/row sizes, result count and stalled-consumer timeout.
`--limit 0` emits no rows but still executes and drains the query. Empty result
metadata unavailable from SQLx is explicitly marked incomplete, never obtained
by replaying the query.

## Large-file transfers

```sh
rowboatctl --connection 'Development' export-stream customers --format csv --output customers.csv
rowboatctl --connection 'Development' import-stream customers customers.csv --format csv
rowboatctl --connection 'Development' import-stream customers customers.csv --format csv --apply
rowboatctl --connection 'Development' import-stream customers customers.json --format json-array --apply
```

Streaming formats are CSV, JSON Lines (`jsonl`), JSON arrays (`json-array`, alias
`json`, import only), and INSERT SQL (`sql`, export only). JSON Lines is one row
object per line; array elements may span lines. Both JSON import formats reject
duplicate fields, malformed tails and unsupported nested data, while preserving
exact numbers, null, empty strings and `{"$binary":"hex"}` values. Only CSV accepts
a leading BOM. CSV retains Rowboat's lossless NULL/binary/backslash encoding.

Export writes a private temporary file, publishes it only after successful
completion, and refuses to overwrite an existing destination. It uses the
supplied live connection, including its current session/transaction context.

Import preview scans the complete file and hashes it but retains only a bounded
sample. Apply re-reads the same file, verifies metadata and full fingerprint,
and commits only after successful EOF and server validation of every batch.
Late parsing/constraint/fingerprint errors and unfinished producers roll back
the owned transaction. Existing transactions and read-only/nontransactional
targets are rejected, including MySQL temporary tables that shadow a permanent
target. Targets with user triggers/rewrite rules are also rejected where their
side effects cannot satisfy the atomic-import contract. This is a bounded sample
review, not a full in-memory row review. Cancellation before Finish rolls back;
after Finish starts commit, the caller waits for the actual outcome. A lost
connection during commit can leave the outcome unknown. Native SQL backups remain
the separate backup/restore workflow.
