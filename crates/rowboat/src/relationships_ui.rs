//! Typed FK navigation bound to the source row, table and connection generation.
use crate::{
    app::{Event, Page, TablePage, Workbench},
    filters_ui::BoundFilter,
    forms::RowTarget,
    view::{action, column, label, row},
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::ActiveTheme;
use rowboat_core::{
    BrowseRequest, Cell, ForeignKeyRelationship, QueryResult, RelationshipDirection,
    RelationshipTarget, Structure,
};

pub struct Navigation {
    request: uuid::Uuid,
    target: RowTarget,
    row: Vec<(String, Cell)>,
    pending: bool,
    relationships: Vec<ForeignKeyRelationship>,
    error: Option<String>,
}
pub struct Origin {
    target: RowTarget,
    request: BrowseRequest,
    expression: Option<rowboat_core::FilterExpr>,
    row: Vec<(String, Cell)>,
}
pub struct Loaded {
    request: uuid::Uuid,
    target: RowTarget,
    result: Result<Vec<ForeignKeyRelationship>, String>,
}
fn row_still_current(
    target: &RowTarget,
    current: Option<RowTarget>,
    row: &[(String, Cell)],
    result: &QueryResult,
) -> bool {
    current.as_ref() == Some(target)
        && result.columns.len() == row.len()
        && result.rows.iter().any(|values| {
            values.len() == result.columns.len()
                && result.columns.iter().zip(values).all(|(column, value)| {
                    row.iter()
                        .any(|(name, original)| name == &column.name && value == original)
                })
        })
}
impl Workbench {
    pub fn show_relationships(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Table || !self.can_leave() {
            cx.notify();
            return;
        }
        let (Some(target), Some((row, _)), Some(database)) = (
            self.current_target(),
            self.selected_cell(),
            self.database.clone(),
        ) else {
            return;
        };
        let Some(values) = self.table_result.rows.get(row) else {
            return;
        };
        let row = self
            .table_result
            .columns
            .iter()
            .zip(values)
            .map(|(c, v)| (c.name.clone(), v.clone()))
            .collect();
        let request = uuid::Uuid::new_v4();
        self.relationships = Some(Navigation {
            request,
            target: target.clone(),
            row,
            pending: true,
            relationships: vec![],
            error: None,
        });
        self.busy = true;
        self.status = "Loading typed foreign-key relationships…".into();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = database
                .relationships(&target.table)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Relationships(Box::new(Loaded {
                request,
                target,
                result,
            })));
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    pub fn receive_relationships(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.relationships
            && dialog.request == loaded.request
            && dialog.target == loaded.target
        {
            dialog.pending = false;
            self.busy = false;
            match loaded.result {
                Ok(relationships) => {
                    dialog.relationships = relationships;
                    self.status = "Relationships loaded · choose an exact key mapping".into();
                }
                Err(error) => dialog.error = Some(error),
            }
        }
        cx.notify();
    }
    fn navigate_relationship(
        &mut self,
        index: usize,
        direction: RelationshipDirection,
        cx: &mut Context<Self>,
    ) {
        let result = (|| {
            anyhow::ensure!(
                !self.busy
                    && !self.close_requested
                    && self.changes.is_empty()
                    && !self.has_row_draft(),
                "Finish pending edits and operations before navigating relationships"
            );
            let dialog = self
                .relationships
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No relationship navigation"))?;
            anyhow::ensure!(
                row_still_current(
                    &dialog.target,
                    self.current_target(),
                    &dialog.row,
                    &self.table_result
                ),
                "The selected source row or connection session changed; choose it again"
            );
            let relationship = dialog
                .relationships
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("Relationship no longer exists"))?;
            let expected = match direction {
                RelationshipDirection::Referenced => &relationship.source,
                RelationshipDirection::Referencing => &relationship.referenced,
            };
            anyhow::ensure!(
                expected == &dialog.target.table,
                "Relationship direction does not match the source table"
            );
            rowboat_core::relationship_filter(relationship, direction, &dialog.row)
        })();
        match result {
            Ok(Some(RelationshipTarget { table, filter })) => {
                if let Some(dialog) = &self.relationships {
                    self.relationship_origin = Some(Origin {
                        target: dialog.target.clone(),
                        request: self.browse_request.clone(),
                        expression: self.filter_expression(),
                        row: dialog.row.clone(),
                    });
                }
                let target = RowTarget {
                    table: table.clone(),
                    ..self.current_target().unwrap()
                };
                self.filters.active = Some(BoundFilter {
                    target,
                    expression: filter,
                });
                self.filters.editor = None;
                self.relationships = None;
                self.table = Some(table);
                self.structure = Structure::default();
                self.table_result = QueryResult::default();
                self.selected = None;
                self.browse_request = BrowseRequest {
                    limit: self.state.preferences.page_size,
                    ..Default::default()
                };
                self.page = Page::Table;
                self.table_page = TablePage::Content;
                self.refresh_table(cx);
            }
            Ok(None) => {
                if let Some(dialog) = &mut self.relationships {
                    dialog.error=Some("At least one key component is SQL NULL; this row has no relationship target.".into());
                }
            }
            Err(e) => {
                if let Some(dialog) = &mut self.relationships {
                    dialog.error = Some(format!("{e:#}"));
                }
            }
        }
        cx.notify();
    }
    pub fn dismiss_relationships(&mut self) -> bool {
        if self.relationships.as_ref().is_some_and(|d| !d.pending) {
            self.relationships = None;
            true
        } else {
            false
        }
    }
    pub fn return_relationship_origin(&mut self, cx: &mut Context<Self>) {
        if !self.can_leave() {
            cx.notify();
            return;
        }
        let Some(origin) = self.relationship_origin.take() else {
            return;
        };
        if origin.target.session_generation != self.session_generation
            || Some(&origin.target.connection_id) != self.owner.as_ref()
        {
            self.fail("The relationship origin belongs to an earlier connection session.".into());
            cx.notify();
            return;
        }
        self.table = Some(origin.target.table.clone());
        self.browse_request = origin.request.clone();
        self.filters.active = origin.expression.clone().map(|expression| BoundFilter {
            target: origin.target.clone(),
            expression,
        });
        self.table_result = Default::default();
        self.structure = Default::default();
        self.selected = None;
        self.page = Page::Table;
        self.table_page = TablePage::Content;
        self.status = format!(
            "Returning to relationship origin ({} original columns)",
            origin.row.len()
        );
        self.relationship_return = Some(origin);
        self.refresh_table(cx);
    }
    pub fn restore_relationship_selection(&mut self, cx: &App) {
        let Some(origin) = self.relationship_return.take() else {
            return;
        };
        if self.current_target() != Some(origin.target) {
            return;
        }
        if let Some(index) = self.table_result.rows.iter().position(|values| {
            self.table_result.columns.len() == origin.row.len()
                && self
                    .table_result
                    .columns
                    .iter()
                    .zip(values)
                    .all(|(col, value)| {
                        origin
                            .row
                            .iter()
                            .any(|(name, old)| name == &col.name && old == value)
                    })
        }) {
            self.select_grid_cell(index, 0, false, cx);
        }
    }
    pub fn render_relationships(&self, cx: &mut Context<Self>) -> AnyElement {
        let dialog = self.relationships.as_ref().unwrap();
        let theme = cx.omarchy().clone();
        let mut body=column().w(px(820.)).max_h(px(650.)).p_5().gap_3()
            .child(row().child(div().text_size(px(21.)).child(format!("Related rows · {}.{}",dialog.target.table.schema,dialog.target.table.name))).child(div().flex_1())
                .child(action("close-relationships","Close",false,!dialog.pending,cx,|s,w,cx|{s.dismiss_relationships();w.focus(&s.focus,cx);cx.notify();})))
            .child(label("Uses every typed key component from the original selected row. Navigation executes one parameterized browse; NULL keys never navigate to unrelated NULL rows.",cx));
        if dialog.pending {
            body = body.child("Loading relationships…");
        } else if dialog.relationships.is_empty() && dialog.error.is_none() {
            body = body.child("No incoming or outgoing foreign keys were reported for this table.");
        }
        if let Some(error) = &dialog.error {
            body = body.child(div().text_color(theme.danger).child(error.clone()));
        }
        let mut items = column().gap_3();
        for (index, relationship) in dialog.relationships.iter().enumerate() {
            let columns = relationship
                .columns
                .iter()
                .map(|c| format!("{} → {}", c.source, c.referenced))
                .collect::<Vec<_>>()
                .join(" · ");
            let mut item = column()
                .gap_1()
                .child(format!("{} · {columns}", relationship.name));
            if relationship.source == dialog.target.table {
                item = item.child(action(
                    ("fk-parent", index),
                    format!(
                        "Open parent {}.{}",
                        relationship.referenced.schema, relationship.referenced.name
                    ),
                    false,
                    true,
                    cx,
                    move |s, _, cx| {
                        s.navigate_relationship(index, RelationshipDirection::Referenced, cx)
                    },
                ));
            }
            if relationship.referenced == dialog.target.table {
                item = item.child(action(
                    ("fk-children", index),
                    format!(
                        "Open referencing rows {}.{}",
                        relationship.source.schema, relationship.source.name
                    ),
                    false,
                    true,
                    cx,
                    move |s, _, cx| {
                        s.navigate_relationship(index, RelationshipDirection::Referencing, cx)
                    },
                ));
            }
            items = items.child(item);
        }
        body = body.child(
            div()
                .id("relationship-list")
                .overflow_y_scroll()
                .min_h_0()
                .child(items),
        );
        self.modal(body.into_any_element(), cx)
    }
}
#[cfg(test)]
mod tests {
    use super::row_still_current;
    use crate::forms::RowTarget;
    use rowboat_core::{Cell, Column, QueryResult, TableRef};
    #[test]
    fn relationship_snapshot_rejects_reconnect_other_table_and_changed_original_row() {
        let target = RowTarget {
            connection_id: "pg".into(),
            session_generation: 1,
            table: TableRef {
                schema: "public".into(),
                name: "child".into(),
            },
        };
        let result = QueryResult {
            columns: vec![Column {
                name: "tenant,id".into(),
                data_type: "text".into(),
                nullable: false,
                primary_key: true,
                default_value: None,
            }],
            rows: vec![vec![Cell::Text("original".into())]],
            ..Default::default()
        };
        let row = vec![("tenant,id".into(), Cell::Text("original".into()))];
        assert!(row_still_current(
            &target,
            Some(target.clone()),
            &row,
            &result
        ));
        let mut changed = target.clone();
        changed.session_generation += 1;
        assert!(!row_still_current(&target, Some(changed), &row, &result));
        let mut changed = target.clone();
        changed.table.name = "parent".into();
        assert!(!row_still_current(&target, Some(changed), &row, &result));
        let mut changed = result.clone();
        changed.rows[0][0] = Cell::Text("changed".into());
        assert!(!row_still_current(
            &target,
            Some(target.clone()),
            &row,
            &changed
        ));
    }
}
