//! Nested parameterized filter drafts; applied expressions are immutable snapshots.
use crate::{
    app::{Page, TablePage, Workbench},
    forms::{self, RowTarget},
    grid_ui::ValueKind,
    view::{action, column, label, row},
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::{Cell, Filter, FilterExpr, FilterOp};

#[derive(Default)]
pub struct FilterState {
    pub active: Option<BoundFilter>,
    pub editor: Option<FilterEditor>,
    next: u64,
}
pub struct BoundFilter {
    pub target: RowTarget,
    pub expression: FilterExpr,
}
pub struct FilterEditor {
    target: RowTarget,
    root: Node,
    name: Entity<InputState>,
    saved: Option<String>,
}
struct Node {
    id: u64,
    kind: NodeKind,
}
enum NodeKind {
    Group {
        all: bool,
        children: Vec<Node>,
    },
    Condition {
        column: Entity<InputState>,
        op: usize,
        values: Vec<Value>,
    },
}
struct Value {
    kind: ValueKind,
    input: Entity<InputState>,
}
const OPS: [&str; 14] = [
    "=",
    "≠",
    "<",
    ">",
    "≤",
    "≥",
    "contains",
    "starts with",
    "IS NULL",
    "IS NOT NULL",
    "IN",
    "NOT IN",
    "BETWEEN",
    "NOT BETWEEN",
];
fn predicate(column: String, op: usize, values: Vec<Cell>) -> anyhow::Result<FilterExpr> {
    let leaf = match op {
        0 => FilterOp::Eq,
        1 => FilterOp::NotEq,
        2 => FilterOp::Less,
        3 => FilterOp::Greater,
        4 => FilterOp::LessEq,
        5 => FilterOp::GreaterEq,
        6 => FilterOp::Contains,
        7 => FilterOp::StartsWith,
        8 => FilterOp::IsNull,
        9 => FilterOp::IsNotNull,
        10 | 11 => {
            return Ok(FilterExpr::In {
                column,
                values,
                negated: op == 11,
            });
        }
        12 | 13 => {
            anyhow::ensure!(values.len() == 2, "BETWEEN needs two values");
            return Ok(FilterExpr::Between {
                column,
                low: values[0].clone(),
                high: values[1].clone(),
                negated: op == 13,
            });
        }
        _ => anyhow::bail!("Unknown filter operator"),
    };
    Ok(FilterExpr::Leaf(Filter {
        column,
        op: leaf,
        value: if matches!(op, 8 | 9) {
            Cell::Null
        } else {
            values
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("Filter needs a value"))?
        },
    }))
}
impl Value {
    fn new(cell: Cell, window: &mut Window, cx: &mut App) -> Self {
        let kind = match &cell {
            Cell::Text(_) => ValueKind::Text,
            Cell::Binary(_) => ValueKind::Binary,
            Cell::Null => ValueKind::Null,
        };
        Self {
            kind,
            input: forms::input(
                if matches!(cell, Cell::Null) {
                    String::new()
                } else {
                    cell.display()
                },
                "Bound value",
                window,
                cx,
            ),
        }
    }
}
impl Node {
    fn from_expr(expr: FilterExpr, next: &mut u64, window: &mut Window, cx: &mut App) -> Self {
        let id = *next;
        *next += 1;
        let condition =
            |column, op, cells: Vec<Cell>, window: &mut Window, cx: &mut App| NodeKind::Condition {
                column: forms::input(column, "Column name", window, cx),
                op,
                values: cells
                    .into_iter()
                    .map(|cell| Value::new(cell, window, cx))
                    .collect(),
            };
        let kind = match expr {
            FilterExpr::And(children) => NodeKind::Group {
                all: true,
                children: children
                    .into_iter()
                    .map(|c| Self::from_expr(c, next, window, cx))
                    .collect(),
            },
            FilterExpr::Or(children) => NodeKind::Group {
                all: false,
                children: children
                    .into_iter()
                    .map(|c| Self::from_expr(c, next, window, cx))
                    .collect(),
            },
            FilterExpr::Leaf(filter) => {
                let op = match filter.op {
                    FilterOp::Eq => 0,
                    FilterOp::NotEq => 1,
                    FilterOp::Less => 2,
                    FilterOp::Greater => 3,
                    FilterOp::LessEq => 4,
                    FilterOp::GreaterEq => 5,
                    FilterOp::Contains => 6,
                    FilterOp::StartsWith => 7,
                    FilterOp::IsNull => 8,
                    FilterOp::IsNotNull => 9,
                };
                condition(filter.column, op, vec![filter.value], window, cx)
            }
            FilterExpr::In {
                column,
                values,
                negated,
            } => condition(column, if negated { 11 } else { 10 }, values, window, cx),
            FilterExpr::Between {
                column,
                low,
                high,
                negated,
            } => condition(
                column,
                if negated { 13 } else { 12 },
                vec![low, high],
                window,
                cx,
            ),
        };
        Self { id, kind }
    }
    fn find_mut(&mut self, id: u64) -> Option<&mut Node> {
        if self.id == id {
            return Some(self);
        }
        if let NodeKind::Group { children, .. } = &mut self.kind {
            return children.iter_mut().find_map(|child| child.find_mut(id));
        }
        None
    }
    fn remove(&mut self, id: u64) {
        if let NodeKind::Group { children, .. } = &mut self.kind {
            children.retain(|c| c.id != id);
            for child in children {
                child.remove(id);
            }
        }
    }
    fn count(&self) -> usize {
        1 + match &self.kind {
            NodeKind::Group { children, .. } => children.iter().map(Self::count).sum(),
            _ => 0,
        }
    }
    fn expression(&self, columns: &[rowboat_core::Column], cx: &App) -> anyhow::Result<FilterExpr> {
        match &self.kind {
            NodeKind::Group { all, children } => {
                let children = children
                    .iter()
                    .map(|c| c.expression(columns, cx))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok(if *all {
                    FilterExpr::And(children)
                } else {
                    FilterExpr::Or(children)
                })
            }
            NodeKind::Condition { column, op, values } => {
                let name = column.read(cx).value().to_string();
                anyhow::ensure!(
                    columns.iter().any(|c| c.name == name),
                    "Unknown filter column: {name}"
                );
                let values = if matches!(op, 8 | 9) {
                    vec![]
                } else {
                    values
                        .iter()
                        .map(|v| {
                            v.kind
                                .parse(&v.input.read(cx).value())
                                .map_err(anyhow::Error::msg)
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?
                };
                predicate(name, *op, values)
            }
        }
    }
}
impl Workbench {
    pub fn filter_expression(&self) -> Option<FilterExpr> {
        let target = self.current_target()?;
        self.filters
            .active
            .as_ref()
            .filter(|a| a.target == target)
            .map(|a| a.expression.clone())
    }
    pub fn show_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_extension_dialog() {
            return;
        }
        if self.busy
            || self.close_requested
            || self.has_row_draft()
            || self.preferences_form.is_some()
        {
            return;
        }
        let Some(target) = self.current_target() else {
            return;
        };
        let expression = self
            .filter_expression()
            .unwrap_or_else(|| FilterExpr::And(vec![]));
        let expression = match expression {
            FilterExpr::And(_) | FilterExpr::Or(_) => expression,
            other => FilterExpr::And(vec![other]),
        };
        let root = Node::from_expr(expression, &mut self.filters.next, window, cx);
        self.error = None;
        self.filters.editor = Some(FilterEditor {
            target,
            root,
            name: forms::input("", "Saved filter name", window, cx),
            saved: None,
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    fn filter_draft(&self, cx: &App) -> anyhow::Result<(RowTarget, FilterExpr)> {
        let editor = self
            .filters
            .editor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No filter draft"))?;
        anyhow::ensure!(
            self.current_target() == Some(editor.target.clone()),
            "Filter draft belongs to another table or session"
        );
        let expression = editor.root.expression(&self.structure.columns, cx)?;
        expression.validate()?;
        Ok((editor.target.clone(), expression))
    }
    fn apply_filter_draft(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.close_requested || self.has_row_draft() || !self.changes.is_empty() {
            self.fail("Apply or revert pending row edits before changing filters.".into());
            cx.notify();
            return;
        }
        match self.filter_draft(cx) {
            Ok((target, expression)) => {
                self.filters.active = Some(BoundFilter { target, expression });
                self.filters.editor = None;
                self.browse_request.filters.clear();
                self.browse_request.offset = 0;
                self.page = Page::Table;
                self.table_page = TablePage::Content;
                self.refresh_table(cx);
            }
            Err(e) => {
                self.fail(format!("Filter: {e:#}"));
                cx.notify();
            }
        }
    }
    fn save_filter_draft(&mut self, cx: &mut Context<Self>) {
        let result = (|| {
            let (target, expression) = self.filter_draft(cx)?;
            let editor = self.filters.editor.as_ref().unwrap();
            let id = editor
                .saved
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let saved = rowboat_services::SavedFilter {
                id,
                name: editor.name.read(cx).value().trim().into(),
                connection_id: target.connection_id,
                table: target.table,
                expression,
            };
            let shared = self.shared.borrow();
            anyhow::ensure!(shared.writable, "App data is not writable");
            let mut state = shared.state.clone();
            state.saved_filters.retain(|f| f.id != saved.id);
            state.saved_filters.push(saved.clone());
            rowboat_services::validate_app_state(&state)?;
            Ok::<_, anyhow::Error>(saved)
        })();
        match result {
            Ok(saved) => {
                self.filters.editor.as_mut().unwrap().saved = Some(saved.id.clone());
                self.edit_state(|state| {
                    state.saved_filters.retain(|f| f.id != saved.id);
                    state.saved_filters.push(saved);
                });
                self.status =
                    "Filter saved for this connection and table; apply explicitly to browse."
                        .into();
            }
            Err(e) => self.fail(format!("Save filter: {e:#}")),
        }
        cx.notify();
    }
    fn add_filter_node(
        &mut self,
        parent: u64,
        group: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = &mut self.filters.editor else {
            return;
        };
        if editor.root.count() >= 256 {
            return;
        }
        let expr = if group {
            FilterExpr::And(vec![])
        } else {
            FilterExpr::Leaf(Filter {
                column: self
                    .structure
                    .columns
                    .first()
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                op: FilterOp::Eq,
                value: Cell::Text(String::new()),
            })
        };
        let node = Node::from_expr(expr, &mut self.filters.next, window, cx);
        if let Some(Node {
            kind: NodeKind::Group { children, .. },
            ..
        }) = editor.root.find_mut(parent)
        {
            children.push(node);
        }
        cx.notify();
    }
    fn change_filter_op(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Node {
            kind: NodeKind::Condition { op, values, .. },
            ..
        }) = self
            .filters
            .editor
            .as_mut()
            .and_then(|e| e.root.find_mut(id))
        {
            *op = (*op + 1) % OPS.len();
            let required = if *op >= 12 { 2 } else { 1 };
            while values.len() < required {
                values.push(Value::new(Cell::Text(String::new()), window, cx));
            }
            if *op < 10 || *op >= 12 {
                values.truncate(required);
            }
            if !matches!(*op, 10 | 11) {
                for value in values {
                    if value.kind == ValueKind::Null {
                        value.kind = ValueKind::Text;
                    }
                }
            }
        }
        cx.notify();
    }
    fn render_filter_node(
        &self,
        node: &Node,
        root: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = node.id;
        let theme = cx.omarchy().clone();
        let mut body = column()
            .p_2()
            .gap_2()
            .border_l_2()
            .border_color(theme.accent.opacity(0.5));
        match &node.kind {
            NodeKind::Group { all, children } => {
                body = body.child(
                    row()
                        .child(action(
                            ("filter-group", id),
                            if *all {
                                "AND · match all"
                            } else {
                                "OR · match any"
                            },
                            false,
                            true,
                            cx,
                            move |s, _, cx| {
                                if let Some(Node {
                                    kind: NodeKind::Group { all, .. },
                                    ..
                                }) = s.filters.editor.as_mut().and_then(|e| e.root.find_mut(id))
                                {
                                    *all = !*all;
                                }
                                cx.notify();
                            },
                        ))
                        .child(action(
                            ("filter-add", id),
                            "+ Condition",
                            false,
                            true,
                            cx,
                            move |s, w, cx| s.add_filter_node(id, false, w, cx),
                        ))
                        .child(action(
                            ("filter-nest", id),
                            "+ Nested group",
                            false,
                            true,
                            cx,
                            move |s, w, cx| s.add_filter_node(id, true, w, cx),
                        )),
                );
                if children.is_empty() {
                    body = body.child(label(
                        if *all {
                            "Empty AND matches all rows"
                        } else {
                            "Empty OR matches no rows"
                        },
                        cx,
                    ));
                }
                for child in children {
                    body = body.child(self.render_filter_node(child, false, window, cx));
                }
            }
            NodeKind::Condition {
                column: field,
                op,
                values,
            } => {
                let mut condition = row()
                    .flex_wrap()
                    .child(div().w(px(190.)).child(ui::input(
                        ("filter-column", id),
                        field,
                        window,
                        cx,
                    )))
                    .child(action(
                        ("filter-operator", id),
                        format!("{} ↻", OPS[*op]),
                        false,
                        true,
                        cx,
                        move |s, w, cx| s.change_filter_op(id, w, cx),
                    ));
                if !matches!(op, 8 | 9) {
                    for (index, value) in values.iter().enumerate() {
                        condition = condition.child(
                            column()
                                .w(px(190.))
                                .child(action(
                                    SharedString::from(format!("filter-kind-{id}-{index}")),
                                    value.kind.label(),
                                    false,
                                    true,
                                    cx,
                                    move |s, _, cx| {
                                        if let Some(Node {
                                            kind: NodeKind::Condition { op, values, .. },
                                            ..
                                        }) = s
                                            .filters
                                            .editor
                                            .as_mut()
                                            .and_then(|e| e.root.find_mut(id))
                                            && let Some(value) = values.get_mut(index)
                                        {
                                            value.kind = value.kind.next();
                                            if !matches!(*op, 10 | 11)
                                                && value.kind == ValueKind::Null
                                            {
                                                value.kind = ValueKind::Text;
                                            }
                                        }
                                        cx.notify();
                                    },
                                ))
                                .when(value.kind != ValueKind::Null, |d| {
                                    d.child(ui::input(
                                        SharedString::from(format!("filter-value-{id}-{index}")),
                                        &value.input,
                                        window,
                                        cx,
                                    ))
                                }),
                        );
                    }
                }
                if matches!(op, 10 | 11) {
                    condition = condition
                        .child(action(
                            ("filter-list-add", id),
                            "+ Value",
                            false,
                            values.len() < 256,
                            cx,
                            move |s, w, cx| {
                                if let Some(Node {
                                    kind: NodeKind::Condition { values, .. },
                                    ..
                                }) = s.filters.editor.as_mut().and_then(|e| e.root.find_mut(id))
                                {
                                    values.push(Value::new(Cell::Text(String::new()), w, cx));
                                }
                                cx.notify();
                            },
                        ))
                        .child(action(
                            ("filter-list-remove", id),
                            "− Value",
                            false,
                            !values.is_empty(),
                            cx,
                            move |s, _, cx| {
                                if let Some(Node {
                                    kind: NodeKind::Condition { values, .. },
                                    ..
                                }) = s.filters.editor.as_mut().and_then(|e| e.root.find_mut(id))
                                {
                                    values.pop();
                                }
                                cx.notify();
                            },
                        ));
                }
                body = body.child(condition);
            }
        }
        if !root {
            body = body.child(action(
                ("filter-remove", id),
                "Remove",
                false,
                true,
                cx,
                move |s, _, cx| {
                    if let Some(e) = &mut s.filters.editor {
                        e.root.remove(id);
                    }
                    cx.notify();
                },
            ));
        }
        body.into_any_element()
    }
    pub fn render_filters(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.filters.editor.as_ref().unwrap();
        let target = editor.target.clone();
        let mut body=column().p_5().gap_3().child(row().child(div().text_size(px(21.)).child(format!("Filters · {}.{}",target.table.schema,target.table.name))).child(div().flex_1()).child(action("close-filters","Close",false,true,cx,|s,w,cx|{s.filters.editor=None;w.focus(&s.focus,cx);cx.notify();})))
            .child(label("Nested groups use bound parameters. Operator ↻ cycles comparisons, NULL, IN and BETWEEN. Draft edits do not change the active browse filter.",cx))
            .child(self.render_filter_node(&editor.root,true,window,cx))
            .child(row().child(div().w(px(280.)).child(ui::input("saved-filter-name",&editor.name,window,cx))).child(action("save-filter","Save filter",false,self.loaded,cx,|s,_,cx|s.save_filter_draft(cx)))
                .child(action("apply-filter-tree","Apply filter",true,!self.busy&&self.changes.is_empty()&&!self.has_row_draft(),cx,|s,_,cx|s.apply_filter_draft(cx))));
        for saved in self
            .state
            .saved_filters
            .iter()
            .filter(|f| f.connection_id == target.connection_id && f.table == target.table)
        {
            let id = saved.id.clone();
            let remove = id.clone();
            body = body.child(
                row()
                    .child(action(
                        SharedString::from(format!("load-filter-{id}")),
                        format!("Load: {}", saved.name),
                        false,
                        true,
                        cx,
                        move |s, w, cx| {
                            if let Some(saved) =
                                s.state.saved_filters.iter().find(|f| f.id == id).cloned()
                                && let Some(editor) = &mut s.filters.editor
                                && saved.connection_id == editor.target.connection_id
                                && saved.table == editor.target.table
                            {
                                let expr = match saved.expression {
                                    FilterExpr::And(_) | FilterExpr::Or(_) => saved.expression,
                                    other => FilterExpr::And(vec![other]),
                                };
                                editor.root = Node::from_expr(expr, &mut s.filters.next, w, cx);
                                editor.saved = Some(saved.id);
                                editor
                                    .name
                                    .update(cx, |n, cx| n.set_value(saved.name, w, cx));
                            }
                            cx.notify();
                        },
                    ))
                    .child(action(
                        SharedString::from(format!("delete-filter-{remove}")),
                        "Delete saved",
                        false,
                        self.loaded,
                        cx,
                        move |s, _, cx| {
                            s.edit_state(|state| state.saved_filters.retain(|f| f.id != remove));
                            cx.notify();
                        },
                    )),
            );
        }
        if let Some(error) = &self.error {
            body = body.child(label(error.clone(), cx));
        }
        self.modal(
            div()
                .id("filter-tree-scroll")
                .w(px(970.))
                .max_h(window.viewport_size().height - px(65.))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
    pub fn render_filter_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let clean = !self.busy && !self.has_row_draft() && self.changes.is_empty();
        if self.filter_expression().is_none() && self.relationship_origin.is_none() {
            return div().into_any_element();
        }
        row()
            .px_3()
            .py_2()
            .flex_wrap()
            .child(label(
                if self.filter_expression().is_some() {
                    "Filtered rows"
                } else {
                    "All rows"
                },
                cx,
            ))
            .when(self.relationship_origin.is_some(), |d| {
                d.child(action(
                    "back-related-origin",
                    "← Original row's table",
                    false,
                    clean,
                    cx,
                    |s, _, cx| s.return_relationship_origin(cx),
                ))
            })
            .child(action(
                "reset-filter-tree",
                "Clear filter",
                false,
                clean,
                cx,
                |s, _, cx| {
                    s.filters.active = None;
                    s.browse_request.filters.clear();
                    s.browse_request.offset = 0;
                    s.refresh_table(cx);
                },
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::predicate;
    use rowboat_core::{Cell, FilterExpr};
    #[test]
    fn null_operators_do_not_reuse_hidden_text_and_between_rejects_null_endpoints() {
        let null = predicate("value".into(), 8, vec![Cell::Text("stale draft".into())]).unwrap();
        assert!(matches!(null,FilterExpr::Leaf(f) if f.value==Cell::Null));
        let between = predicate("id".into(), 12, vec![Cell::Null, Cell::Text("2".into())]).unwrap();
        assert!(between.validate().is_err());
    }
    #[test]
    fn nested_applied_expression_is_an_independent_snapshot_with_typed_values() {
        let mut draft = FilterExpr::And(vec![FilterExpr::Or(vec![
            predicate("id".into(), 0, vec![Cell::Text("1".into())]).unwrap(),
            predicate(
                "blob".into(),
                10,
                vec![Cell::Binary(vec![0, 255]), Cell::Null],
            )
            .unwrap(),
        ])]);
        let active = draft.clone();
        let original = serde_json::to_value(&active).unwrap();
        if let FilterExpr::And(children) = &mut draft {
            children.clear();
        }
        assert_eq!(serde_json::to_value(&active).unwrap(), original);
        assert_ne!(serde_json::to_value(&draft).unwrap(), original);
        active.validate().unwrap();
    }
}
