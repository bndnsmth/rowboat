//! One pending mutation per original row; repeated edits retain the conflict snapshot.
use crate::forms::RowTarget;
use rowboat_core::grid::{GridPosition, GridRange};
use rowboat_core::{Cell, RowChange};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GridOrigin {
    Table(RowTarget),
    Query {
        generation: u64,
        tab: usize,
        result: usize,
    },
}
#[derive(Clone, Debug)]
pub struct GridSelection {
    pub origin: GridOrigin,
    pub row: usize,
    pub column: usize,
    pub focus: GridPosition,
    pub visible_rows: Vec<usize>,
}
impl GridSelection {
    pub fn for_origin(&self, origin: &GridOrigin) -> Option<(usize, usize)> {
        (&self.origin == origin).then_some((self.focus.row, self.focus.column))
    }
    pub fn range(&self, origin: &GridOrigin, visible_rows: &[usize]) -> Option<GridRange> {
        if &self.origin != origin || self.visible_rows != visible_rows {
            return None;
        }
        GridRange::new(
            GridPosition {
                row: visible_rows.iter().position(|r| *r == self.row)?,
                column: self.column,
            },
            GridPosition {
                row: visible_rows.iter().position(|r| *r == self.focus.row)?,
                column: self.focus.column,
            },
        )
        .ok()
    }
}

pub fn readable(change: &RowChange) -> String {
    fn value(cell: &Cell) -> String {
        match cell {
            Cell::Null => "SQL NULL".into(),
            Cell::Text(s) => {
                let short: String = s.chars().take(240).collect();
                format!(
                    "{}{}",
                    serde_json::to_string(&short).unwrap_or_default(),
                    if s.chars().count() > 240 { "…" } else { "" }
                )
            }
            Cell::Binary(bytes) => format!(
                "binary · {} bytes · {}{}",
                bytes.len(),
                Cell::Binary(bytes.iter().take(32).copied().collect()).display(),
                if bytes.len() > 32 { "…" } else { "" }
            ),
        }
    }
    match change {
        RowChange::Insert { values } if values.is_empty() => {
            "INSERT · all columns use server defaults".into()
        }
        RowChange::Insert { values } => format!(
            "INSERT\n{}",
            values
                .iter()
                .map(|(name, v)| format!("{name}: {}", value(v)))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        RowChange::Update { original, values } => format!(
            "UPDATE\n{}",
            values
                .iter()
                .map(|(name, v)| format!(
                    "{name}: {} → {}",
                    original
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| value(v))
                        .unwrap_or_else(|| "not reported".into()),
                    value(v)
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        RowChange::Delete { original } => format!(
            "DELETE ROW\n{}",
            original
                .iter()
                .map(|(name, v)| format!("{name}: {}", value(v)))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

fn original(change: &RowChange) -> Option<&Vec<(String, Cell)>> {
    match change {
        RowChange::Update { original, .. } | RowChange::Delete { original } => Some(original),
        RowChange::Insert { .. } => None,
    }
}

pub fn stage(changes: &mut Vec<RowChange>, change: RowChange) {
    if let Some(row) = original(&change) {
        changes.retain(|old| original(old) != Some(row));
    }
    if !matches!(&change,RowChange::Update{values,..} if values.is_empty()) {
        changes.push(change);
    }
}

#[cfg(test)]
mod tests {
    use super::{Cell, GridOrigin, GridSelection, RowChange, RowTarget, stage};
    use rowboat_core::TableRef;
    #[test]
    fn reversed_range_tracks_display_order_and_rejects_changed_filter_or_origin() {
        use rowboat_core::grid::GridPosition;
        let origin = GridOrigin::Query {
            generation: 2,
            tab: 4,
            result: 1,
        };
        let selection = GridSelection {
            origin: origin.clone(),
            row: 8,
            column: 3,
            focus: GridPosition { row: 2, column: 1 },
            visible_rows: vec![2, 5, 8],
        };
        let range = selection.range(&origin, &[2, 5, 8]).unwrap();
        assert_eq!(range.start(), GridPosition { row: 0, column: 1 });
        assert_eq!(range.end(), GridPosition { row: 2, column: 3 });
        assert!(selection.range(&origin, &[2, 8]).is_none());
        assert!(
            selection
                .range(
                    &GridOrigin::Query {
                        generation: 2,
                        tab: 5,
                        result: 1
                    },
                    &[2, 5, 8]
                )
                .is_none()
        );
        assert!(
            selection
                .range(
                    &GridOrigin::Query {
                        generation: 3,
                        tab: 4,
                        result: 1
                    },
                    &[2, 5, 8]
                )
                .is_none()
        );
    }
    #[test]
    fn selection_never_crosses_query_table_or_reconnected_session() {
        let table = RowTarget {
            connection_id: "profile".into(),
            session_generation: 1,
            table: TableRef {
                schema: "public".into(),
                name: "customers".into(),
            },
        };
        let selected = GridSelection {
            origin: GridOrigin::Query {
                generation: 1,
                tab: 1,
                result: 0,
            },
            row: 2,
            column: 0,
            focus: rowboat_core::grid::GridPosition { row: 2, column: 0 },
            visible_rows: vec![0, 1, 2],
        };
        assert_eq!(selected.for_origin(&GridOrigin::Table(table.clone())), None);
        let selected = GridSelection {
            origin: GridOrigin::Table(table.clone()),
            row: 2,
            column: 0,
            focus: rowboat_core::grid::GridPosition { row: 2, column: 0 },
            visible_rows: vec![0, 1, 2],
        };
        assert_eq!(
            selected.for_origin(&GridOrigin::Table(table.clone())),
            Some((2, 0))
        );
        let mut reconnected = table;
        reconnected.session_generation = 2;
        assert_eq!(selected.for_origin(&GridOrigin::Table(reconnected)), None);
    }
    fn row(id: &str) -> Vec<(String, Cell)> {
        vec![
            ("id".into(), Cell::Text(id.into())),
            ("name".into(), Cell::Text("original".into())),
        ]
    }
    #[test]
    fn repeated_edits_replace_one_mutation_and_keep_other_rows() {
        let mut changes = vec![];
        stage(
            &mut changes,
            RowChange::Update {
                original: row("1"),
                values: vec![("name".into(), Cell::Text("first".into()))],
            },
        );
        stage(&mut changes, RowChange::Delete { original: row("2") });
        stage(
            &mut changes,
            RowChange::Update {
                original: row("1"),
                values: vec![("name".into(), Cell::Text("second".into()))],
            },
        );
        assert_eq!(changes.len(), 2);
        assert!(
            matches!(&changes[1],RowChange::Update{original,values} if *original==row("1")&&values[0].1==Cell::Text("second".into()))
        );
        stage(&mut changes, RowChange::Delete { original: row("1") });
        stage(&mut changes, RowChange::Delete { original: row("1") });
        assert_eq!(changes.len(), 2);
        assert!(matches!(&changes[1],RowChange::Delete{original} if *original==row("1")));
    }
    #[test]
    fn restoring_original_values_removes_only_that_pending_update() {
        let mut changes = vec![
            RowChange::Insert { values: vec![] },
            RowChange::Update {
                original: row("1"),
                values: vec![("name".into(), Cell::Null)],
            },
        ];
        stage(
            &mut changes,
            RowChange::Update {
                original: row("1"),
                values: vec![],
            },
        );
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], RowChange::Insert { .. }));
    }
}
