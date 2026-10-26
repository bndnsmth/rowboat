//! Pure rectangular selection, explicit clipboard dialects, and atomic edit staging.
//! See `GRID_API.md`. Callers own table/session provenance and UI permissions.

use crate::{Cell, Column, QueryResult, RowChange, Structure};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridError {
    Empty,
    Overflow,
    Bounds,
    Ragged,
    Limit,
    MalformedTsv { offset: usize },
    InvalidEscape,
    Mapping(String),
    MissingPrimaryKey,
    DuplicateKey,
    StaleOriginal,
    DeletedRow,
    NonNullable(String),
}

impl fmt::Display for GridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("Grid rectangle must not be empty"),
            Self::Overflow => f.write_str("Grid dimensions overflow"),
            Self::Bounds => f.write_str("Grid rectangle exceeds result bounds"),
            Self::Ragged => f.write_str("Grid rows must have equal column counts"),
            Self::Limit => f.write_str("Grid operation exceeds configured limits"),
            Self::MalformedTsv { offset } => write!(f, "Malformed TSV quoting at byte {offset}"),
            Self::InvalidEscape => f.write_str("Invalid Rowboat clipboard escape or binary hex"),
            Self::Mapping(name) => write!(f, "Missing, duplicate, or unknown column: {name}"),
            Self::MissingPrimaryKey => f.write_str("Complete non-NULL primary key required"),
            Self::DuplicateKey => f.write_str("Duplicate original primary key"),
            Self::StaleOriginal => {
                f.write_str("Staged original does not match the result snapshot")
            }
            Self::DeletedRow => f.write_str("Cannot edit a row staged for deletion"),
            Self::NonNullable(name) => write!(f, "Column {name} does not allow NULL"),
        }
    }
}

impl std::error::Error for GridError {}

type Result<T> = std::result::Result<T, GridError>;

#[derive(Debug, Clone, Copy)]
pub struct GridLimits {
    pub max_rows: usize,
    pub max_columns: usize,
    pub max_cells: usize,
    pub max_bytes: usize,
}

impl Default for GridLimits {
    fn default() -> Self {
        Self {
            max_rows: 10_000,
            max_columns: 1_000,
            max_cells: 1_000_000,
            max_bytes: 16 * 1024 * 1024,
        }
    }
}

impl GridLimits {
    fn shape(self, rows: usize, columns: usize) -> Result<()> {
        let cells = rows.checked_mul(columns).ok_or(GridError::Overflow)?;
        if rows > self.max_rows || columns > self.max_columns || cells > self.max_cells {
            return Err(GridError::Limit);
        }
        Ok(())
    }

    fn bytes(self, bytes: usize) -> Result<()> {
        if bytes > self.max_bytes {
            return Err(GridError::Limit);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPosition {
    pub row: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridRange {
    start: GridPosition,
    end: GridPosition,
}

impl GridRange {
    /// Both corners are inclusive. Reversed row/column anchors are normalized.
    pub fn new(anchor: GridPosition, focus: GridPosition) -> Result<Self> {
        let start = GridPosition {
            row: anchor.row.min(focus.row),
            column: anchor.column.min(focus.column),
        };
        let end = GridPosition {
            row: anchor.row.max(focus.row),
            column: anchor.column.max(focus.column),
        };
        // Require representable exclusive ends as well as dimensions.
        end.row.checked_add(1).ok_or(GridError::Overflow)?;
        end.column.checked_add(1).ok_or(GridError::Overflow)?;
        let range = Self { start, end };
        range
            .rows()
            .checked_mul(range.columns())
            .ok_or(GridError::Overflow)?;
        Ok(range)
    }

    pub fn from_size(origin: GridPosition, rows: usize, columns: usize) -> Result<Self> {
        if rows == 0 || columns == 0 {
            return Err(GridError::Empty);
        }
        Self::new(
            origin,
            GridPosition {
                row: origin
                    .row
                    .checked_add(rows - 1)
                    .ok_or(GridError::Overflow)?,
                column: origin
                    .column
                    .checked_add(columns - 1)
                    .ok_or(GridError::Overflow)?,
            },
        )
    }

    pub fn start(self) -> GridPosition {
        self.start
    }
    pub fn end(self) -> GridPosition {
        self.end
    }
    pub fn rows(self) -> usize {
        self.end.row - self.start.row + 1
    }
    pub fn columns(self) -> usize {
        self.end.column - self.start.column + 1
    }

    pub fn validate(self, result: &QueryResult) -> Result<()> {
        validate_rows(&result.rows, result.columns.len())?;
        if self.end.row >= result.rows.len() || self.end.column >= result.columns.len() {
            return Err(GridError::Bounds);
        }
        Ok(())
    }
}

fn validate_rows(rows: &[Vec<Cell>], width: usize) -> Result<()> {
    if rows.iter().any(|row| row.len() != width) {
        return Err(GridError::Ragged);
    }
    Ok(())
}

fn cell_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Null => 0,
        Cell::Text(text) => text.len(),
        Cell::Binary(bytes) => bytes.len(),
    }
}

fn check_bytes<'a>(cells: impl Iterator<Item = &'a Cell>, limits: GridLimits) -> Result<()> {
    let mut bytes = 0usize;
    for cell in cells {
        bytes = bytes
            .checked_add(cell_bytes(cell))
            .ok_or(GridError::Overflow)?;
        limits.bytes(bytes)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridData {
    rows: Vec<Vec<Cell>>,
}

impl GridData {
    pub fn new(rows: Vec<Vec<Cell>>, limits: GridLimits) -> Result<Self> {
        let width = rows.first().map_or(0, Vec::len);
        if rows.is_empty() || width == 0 {
            return Err(GridError::Empty);
        }
        limits.shape(rows.len(), width)?;
        validate_rows(&rows, width)?;
        check_bytes(rows.iter().flatten(), limits)?;
        Ok(Self { rows })
    }
    pub fn rows(&self) -> &[Vec<Cell>] {
        &self.rows
    }
    pub fn height(&self) -> usize {
        self.rows.len()
    }
    pub fn width(&self) -> usize {
        self.rows[0].len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardDialect {
    /// Every parsed cell is text; export uses the human-readable display value.
    Spreadsheet,
    /// Explicit lossless NULL/binary/backslash encoding, followed by TSV quoting.
    Rowboat,
}

fn decode(text: String, dialect: ClipboardDialect) -> Result<Cell> {
    if dialect == ClipboardDialect::Spreadsheet || !text.starts_with('\\') {
        return Ok(Cell::Text(text));
    }
    if text == "\\N" {
        return Ok(Cell::Null);
    }
    if let Some(hex) = text.strip_prefix("\\x") {
        return hex::decode(hex)
            .map(Cell::Binary)
            .map_err(|_| GridError::InvalidEscape);
    }
    if let Some(text) = text.strip_prefix("\\\\") {
        return Ok(Cell::Text(format!("\\{text}")));
    }
    Err(GridError::InvalidEscape)
}

/// Strict CSV quoting with TAB separators. CRLF, LF, and CR delimit records;
/// inside quotes their exact bytes are preserved. A final record ending is optional.
pub fn parse_tsv(text: &str, dialect: ClipboardDialect, limits: GridLimits) -> Result<GridData> {
    limits.bytes(text.len())?;
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    // 0 = field start, 1 = unquoted, 2 = quoted, 3 = closing quote.
    let mut state = 0;
    let mut chars = text.char_indices().peekable();
    let mut ended_record = false;
    while let Some((offset, ch)) = chars.next() {
        ended_record = false;
        if state == 2 {
            if ch == '"' {
                state = 3;
            } else {
                field.push(ch);
            }
            continue;
        }
        if state == 3 && ch == '"' {
            field.push('"');
            state = 2;
            continue;
        }
        if matches!(ch, '\t' | '\r' | '\n') {
            row.push(decode(std::mem::take(&mut field), dialect)?);
            limits.shape(
                rows.len().checked_add(1).ok_or(GridError::Overflow)?,
                row.len(),
            )?;
            state = 0;
            if ch != '\t' {
                if ch == '\r' && chars.peek().is_some_and(|(_, c)| *c == '\n') {
                    chars.next();
                }
                if rows
                    .first()
                    .is_some_and(|r: &Vec<Cell>| r.len() != row.len())
                {
                    return Err(GridError::Ragged);
                }
                rows.push(std::mem::take(&mut row));
                ended_record = true;
            }
        } else if ch == '"' && state == 0 {
            state = 2;
        } else if ch == '"' || state == 3 {
            return Err(GridError::MalformedTsv { offset });
        } else {
            field.push(ch);
            state = 1;
        }
    }
    if state == 2 {
        return Err(GridError::MalformedTsv { offset: text.len() });
    }
    if !ended_record {
        row.push(decode(field, dialect)?);
        rows.push(row);
    }
    GridData::new(rows, limits)
}

pub fn export_tsv(
    result: &QueryResult,
    range: GridRange,
    dialect: ClipboardDialect,
    limits: GridLimits,
) -> Result<String> {
    range.validate(result)?;
    limits.shape(range.rows(), range.columns())?;
    let mut output = String::new();
    for r in range.start.row..=range.end.row {
        if r > range.start.row {
            push_bounded(&mut output, "\n", limits)?;
        }
        for c in range.start.column..=range.end.column {
            if c > range.start.column {
                push_bounded(&mut output, "\t", limits)?;
            }
            let cell = &result.rows[r][c];
            limits.bytes(cell_bytes(cell))?;
            let text = match (dialect, cell) {
                (ClipboardDialect::Spreadsheet, _) => cell.display(),
                (_, Cell::Null) => "\\N".into(),
                (_, Cell::Binary(bytes)) => format!("\\x{}", hex::encode(bytes)),
                (_, Cell::Text(text)) if text.starts_with('\\') => format!("\\{text}"),
                (_, Cell::Text(text)) => text.clone(),
            };
            // Quote empty strings too: this preserves a final all-empty record.
            if text.is_empty() || text.contains(['\t', '\r', '\n', '"']) {
                push_bounded(&mut output, "\"", limits)?;
                for part in text.split_inclusive('"') {
                    push_bounded(&mut output, part, limits)?;
                    if part.ends_with('"') {
                        push_bounded(&mut output, "\"", limits)?;
                    }
                }
                push_bounded(&mut output, "\"", limits)?;
            } else {
                push_bounded(&mut output, &text, limits)?;
            }
        }
    }
    Ok(output)
}

fn push_bounded(output: &mut String, text: &str, limits: GridLimits) -> Result<()> {
    limits.bytes(
        output
            .len()
            .checked_add(text.len())
            .ok_or(GridError::Overflow)?,
    )?;
    output.push_str(text);
    Ok(())
}

type NamedCell = (String, Cell);
type NamedCells = Vec<NamedCell>;
type RowKey = Vec<(u8, Vec<u8>)>;

struct Mapping<'a> {
    columns: &'a [Column],
    result_indices: Vec<usize>,
    keys: Vec<usize>,
}

impl<'a> Mapping<'a> {
    fn new(result: &QueryResult, structure: &'a Structure) -> Result<Self> {
        let mut names = BTreeSet::new();
        for column in &structure.columns {
            if !names.insert(&column.name) {
                return Err(GridError::Mapping(column.name.clone()));
            }
        }
        let mut positions = BTreeMap::new();
        for (index, column) in result.columns.iter().enumerate() {
            if !names.contains(&column.name) || positions.insert(&column.name, index).is_some() {
                return Err(GridError::Mapping(column.name.clone()));
            }
        }
        let result_indices = structure
            .columns
            .iter()
            .map(|column| {
                positions
                    .get(&column.name)
                    .copied()
                    .ok_or_else(|| GridError::Mapping(column.name.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        let keys = structure
            .columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.primary_key.then_some(i))
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Err(GridError::MissingPrimaryKey);
        }
        Ok(Self {
            columns: &structure.columns,
            result_indices,
            keys,
        })
    }

    fn original(&self, row: &[Cell]) -> NamedCells {
        self.columns
            .iter()
            .zip(&self.result_indices)
            .map(|(column, &index)| (column.name.clone(), row[index].clone()))
            .collect()
    }

    fn key(&self, original: &[(String, Cell)]) -> Result<RowKey> {
        self.keys
            .iter()
            .map(|&i| match &original[i].1 {
                Cell::Null => Err(GridError::MissingPrimaryKey),
                Cell::Text(text) => Ok((0, text.as_bytes().to_vec())),
                Cell::Binary(bytes) => Ok((1, bytes.clone())),
            })
            .collect()
    }

    fn named(&self, values: &[(String, Cell)], full: bool, check_null: bool) -> Result<NamedCells> {
        let mut found = BTreeMap::new();
        for (name, value) in values {
            let column = self
                .columns
                .iter()
                .find(|column| &column.name == name)
                .ok_or_else(|| GridError::Mapping(name.clone()))?;
            if found.insert(name, value).is_some() {
                return Err(GridError::Mapping(name.clone()));
            }
            if check_null {
                nullable(column, value)?;
            }
        }
        let mut ordered = Vec::new();
        for column in self.columns {
            if let Some(value) = found.get(&column.name) {
                ordered.push((column.name.clone(), (*value).clone()));
            } else if full {
                return Err(GridError::Mapping(column.name.clone()));
            }
        }
        Ok(ordered)
    }
}

fn nullable(column: &Column, value: &Cell) -> Result<()> {
    if *value == Cell::Null && (!column.nullable || column.primary_key) {
        return Err(GridError::NonNullable(column.name.clone()));
    }
    Ok(())
}

/// Returns the complete replacement staging list; never mutates `existing` or the result.
pub fn stage_paste(
    result: &QueryResult,
    structure: &Structure,
    origin: GridPosition,
    data: &GridData,
    existing: &[RowChange],
    limits: GridLimits,
) -> Result<Vec<RowChange>> {
    let range = GridRange::from_size(origin, data.height(), data.width())?;
    limits.shape(data.height(), data.width())?;
    check_bytes(data.rows.iter().flatten(), limits)?;
    stage(
        result,
        structure,
        range,
        |row, col| &data.rows[row][col],
        existing,
        limits,
    )
}

pub fn stage_fill(
    result: &QueryResult,
    structure: &Structure,
    range: GridRange,
    value: Cell,
    existing: &[RowChange],
    limits: GridLimits,
) -> Result<Vec<RowChange>> {
    limits.shape(range.rows(), range.columns())?;
    let cells = range
        .rows()
        .checked_mul(range.columns())
        .ok_or(GridError::Overflow)?;
    limits.bytes(
        cell_bytes(&value)
            .checked_mul(cells)
            .ok_or(GridError::Overflow)?,
    )?;
    stage(result, structure, range, |_, _| &value, existing, limits)
}

pub fn stage_clear(
    result: &QueryResult,
    structure: &Structure,
    range: GridRange,
    existing: &[RowChange],
    limits: GridLimits,
) -> Result<Vec<RowChange>> {
    stage_fill(result, structure, range, Cell::Null, existing, limits)
}

fn stage<'a>(
    result: &QueryResult,
    structure: &Structure,
    range: GridRange,
    value: impl Fn(usize, usize) -> &'a Cell,
    existing: &[RowChange],
    limits: GridLimits,
) -> Result<Vec<RowChange>> {
    range.validate(result)?;
    limits.shape(result.rows.len(), result.columns.len())?;
    check_bytes(result.rows.iter().flatten(), limits)?;
    limits.shape(existing.len(), structure.columns.len())?;
    check_bytes(change_cells(existing), limits)?;
    let mapping = Mapping::new(result, structure)?;
    let mut result_keys = BTreeSet::new();
    for row in &result.rows {
        if !result_keys.insert(mapping.key(&mapping.original(row))?) {
            return Err(GridError::DuplicateKey);
        }
    }
    let mut staged = Vec::new();
    let mut staged_keys = BTreeMap::new();
    for change in existing {
        let normalized = match change {
            RowChange::Insert { values } => RowChange::Insert {
                values: mapping.named(values, false, true)?,
            },
            RowChange::Update { original, values } => RowChange::Update {
                original: mapping.named(original, true, false)?,
                values: mapping.named(values, false, true)?,
            },
            RowChange::Delete { original } => RowChange::Delete {
                original: mapping.named(original, true, false)?,
            },
        };
        if let RowChange::Update { original, .. } | RowChange::Delete { original } = &normalized
            && staged_keys
                .insert(mapping.key(original)?, staged.len())
                .is_some()
        {
            return Err(GridError::DuplicateKey);
        }
        staged.push(normalized);
    }
    for r in range.start.row..=range.end.row {
        let original = mapping.original(&result.rows[r]);
        let key = mapping.key(&original)?;
        let index = if let Some(&index) = staged_keys.get(&key) {
            index
        } else {
            let index = staged.len();
            staged.push(RowChange::Update {
                original: original.clone(),
                values: Vec::new(),
            });
            staged_keys.insert(key, index);
            index
        };
        let values = match &mut staged[index] {
            RowChange::Update {
                original: previous,
                values,
            } => {
                if previous != &original {
                    return Err(GridError::StaleOriginal);
                }
                values
            }
            RowChange::Delete { .. } => return Err(GridError::DeletedRow),
            RowChange::Insert { .. } => unreachable!("only updates/deletes have original keys"),
        };
        for c in range.start.column..=range.end.column {
            let name = &result.columns[c].name;
            let cell = value(r - range.start.row, c - range.start.column);
            // Mapping already proved each result name appears exactly once in structure.
            let column = mapping
                .columns
                .iter()
                .find(|column| &column.name == name)
                .ok_or_else(|| GridError::Mapping(name.clone()))?;
            nullable(column, cell)?;
            if let Some((_, previous)) = values.iter_mut().find(|(n, _)| n == name) {
                *previous = cell.clone();
            } else {
                values.push((name.clone(), cell.clone()));
            }
        }
        values.retain(|(name, cell)| !original.iter().any(|(n, v)| n == name && v == cell));
        *values = mapping.named(values, false, true)?;
    }
    staged
        .retain(|change| !matches!(change, RowChange::Update { values, .. } if values.is_empty()));
    limits.shape(staged.len(), structure.columns.len())?;
    check_bytes(change_cells(&staged), limits)?;
    Ok(staged)
}

fn change_cells(changes: &[RowChange]) -> impl Iterator<Item = &Cell> {
    changes.iter().flat_map(|change| {
        let (original, values): (&[NamedCell], &[NamedCell]) = match change {
            RowChange::Insert { values } => (&[], values),
            RowChange::Update { original, values } => (original, values),
            RowChange::Delete { original } => (original, &[]),
        };
        original.iter().chain(values).map(|(_, value)| value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Cell {
        Cell::Text(value.into())
    }
    fn pos(row: usize, column: usize) -> GridPosition {
        GridPosition { row, column }
    }
    fn limits() -> GridLimits {
        GridLimits::default()
    }
    fn column(name: &str, primary_key: bool, nullable: bool) -> Column {
        Column {
            name: name.into(),
            data_type: "text".into(),
            nullable,
            primary_key,
            default_value: None,
        }
    }
    fn fixture() -> (QueryResult, Structure) {
        let columns = vec![
            column("tenant", true, false),
            column("id", true, false),
            column("note", false, true),
            column("label", false, false),
        ];
        let result = QueryResult {
            columns: vec![
                columns[3].clone(),
                columns[2].clone(),
                columns[1].clone(),
                columns[0].clone(),
            ],
            rows: vec![
                vec![text("A"), text("before"), text("1"), text("tenant")],
                vec![text("B"), Cell::Null, text("2"), text("tenant")],
            ],
            ..Default::default()
        };
        (
            result,
            Structure {
                columns,
                ..Default::default()
            },
        )
    }

    #[test]
    fn grid_lossless_tsv_preserves_utf8_null_binary_quotes_and_final_empty_row() {
        let values = vec![
            Cell::Null,
            text("NULL"),
            text(""),
            text("\\N"),
            text("\\xdead"),
            text("\\"),
            text("雪\t\"a\"\r\nb\rc\n"),
            Cell::Binary(vec![0, 255, 34, 10]),
            Cell::Binary(vec![]),
        ];
        let result = QueryResult {
            columns: (0..values.len())
                .map(|i| column(&i.to_string(), false, true))
                .collect(),
            rows: vec![values.clone(), vec![text(""); values.len()]],
            ..Default::default()
        };
        let range = GridRange::new(pos(1, values.len() - 1), pos(0, 0)).unwrap();
        let encoded = export_tsv(&result, range, ClipboardDialect::Rowboat, limits()).unwrap();
        assert_eq!(
            parse_tsv(&encoded, ClipboardDialect::Rowboat, limits())
                .unwrap()
                .rows(),
            result.rows
        );
        assert!(encoded.contains("\\N\tNULL\t\"\"\t\\\\N"));
        let one_empty = QueryResult {
            columns: vec![column("x", false, true)],
            rows: vec![vec![text("")], vec![text("")]],
            ..Default::default()
        };
        let encoded = export_tsv(
            &one_empty,
            GridRange::new(pos(0, 0), pos(1, 0)).unwrap(),
            ClipboardDialect::Rowboat,
            limits(),
        )
        .unwrap();
        assert_eq!(encoded, "\"\"\n\"\"");
        assert_eq!(
            parse_tsv(&encoded, ClipboardDialect::Rowboat, limits())
                .unwrap()
                .height(),
            2
        );
    }

    #[test]
    fn grid_spreadsheet_is_always_text_and_csv_quoting_is_strict() {
        let input = "NULL\t\\N\t\\x00ff\t\"雪\t\"\"quote\"\"\r\nline\"\t\r\n";
        let parsed = parse_tsv(input, ClipboardDialect::Spreadsheet, limits()).unwrap();
        assert_eq!(
            parsed.rows()[0],
            vec![
                text("NULL"),
                text("\\N"),
                text("\\x00ff"),
                text("雪\t\"quote\"\r\nline"),
                text("")
            ]
        );
        assert_eq!(
            parse_tsv("", ClipboardDialect::Spreadsheet, limits())
                .unwrap()
                .rows(),
            &[vec![text("")]]
        );
        for invalid in ["a\"b", "\"a", "\"a\"b", "\"a\" \tq"] {
            assert!(matches!(
                parse_tsv(invalid, ClipboardDialect::Spreadsheet, limits()),
                Err(GridError::MalformedTsv { .. })
            ));
        }
        for invalid in ["\\x0", "\\xgg", "\\unknown", "\\n"] {
            assert_eq!(
                parse_tsv(invalid, ClipboardDialect::Rowboat, limits()),
                Err(GridError::InvalidEscape)
            );
        }
        for invalid in ["a\tb\nc", "a\nb\tc", "a\tb\n\n"] {
            assert_eq!(
                parse_tsv(invalid, ClipboardDialect::Spreadsheet, limits()),
                Err(GridError::Ragged)
            );
        }
    }

    #[test]
    fn grid_checked_rectangles_and_resource_limits() {
        let (mut result, _) = fixture();
        let range = GridRange::new(pos(1, 3), pos(0, 1)).unwrap();
        assert_eq!(range.start(), pos(0, 1));
        assert_eq!((range.rows(), range.columns()), (2, 3));
        range.validate(&result).unwrap();
        assert_eq!(GridRange::from_size(pos(0, 0), 0, 1), Err(GridError::Empty));
        assert_eq!(
            GridRange::from_size(pos(usize::MAX, 0), 2, 1),
            Err(GridError::Overflow)
        );
        assert_eq!(
            GridRange::new(pos(0, 0), pos(usize::MAX - 1, 1)),
            Err(GridError::Overflow)
        );
        assert_eq!(
            GridRange::new(pos(0, 0), pos(2, 0))
                .unwrap()
                .validate(&result),
            Err(GridError::Bounds)
        );
        result.rows[0].pop();
        assert_eq!(
            export_tsv(&result, range, ClipboardDialect::Rowboat, limits()),
            Err(GridError::Ragged)
        );
        let tiny = GridLimits {
            max_cells: 1,
            ..limits()
        };
        assert_eq!(
            parse_tsv("a\tb", ClipboardDialect::Spreadsheet, tiny),
            Err(GridError::Limit)
        );
        let tiny = GridLimits {
            max_bytes: 1,
            ..limits()
        };
        assert_eq!(
            parse_tsv("雪", ClipboardDialect::Spreadsheet, tiny),
            Err(GridError::Limit)
        );
        assert_eq!(limits().shape(usize::MAX, 2), Err(GridError::Overflow));
        assert_eq!(GridData::new(vec![vec![]], limits()), Err(GridError::Empty));
    }

    #[test]
    fn grid_paste_maps_reordered_columns_composite_keys_and_composes_edits() {
        let (result, structure) = fixture();
        let first = stage_fill(
            &result,
            &structure,
            GridRange::new(pos(0, 0), pos(1, 0)).unwrap(),
            text("edited label"),
            &[],
            limits(),
        )
        .unwrap();
        let data = parse_tsv(
            "\"雪\t\"\"q\"\"\nline\"\n\\N",
            ClipboardDialect::Rowboat,
            limits(),
        )
        .unwrap();
        let staged = stage_paste(&result, &structure, pos(0, 1), &data, &first, limits()).unwrap();
        assert_eq!(staged.len(), 2);
        let RowChange::Update { original, values } = &staged[0] else {
            panic!()
        };
        assert_eq!(
            original,
            &vec![
                ("tenant".into(), text("tenant")),
                ("id".into(), text("1")),
                ("note".into(), text("before")),
                ("label".into(), text("A"))
            ]
        );
        assert_eq!(
            values,
            &vec![
                ("note".into(), text("雪\t\"q\"\nline")),
                ("label".into(), text("edited label"))
            ]
        );
        assert_eq!(result.rows[0][0], text("A"));
        let RowChange::Update { values, .. } = &first[0] else {
            panic!()
        };
        assert_eq!(values, &vec![("label".into(), text("edited label"))]);
        let restored = stage_fill(
            &result,
            &structure,
            GridRange::new(pos(0, 0), pos(0, 0)).unwrap(),
            text("A"),
            &first,
            limits(),
        )
        .unwrap();
        assert_eq!(
            restored.len(),
            1,
            "reverting one row preserves other staged rows"
        );
    }

    #[test]
    fn grid_clear_and_failed_operations_are_atomic() {
        let (result, structure) = fixture();
        let range = GridRange::new(pos(0, 1), pos(1, 1)).unwrap();
        let staged = stage_clear(&result, &structure, range, &[], limits()).unwrap();
        assert_eq!(staged.len(), 1, "already NULL row is a no-op");
        assert!(matches!(
            stage_clear(
                &result,
                &structure,
                GridRange::new(pos(0, 0), pos(1, 1)).unwrap(),
                &staged,
                limits()
            ),
            Err(GridError::NonNullable(_))
        ));
        assert_eq!(staged.len(), 1);
        let overflow = parse_tsv("a\tb", ClipboardDialect::Spreadsheet, limits()).unwrap();
        assert!(matches!(
            stage_paste(&result, &structure, pos(1, 3), &overflow, &staged, limits()),
            Err(GridError::Bounds)
        ));
        let huge = GridLimits {
            max_bytes: usize::MAX,
            max_cells: usize::MAX,
            max_rows: usize::MAX,
            max_columns: usize::MAX,
        };
        let wide = GridRange::from_size(pos(0, 0), 1, usize::MAX - 1).unwrap();
        assert!(matches!(
            stage_fill(&result, &structure, wide, text("ab"), &[], huge),
            Err(GridError::Overflow)
        ));
    }

    #[test]
    fn grid_rejects_ambiguous_mapping_missing_keys_and_duplicate_rows() {
        let (result, structure) = fixture();
        let range = GridRange::new(pos(0, 1), pos(0, 1)).unwrap();
        let run = |r: &QueryResult, s: &Structure| stage_clear(r, s, range, &[], limits());
        let mut bad = result.clone();
        bad.columns[0].name = "note".into();
        assert!(matches!(run(&bad, &structure), Err(GridError::Mapping(_))));
        bad = result.clone();
        bad.columns[0].name = "Label".into();
        assert!(matches!(run(&bad, &structure), Err(GridError::Mapping(_))));
        bad = result.clone();
        bad.columns.pop();
        for row in &mut bad.rows {
            row.pop();
        }
        assert!(matches!(run(&bad, &structure), Err(GridError::Mapping(_))));
        bad = result.clone();
        bad.rows[1][2] = text("1");
        assert!(matches!(
            run(&bad, &structure),
            Err(GridError::DuplicateKey)
        ));
        bad = result.clone();
        bad.rows[0][3] = Cell::Null;
        assert!(matches!(
            run(&bad, &structure),
            Err(GridError::MissingPrimaryKey)
        ));
        let mut schema = structure.clone();
        for column in &mut schema.columns {
            column.primary_key = false;
        }
        assert!(matches!(
            run(&result, &schema),
            Err(GridError::MissingPrimaryKey)
        ));
        schema = structure.clone();
        schema.columns.push(schema.columns[0].clone());
        assert!(matches!(run(&result, &schema), Err(GridError::Mapping(_))));
    }

    #[test]
    fn grid_existing_stale_originals_deletions_and_duplicate_edits_are_rejected() {
        let (result, structure) = fixture();
        let range = GridRange::new(pos(0, 1), pos(0, 1)).unwrap();
        let staged = stage_clear(&result, &structure, range, &[], limits()).unwrap();
        let mut stale = result.clone();
        stale.rows[0][0] = text("concurrent update");
        assert!(matches!(
            stage_clear(&stale, &structure, range, &staged, limits()),
            Err(GridError::StaleOriginal)
        ));
        let RowChange::Update { original, .. } = &staged[0] else {
            panic!()
        };
        let deleted = vec![RowChange::Delete {
            original: original.clone(),
        }];
        assert!(matches!(
            stage_clear(&result, &structure, range, &deleted, limits()),
            Err(GridError::DeletedRow)
        ));
        let duplicate = vec![staged[0].clone(), staged[0].clone()];
        assert!(matches!(
            stage_clear(&result, &structure, range, &duplicate, limits()),
            Err(GridError::DuplicateKey)
        ));
        let mut incomplete = original.clone();
        incomplete.pop();
        assert!(matches!(
            stage_clear(
                &result,
                &structure,
                range,
                &[RowChange::Delete {
                    original: incomplete
                }],
                limits()
            ),
            Err(GridError::Mapping(_))
        ));
        let insert = RowChange::Insert {
            values: vec![("label".into(), text("new"))],
        };
        let mut existing = deleted;
        existing.push(insert);
        let other = GridRange::new(pos(1, 0), pos(1, 0)).unwrap();
        let output = stage_fill(
            &result,
            &structure,
            other,
            text("other"),
            &existing,
            limits(),
        )
        .unwrap();
        assert!(matches!(
            &output[..],
            [
                RowChange::Delete { .. },
                RowChange::Insert { .. },
                RowChange::Update { .. }
            ]
        ));
    }
}
