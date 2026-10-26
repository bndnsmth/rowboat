//! Exact, read-only comparison of already retained results. No database access,
//! coercion, collation, JSON parsing or floating-point conversion occurs here.
use crate::{Cell, QueryResult};
use std::{
    cmp::Ordering,
    fmt,
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnPair {
    pub left: usize,
    pub right: usize,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mapping {
    pub keys: Vec<ColumnPair>,
    pub columns: Vec<ColumnPair>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompareError {
    Cancelled,
    Budget {
        required: usize,
        available: usize,
    },
    Mapping(String),
    RowWidth {
        side: Side,
        row: usize,
    },
    NullKey {
        side: Side,
        row: usize,
        column: usize,
    },
    DuplicateKey {
        side: Side,
        first: usize,
        second: usize,
    },
}
impl fmt::Display for CompareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "Comparison cancelled"),
            Self::Budget {
                required,
                available,
            } => write!(
                f,
                "Comparison needs {required} bytes; {available} bytes available in shared result budget"
            ),
            Self::Mapping(s) => f.write_str(s),
            Self::RowWidth { side, row } => write!(
                f,
                "{side:?} row {} does not match its column metadata",
                row + 1
            ),
            Self::NullKey { side, row, column } => write!(
                f,
                "{side:?} row {}, column {} has a NULL key; choose an unambiguous non-NULL key",
                row + 1,
                column + 1
            ),
            Self::DuplicateKey {
                side,
                first,
                second,
            } => write!(
                f,
                "{side:?} rows {} and {} have duplicate composite keys; matching is ambiguous",
                first + 1,
                second + 1
            ),
        }
    }
}
impl std::error::Error for CompareError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DifferenceKind {
    Added,
    Removed,
    Changed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowDifference {
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub kind: DifferenceKind,
}
#[derive(Debug)]
pub struct Comparison {
    pub rows: Vec<RowDifference>,
    pub unchanged: usize,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    /// Always true: even complete SQL results do not prove whole-table equality.
    pub retained_rows_only: bool,
    pub partial: bool,
}

/// Conservative reservation for all temporary sort buffers plus maximum output.
/// Input cells are borrowed, never copied. Vec capacities are explicitly bounded.
pub fn comparison_bytes(left_rows: usize, right_rows: usize) -> usize {
    left_rows
        .saturating_add(right_rows)
        .saturating_mul(2 * size_of::<usize>() + size_of::<RowDifference>())
        .saturating_add(size_of::<Comparison>() + 1024)
}

/// Heap capacity accounting for active results (including spare Vec capacity).
/// The stream uses logical row bytes; the GUI must reserve its growth overhead.
pub fn result_bytes(result: &QueryResult) -> usize {
    let mut bytes = size_of::<QueryResult>()
        .saturating_add(
            result
                .columns
                .capacity()
                .saturating_mul(size_of::<crate::Column>()),
        )
        .saturating_add(
            result
                .rows
                .capacity()
                .saturating_mul(size_of::<Vec<Cell>>()),
        );
    for c in &result.columns {
        bytes = bytes
            .saturating_add(c.name.capacity())
            .saturating_add(c.data_type.capacity())
            .saturating_add(c.default_value.as_ref().map_or(0, String::capacity));
    }
    for row in &result.rows {
        bytes = bytes.saturating_add(row.capacity().saturating_mul(size_of::<Cell>()));
        for c in row {
            bytes = bytes.saturating_add(match c {
                Cell::Null => 0,
                Cell::Text(s) => s.capacity(),
                Cell::Binary(b) => b.capacity(),
            });
        }
    }
    bytes
}

pub fn validate_mapping(
    left: &QueryResult,
    right: &QueryResult,
    mapping: &Mapping,
) -> Result<(), CompareError> {
    validate_mapping_inner(left, right, mapping, &AtomicBool::new(false))
}
fn validate_mapping_inner(
    left: &QueryResult,
    right: &QueryResult,
    mapping: &Mapping,
    cancel: &AtomicBool,
) -> Result<(), CompareError> {
    if mapping.keys.is_empty() || mapping.columns.is_empty() {
        return Err(CompareError::Mapping(
            "Choose at least one key and one compared column".into(),
        ));
    }
    for pairs in [&mapping.keys, &mapping.columns] {
        for (i, pair) in pairs.iter().enumerate() {
            check(cancel)?;
            let (Some(l), Some(r)) = (left.columns.get(pair.left), right.columns.get(pair.right))
            else {
                return Err(CompareError::Mapping(
                    "Mapped column is outside the retained schema".into(),
                ));
            };
            if pairs[..i]
                .iter()
                .any(|p| p.left == pair.left || p.right == pair.right)
            {
                return Err(CompareError::Mapping(
                    "Each column can appear only once within key or value mappings".into(),
                ));
            }
            if !l.data_type.trim().eq_ignore_ascii_case(r.data_type.trim()) {
                return Err(CompareError::Mapping(format!(
                    "Incompatible types: left column {} ({}) ↔ right column {} ({}). Exact comparison requires matching type metadata",
                    pair.left + 1,
                    l.data_type.chars().take(64).collect::<String>(),
                    pair.right + 1,
                    r.data_type.chars().take(64).collect::<String>()
                )));
            }
        }
    }
    Ok(())
}

fn check(cancel: &AtomicBool) -> Result<(), CompareError> {
    if cancel.load(AtomicOrdering::Relaxed) {
        Err(CompareError::Cancelled)
    } else {
        Ok(())
    }
}
fn cell_order(left: &Cell, right: &Cell) -> Ordering {
    match (left, right) {
        (Cell::Null, Cell::Null) => Ordering::Equal,
        (Cell::Null, _) => Ordering::Less,
        (_, Cell::Null) => Ordering::Greater,
        (Cell::Text(l), Cell::Text(r)) => l.as_bytes().cmp(r.as_bytes()),
        (Cell::Binary(l), Cell::Binary(r)) => l.cmp(r),
        (Cell::Text(_), Cell::Binary(_)) => Ordering::Less,
        (Cell::Binary(_), Cell::Text(_)) => Ordering::Greater,
    }
}
fn key_order(left: &[Cell], right: &[Cell], keys: &[ColumnPair]) -> Ordering {
    keys.iter()
        .map(|p| cell_order(&left[p.left], &right[p.right]))
        .find(|o| *o != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}
fn sorted_rows(
    result: &QueryResult,
    keys: &[ColumnPair],
    side: Side,
    cancel: &AtomicBool,
) -> Result<Vec<usize>, CompareError> {
    let index = |p: &ColumnPair| if side == Side::Left { p.left } else { p.right };
    for (i, row) in result.rows.iter().enumerate() {
        check(cancel)?;
        if row.len() != result.columns.len() {
            return Err(CompareError::RowWidth { side, row: i });
        }
        for p in keys {
            if row[index(p)] == Cell::Null {
                return Err(CompareError::NullKey {
                    side,
                    row: i,
                    column: index(p),
                });
            }
        }
    }
    let order = |a: usize, b: usize| {
        keys.iter()
            .map(|p| cell_order(&result.rows[a][index(p)], &result.rows[b][index(p)]))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    };
    let n = result.rows.len();
    let mut rows: Vec<_> = (0..n).collect();
    let mut scratch = vec![0; n];
    // Iterative merge sort permits cancellation inside sorting, not only after it.
    let mut width = 1usize;
    while width < n {
        for start in (0..n).step_by(width.saturating_mul(2)) {
            let mid = (start + width).min(n);
            let end = start.saturating_add(width.saturating_mul(2)).min(n);
            let (mut l, mut r) = (start, mid);
            for slot in &mut scratch[start..end] {
                check(cancel)?;
                if l < mid && (r == end || order(rows[l], rows[r]) != Ordering::Greater) {
                    *slot = rows[l];
                    l += 1;
                } else {
                    *slot = rows[r];
                    r += 1;
                }
            }
        }
        std::mem::swap(&mut rows, &mut scratch);
        width = width.saturating_mul(2);
    }
    for pair in rows.windows(2) {
        check(cancel)?;
        if order(pair[0], pair[1]) == Ordering::Equal {
            return Err(CompareError::DuplicateKey {
                side,
                first: pair[0],
                second: pair[1],
            });
        }
    }
    Ok(rows)
}

/// `left_partial/right_partial` must include unfinished/cancelled results and
/// table-page scope, in addition to QueryResult::truncated.
pub fn compare(
    left: &QueryResult,
    right: &QueryResult,
    mapping: &Mapping,
    partial: [bool; 2],
    available_bytes: usize,
    cancel: &AtomicBool,
) -> Result<Comparison, CompareError> {
    check(cancel)?;
    validate_mapping_inner(left, right, mapping, cancel)?;
    let required = comparison_bytes(left.rows.len(), right.rows.len());
    if required > available_bytes {
        return Err(CompareError::Budget {
            required,
            available: available_bytes,
        });
    }
    let lrows = sorted_rows(left, &mapping.keys, Side::Left, cancel)?;
    let rrows = sorted_rows(right, &mapping.keys, Side::Right, cancel)?;
    let mut out = Comparison {
        rows: Vec::with_capacity(lrows.len() + rrows.len()),
        unchanged: 0,
        added: 0,
        removed: 0,
        changed: 0,
        retained_rows_only: true,
        partial: partial[0] || partial[1] || left.truncated || right.truncated,
    };
    let (mut l, mut r) = (0, 0);
    while l < lrows.len() || r < rrows.len() {
        check(cancel)?;
        let order = match (lrows.get(l), rrows.get(r)) {
            (Some(&li), Some(&ri)) => key_order(&left.rows[li], &right.rows[ri], &mapping.keys),
            (Some(_), None) => Ordering::Less,
            _ => Ordering::Greater,
        };
        match order {
            Ordering::Less => {
                out.removed += 1;
                out.rows.push(RowDifference {
                    left: Some(lrows[l]),
                    right: None,
                    kind: DifferenceKind::Removed,
                });
                l += 1;
            }
            Ordering::Greater => {
                out.added += 1;
                out.rows.push(RowDifference {
                    left: None,
                    right: Some(rrows[r]),
                    kind: DifferenceKind::Added,
                });
                r += 1;
            }
            Ordering::Equal => {
                let (li, ri) = (lrows[l], rrows[r]);
                let mut changed = false;
                for p in &mapping.columns {
                    check(cancel)?;
                    changed |= left.rows[li][p.left] != right.rows[ri][p.right];
                }
                if changed {
                    out.changed += 1;
                    out.rows.push(RowDifference {
                        left: Some(li),
                        right: Some(ri),
                        kind: DifferenceKind::Changed,
                    });
                } else {
                    out.unchanged += 1;
                }
                l += 1;
                r += 1;
            }
        }
    }
    check(cancel)?;
    Ok(out)
}
