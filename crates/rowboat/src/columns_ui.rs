//! Advanced MySQL column forms retain the full inspected definition and origin.
use crate::{
    app::{Event, Workbench},
    forms,
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy as ui;
use rowboat_admin::ddl::{
    self, AdvancedMySqlColumnDefinition, GeneratedStorage, MySqlColumnEdit, MySqlColumnSnapshot,
    MySqlGeneration, MySqlSpecialType,
};
use rowboat_core::{Database, Engine, TableRef};
use std::collections::BTreeMap;

pub struct ColumnDialog {
    id: uuid::Uuid,
    route: Route,
    table: TableRef,
    name: String,
    snapshot: Option<MySqlColumnSnapshot>,
    fields: BTreeMap<&'static str, Entity<InputState>>,
    type_mode: usize,
    generation_mode: usize,
    nullable: bool,
    review: Option<(String, MySqlColumnEdit)>,
    error: Option<String>,
}

pub enum ColumnEvent {
    Inspected {
        id: uuid::Uuid,
        result: Result<MySqlColumnSnapshot, String>,
    },
    Applied {
        id: uuid::Uuid,
        result: Result<MySqlColumnSnapshot, String>,
        transaction: Result<bool, String>,
    },
}
const TYPES: &[&str] = &[
    "Keep inspected type",
    "Ordinary type",
    "ENUM",
    "SET",
    "DATETIME",
    "TINYINT",
];
const GENERATION: &[&str] = &["Keep generation", "Remove generation", "VIRTUAL", "STORED"];
impl ColumnDialog {
    fn draft(&self, cx: &App) -> anyhow::Result<AdvancedMySqlColumnDefinition> {
        let mut definition = self
            .snapshot
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Inspect the column first"))?
            .definition()
            .clone();
        let value = |key| self.fields[key].read(cx).value().to_string();
        match self.type_mode {
            0 => {}
            1 => {
                definition.base.column.data_type =
                    ddl::DataType::parse(Engine::MySql, &value("type / values"))?;
                definition.specialized_type = None;
            }
            mode => {
                let special = match mode {
                    2 => MySqlSpecialType::Enum(serde_json::from_str(&value("type / values"))?),
                    3 => MySqlSpecialType::Set(serde_json::from_str(&value("type / values"))?),
                    4 => MySqlSpecialType::DateTime {
                        precision: value("type / values").parse()?,
                    },
                    _ => MySqlSpecialType::TinyInt {
                        display_width: if value("type / values").is_empty() {
                            None
                        } else {
                            Some(value("type / values").parse()?)
                        },
                    },
                };
                definition.base.column.data_type = special.base_type();
                definition.specialized_type = Some(special);
            }
        }
        match self.generation_mode {
            0 => {}
            1 => definition.generation = None,
            mode => {
                definition.generation = Some(MySqlGeneration {
                    expression: value("generation expression"),
                    storage: if mode == 2 {
                        GeneratedStorage::Virtual
                    } else {
                        GeneratedStorage::Stored
                    },
                })
            }
        }
        definition.base.column.nullable = self.nullable;
        definition.base.comment = value("comment");
        Ok(definition)
    }
}
async fn apply_column(
    db: &Database,
    edit: &MySqlColumnEdit,
) -> anyhow::Result<MySqlColumnSnapshot> {
    anyhow::ensure!(
        !db.in_transaction().await?,
        "End the transaction before MySQL column DDL"
    );
    anyhow::ensure!(
        ddl::mysql_column_is_current(db, edit).await?,
        "Full column/table/origin metadata changed; inspect and review again"
    );
    db.query(&edit.plan().sql(), 0).await?;
    ddl::inspect_mysql_column(
        db,
        edit.original().table(),
        &edit.original().definition().base.column.name,
    )
    .await
}
impl Workbench {
    pub fn show_advanced_column(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_leave() || self.close_requested {
            return;
        }
        let (Some(db), Some(table)) = (self.database.clone(), self.table.clone()) else {
            return;
        };
        if db.engine() != Engine::MySql {
            return;
        }
        let fields = ["type / values", "generation expression", "comment"]
            .into_iter()
            .map(|key| (key, forms::input("", key, window, cx)))
            .collect();
        let id = uuid::Uuid::new_v4();
        self.column_form = Some(ColumnDialog {
            id,
            route: self.tx.route,
            table: table.clone(),
            name: name.clone(),
            snapshot: None,
            fields,
            type_mode: 0,
            generation_mode: 0,
            nullable: false,
            review: None,
            error: None,
        });
        self.busy = true;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = ddl::inspect_mysql_column(&db, &table, &name)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Column(Box::new(ColumnEvent::Inspected {
                id,
                result,
            })));
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    fn review_advanced_column(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(d) = &mut self.column_form else {
            return;
        };
        d.review = None;
        let result = (|| {
            anyhow::ensure!(d.route == self.tx.route, "Connection changed");
            let definition = d.draft(cx)?;
            let fingerprint = serde_json::to_string(&definition)?;
            let edit = ddl::plan_mysql_column_edit(d.snapshot.as_ref().unwrap(), definition)?;
            anyhow::Ok((fingerprint, edit))
        })();
        match result {
            Ok(review) => {
                d.review = Some(review);
                d.error = None;
            }
            Err(e) => d.error = Some(format!("{e:#}")),
        }
        cx.notify();
    }
    fn apply_advanced_column(&mut self, cx: &mut Context<Self>) {
        if self.busy
            || self.close_requested
            || self.transaction
            || !self.transaction_known
            || self.profile.as_ref().is_none_or(|p| p.read_only)
        {
            return;
        }
        let (Some(db), Some(d)) = (self.database.clone(), self.column_form.as_ref()) else {
            return;
        };
        let Some((fingerprint, edit)) = &d.review else {
            return;
        };
        if d.route != self.tx.route
            || !d
                .draft(cx)
                .and_then(|v| Ok(serde_json::to_string(&v)?))
                .is_ok_and(|v| &v == fingerprint)
        {
            self.fail("Column draft changed; review again".into());
            return;
        }
        let id = d.id;
        let edit = edit.clone();
        self.busy = true;
        let tx = self.tx.clone();
        self.column_form.as_mut().unwrap().review = None;
        self.runtime.spawn(async move {
            let result = apply_column(&db, &edit).await.map_err(|e| {
                format!("{e:#}; MySQL DDL may have committed; reopen inspection to verify")
            });
            let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Column(Box::new(ColumnEvent::Applied {
                id,
                result,
                transaction,
            })));
        });
        cx.notify();
    }
    pub fn receive_column(
        &mut self,
        event: ColumnEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (id, result, transaction) = match event {
            ColumnEvent::Inspected { id, result } => (id, result, None),
            ColumnEvent::Applied {
                id,
                result,
                transaction,
            } => (id, result, Some(transaction)),
        };
        let Some(d) = &mut self.column_form else {
            return;
        };
        if d.id != id || d.route != self.tx.route {
            return;
        }
        self.busy = false;
        if let Some(transaction) = transaction {
            match transaction {
                Ok(active) => {
                    self.transaction = active;
                    self.transaction_known = true;
                }
                Err(e) => {
                    self.transaction_known = false;
                    d.error = Some(e);
                }
            }
            self.structure = Default::default();
            self.table_result = Default::default();
        }
        match result {
            Ok(snapshot) => {
                d.nullable = snapshot.definition().base.column.nullable;
                d.fields["comment"].update(cx, |v, cx| {
                    v.set_value(snapshot.definition().base.comment.clone(), window, cx)
                });
                d.fields["generation expression"].update(cx, |v, cx| {
                    v.set_value(
                        snapshot
                            .definition()
                            .generation
                            .as_ref()
                            .map(|g| g.expression.clone())
                            .unwrap_or_default(),
                        window,
                        cx,
                    )
                });
                d.snapshot = Some(snapshot);
                d.type_mode = 0;
                d.generation_mode = 0;
                d.review = None;
            }
            Err(e) => d.error = Some(e),
        }
        cx.notify();
    }
    pub fn render_advanced_column(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let d = self.column_form.as_ref().unwrap();
        let free = !self.busy;
        let mut body=column().p_5().gap_3().child(row().child(label(format!("Advanced MySQL column · {}.{}.{}",d.table.schema,d.table.name,d.name),cx)).child(action("close-advanced-column","Close",false,free,cx,|s,w,cx|{s.column_form=None;w.focus(&s.focus,cx);cx.notify();})))
            .child(label("Full native attributes are retained. Types use a name, ENUM/SET JSON member array, DATETIME precision 0–6, or optional TINYINT display width. Generated expressions are validated; unsupported attribute combinations block Review.",cx));
        if let Some(snapshot) = &d.snapshot {
            body = body.child(label(
                format!(
                    "Inspected definition: {}",
                    serde_json::to_string_pretty(snapshot.definition()).unwrap_or_default()
                ),
                cx,
            ));
        }
        let mut types = row().flex_wrap();
        for (i, name) in TYPES.iter().enumerate() {
            types = types.child(action(
                ("column-type-mode", i),
                *name,
                d.type_mode == i,
                free,
                cx,
                move |s, _, cx| {
                    if let Some(d) = &mut s.column_form {
                        d.type_mode = i;
                        d.review = None;
                    }
                    cx.notify();
                },
            ));
        }
        body = body.child(types);
        let mut generation = row();
        for (i, name) in GENERATION.iter().enumerate() {
            generation = generation.child(action(
                ("column-generation-mode", i),
                *name,
                d.generation_mode == i,
                free,
                cx,
                move |s, _, cx| {
                    if let Some(d) = &mut s.column_form {
                        d.generation_mode = i;
                        d.review = None;
                    }
                    cx.notify();
                },
            ));
        }
        body = body.child(generation);
        for (key, value) in &d.fields {
            body = body.child(row().child(label(*key, cx)).child(ui::input(
                format!("advanced-column-{key}"),
                value,
                window,
                cx,
            )));
        }
        body = body
            .child(action(
                "column-nullable",
                format!("Nullable: {}", d.nullable),
                d.nullable,
                free,
                cx,
                |s, _, cx| {
                    if let Some(d) = &mut s.column_form {
                        d.nullable = !d.nullable;
                        d.review = None;
                    }
                    cx.notify();
                },
            ))
            .child(action(
                "review-advanced-column",
                "Review full definition SQL",
                true,
                free && d.snapshot.is_some(),
                cx,
                |s, _, cx| s.review_advanced_column(cx),
            ));
        if let Some((_, edit)) = &d.review {
            body = body.child(label(edit.plan().sql(), cx));
            for warning in &edit.plan().warnings {
                body = body.child(label(warning.clone(), cx));
            }
            body = body.child(action(
                "apply-advanced-column",
                "Apply reviewed column change",
                true,
                free && !self.transaction
                    && self.transaction_known
                    && self.profile.as_ref().is_some_and(|p| !p.read_only),
                cx,
                |s, _, cx| s.apply_advanced_column(cx),
            ));
        }
        if let Some(error) = &d.error {
            body = body.child(label(error.clone(), cx));
        }
        self.modal(
            div()
                .id("advanced-column-scroll")
                .w(px(960.))
                .max_h(window.viewport_size().height - px(70.))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
}
#[cfg(test)]
#[path = "column_tests.rs"]
mod tests;
