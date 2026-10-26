# Grid API

`rowboat_core::grid` is side-effect-free. All fallible operations return
`Result<_, GridError>`; nothing executes SQL. UI must enforce table/view,
connection read-only, and result/session provenance before staging.

- `GridLimits::default()` bounds rows (10,000), columns (1,000), cells
  (1,000,000), and clipboard bytes (16 MiB); public fields allow tighter bounds.
- `GridPosition { row, column }`: zero-based coordinates.
- `GridRange::new(anchor, focus)`: normalize inclusive reversed corners,
  checked arithmetic. `from_size(origin, rows, columns)` rejects zero/overflow.
  Accessors: `start()`, `end()`, `rows()`, `columns()`;
  `validate(&QueryResult)` checks rectangular data and bounds.
- `GridData::new(Vec<Vec<Cell>>, GridLimits)` validates nonempty rectangular
  data and limits; `rows() -> &[Vec<Cell>]`, `height()`, `width()`.
- `ClipboardDialect::{Spreadsheet, Rowboat}`. Spreadsheet TSV uses standard
  CSV quoting (TAB delimiter), including doubled quotes and embedded CR/LF.
  Spreadsheet parsing **always** returns `Cell::Text`.
  Rowboat mode reserves `\N` for SQL NULL, `\x` + even valid hex for binary,
  and doubles an initial backslash in text. Empty text, literal `NULL`,
  literal `\N`, empty binary, and SQL NULL remain distinct. Unknown Rowboat
  escapes are rejected. Choose Rowboat explicitly on both copy and paste.
- `parse_tsv(text, dialect, limits) -> Result<GridData, GridError>`.
- `export_tsv(result, range, dialect, limits) -> Result<String, GridError>`.
  Spreadsheet export is display-oriented/lossy; Rowboat export is lossless.
- `stage_paste(result, structure, origin, data, existing, limits)` returns
  the complete replacement `Vec<RowChange>` (existing edits included).
  Paste shape must fit from origin; no implicit repetition or expansion.
- `stage_fill(result, structure, range, value, existing, limits)` fills a
  rectangle with one typed `Cell`; `stage_clear(result, structure, range,
  existing, limits)` fills SQL NULL. Both return complete replacement staging.

Staging requires result columns to map exactly by case-sensitive names to all
structure columns (order may differ), a complete non-NULL primary key, unique
result row keys, and full original snapshots. One update per row is generated;
existing edits to other columns and rows are preserved. Same-row original
snapshot mismatches, duplicate staged keys, and editing a staged deletion are
errors. No-op updates disappear. Existing inserts/deletes are retained.
NULL into a non-nullable column is rejected locally; other SQL type conversion
and constraints remain authoritative in `Database::apply_changes`. Bind the
returned list to the same table/result session, review, then explicitly apply.
