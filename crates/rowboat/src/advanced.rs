//! Native inspection tools and review-first schema/query helpers.
use crate::{
    app::*,
    forms,
    view::{action, column, label, row, section},
};
use gpui_kit::{
    base::input::{InputEvent, InputState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_admin::{
    ddl::*,
    metrics::MetricsSample,
    objects::{self, DatabaseObject, ObjectKind},
};
use rowboat_core::{Engine, TableRef};

pub struct DesignColumn {
    name: Entity<InputState>,
    data_type: Entity<InputState>,
    nullable: bool,
    primary: bool,
}
pub struct TableDesigner {
    schema: Entity<InputState>,
    name: Entity<InputState>,
    columns: Vec<DesignColumn>,
    pub(crate) target: Option<crate::management::Target>,
    pub(crate) review: Option<crate::management::Review>,
}
pub struct AdvancedState {
    pub objects: Vec<DatabaseObject>,
    pub object_kind: ObjectKind,
    pub objects_loaded: bool,
    pub selected_object: Option<usize>,
    pub object_filter: Entity<InputState>,
    pub metrics: Option<MetricsSample>,
    pub inline: crate::inline_completion::State,
    pub completion_menus: std::collections::HashMap<
        usize,
        std::rc::Rc<std::cell::RefCell<crate::inline_completion::Menu>>,
    >,
    pub designer: Option<TableDesigner>,
    _subscriptions: Vec<Subscription>,
}
impl AdvancedState {
    pub fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
        let object_filter = forms::input("", "Filter database objects…", window, cx);
        let subscriptions =
            vec![cx.subscribe(&object_filter, |_, _, _: &InputEvent, cx| cx.notify())];
        Self {
            objects: vec![],
            object_kind: ObjectKind::Function,
            objects_loaded: false,
            selected_object: None,
            object_filter,
            metrics: None,
            inline: Default::default(),
            completion_menus: Default::default(),
            designer: None,
            _subscriptions: subscriptions,
        }
    }
    pub fn reset_connection(&mut self) {
        self.objects.clear();
        self.objects_loaded = false;
        self.selected_object = None;
        self.metrics = None;
        self.inline.invalidate();
    }
}

impl Workbench {
    pub fn render_parameters(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.modal(column().w(px(660.)).p_6().gap_4()
            .child(row().child(div().text_size(px(21.)).font_weight(FontWeight::SEMIBOLD).child("Named query parameters")).child(div().flex_1()).child(action("close-parameters","✕",false,true,cx,|s,_,cx|{s.tabs[s.active_tab].show_parameters=false;cx.notify();})))
            .child(label("Use :name in SQL and a JSON object here: {\"name\": \"value\", \"limit\": 20}",cx))
            .child(ui::textarea("parameter-json",&self.tabs[self.active_tab].parameters,window,cx).h(px(210.)))
            .child(label("Values are escaped for the connected dialect. NULL is JSON null. Parameters stay in memory; history stores the SQL template.",cx))
            .child(row().child(div().flex_1()).child(action("apply-parameters","Done",false,true,cx,|s,_,cx|{s.tabs[s.active_tab].show_parameters=false;cx.notify();}))
                .child(action("run-parameters","Run query",true,!self.busy&&self.database.is_some(),cx,|s,_,cx|{s.tabs[s.active_tab].show_parameters=false;s.run_query(cx);}))).into_any_element(),cx)
    }
    pub fn inspect_query(
        &mut self,
        title: &str,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_leave() {
            cx.notify();
            return;
        }
        self.new_query(Some(sql), window, cx);
        self.tabs[self.active_tab].title = title.to_owned();
        self.run_query(cx);
    }
    pub fn load_objects(&mut self, kind: ObjectKind, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::ObjectCatalog(kind), cx) {
            return;
        }
        if self.busy {
            return;
        }
        let Some(db) = self.database.clone() else {
            return;
        };
        self.busy = true;
        self.error = None;
        self.advanced.object_kind = kind;
        self.advanced.objects.clear();
        self.advanced.selected_object = None;
        self.advanced.objects_loaded = false;
        self.status = format!("Loading {kind:?} objects…");
        let tx = self.tx.clone();
        let runtime = self.runtime.clone();
        self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let _ = tx.send(Event::Objects(
                    objects::browse_objects(&db, kind)
                        .await
                        .map_err(|e| format!("{e:#}")),
                ));
            })
        });
        cx.notify();
    }
    pub fn sample_metrics(&mut self, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::ServerMetrics, cx) {
            return;
        }
        if self.busy {
            return;
        }
        let Some(db) = self.database.clone() else {
            return;
        };
        self.busy = true;
        self.error = None;
        self.status = "Sampling server metrics…".into();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let _ = tx.send(Event::Metrics(
                rowboat_admin::metrics::sample(&db)
                    .await
                    .map_err(|e| format!("{e:#}")),
            ));
        });
        cx.notify();
    }
    pub fn complete_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_context_completion(window, cx);
    }
    pub fn close_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_extension_dialog() {
            return;
        }
        if self.url_input.is_some() {
            return;
        }
        if self.management.is_some() {
            return;
        }
        if !self.session_ready
            || self.busy
            || self.library.file_pending
            || self.page != Page::Query
            || self.form.is_some()
            || self.has_row_draft()
            || self.transfer.is_some()
            || self.close_requested
            || self.close_tab_requested.is_some()
            || self.advanced.designer.is_some()
            || self.quick_open
        {
            return;
        }
        let tab = &self.tabs[self.active_tab];
        if !tab.editor.read(cx).value().trim().is_empty() {
            self.close_tab_requested = Some(tab.id);
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        self.finish_close_query(tab.id, window, cx);
    }
    pub fn finish_close_query(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.library.file_pending || !self.tabs.iter().any(|tab| tab.id == id) {
            return;
        }
        let workspace = self
            .tabs
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.connection_id.clone());
        self.tabs.retain(|t| t.id != id);
        self.library.documents.remove(&id);
        self.results.forget_query(id);
        self.execution.launches.remove(&id);
        self.editor_observers.remove(&id);
        self.advanced.completion_menus.remove(&id);
        self.query_edit.results.remove(&id);
        self.editor_focus.remove(&id);
        let remaining = self.tabs.iter().position(|t| t.connection_id == workspace);
        self.active_tab = remaining.unwrap_or(0);
        self.close_tab_requested = None;
        if remaining.is_none() {
            self.new_query(None, window, cx);
            self.tabs[self.active_tab].connection_id = workspace;
        }
        self.selected = None;
        self.widths.clear();
        self.reconcile_panes();
        self.queue_session_save(cx);
        cx.notify();
    }
    pub fn render_close_guard(&self, cx: &mut Context<Self>) -> AnyElement {
        let id = self.close_tab_requested;
        self.modal(column().w(px(570.)).p_6().gap_4()
            .child(div().text_size(px(21.)).font_weight(FontWeight::SEMIBOLD).child("Close this SQL tab?"))
            .child("Closing removes this SQL buffer from recovery. Executed queries remain in history; favorites remain saved.")
            .when(self.busy,|d|d.child(label("An operation is still running. Wait or cancel it before closing.",cx)))
            .child(row().child(action("keep-open","Keep working",true,true,cx,|s,w,cx|{s.close_tab_requested=None;s.tabs[s.active_tab].editor.update(cx,|e,cx|e.focus(w,cx));cx.notify();}))
                .child(div().flex_1())
                .child(action("close-cancel-operation","Cancel query",false,self.busy&&self.database.is_some()&&!self.transfer_running,cx,|s,_,cx|s.cancel(cx)))
                .child(action("confirm-close","Discard & close tab",false,!self.busy,cx,move|s,w,cx|{if let Some(id)=id{s.finish_close_query(id,w,cx);}}))).into_any_element(),cx)
    }
    pub fn show_designer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::SchemaDdl, cx) {
            return;
        }
        if !self.can_leave() {
            cx.notify();
            return;
        }
        let sqlite = self
            .profile
            .as_ref()
            .is_some_and(|p| p.engine == Engine::Sqlite);
        if sqlite && let Err(error) = self.management_guard(false) {
            self.fail(error);
            cx.notify();
            return;
        }
        let schema = self
            .table
            .as_ref()
            .map(|t| t.schema.clone())
            .unwrap_or_else(|| self.namespace_scope().unwrap_or_default().to_owned());
        self.advanced.designer = Some(TableDesigner {
            target: self.management_target(false),
            review: None,
            schema: forms::input(schema, "Schema", window, cx),
            name: forms::input("", "Table name", window, cx),
            columns: vec![DesignColumn {
                name: forms::input("id", "Column", window, cx),
                data_type: forms::input(
                    if sqlite { "integer" } else { "bigint" },
                    "Type",
                    window,
                    cx,
                ),
                nullable: false,
                primary: true,
            }],
        });
        cx.notify();
    }
    pub fn preview_table_design(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(design), Some(profile)) = (&self.advanced.designer, &self.profile) else {
            return;
        };
        let result = (|| {
            let name = design.name.read(cx).value().trim().to_owned();
            let schema = design.schema.read(cx).value().trim().to_owned();
            let columns = design
                .columns
                .iter()
                .map(|c| {
                    Ok(ColumnDesign {
                        name: c.name.read(cx).value().trim().to_owned(),
                        data_type: DataType::parse(profile.engine, &c.data_type.read(cx).value())?,
                        nullable: c.nullable && !c.primary,
                        default: None,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let keys: Vec<_> = design
                .columns
                .iter()
                .filter(|c| c.primary)
                .map(|c| c.name.read(cx).value().trim().to_owned())
                .collect();
            let primary_key = (!keys.is_empty()).then(|| PrimaryKeyDesign {
                name: format!("{name}_pkey"),
                columns: keys,
            });
            Ok::<_, anyhow::Error>(TableDesign {
                table: TableRef { schema, name },
                columns,
                primary_key,
                indexes: vec![],
                foreign_keys: vec![],
            })
        })();
        if profile.engine == Engine::Sqlite {
            if let Err(error) = self.management_guard(false) {
                self.fail(error);
                cx.notify();
                return;
            }
            let Some(target) = self.management_target(false) else {
                return;
            };
            if design.target.as_ref() != Some(&target) {
                self.fail("Designer belongs to a different session; reopen it".into());
                cx.notify();
                return;
            }
            match result {
                Ok(design) => {
                    self.set_designer_disabled(true, cx);
                    self.busy = true;
                    self.error = None;
                    let db = self.database.clone().unwrap();
                    let tx = self.tx.clone();
                    self.runtime.spawn(async move {
                        let result = crate::management::prepare_table(&db, target.clone(), design)
                            .await
                            .map(Box::new)
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Event::Management(Box::new(
                            crate::management_ui::ManagementEvent::TablePreview { target, result },
                        )));
                    });
                }
                Err(error) => self.fail(error.to_string()),
            }
            cx.notify();
            return;
        }
        let result =
            result.and_then(|design| preview(profile.engine, &[DdlOperation::CreateTable(design)]));
        match result {
            Ok(plan) => {
                let warnings = plan
                    .warnings
                    .iter()
                    .map(|w| format!("-- {w}\n"))
                    .collect::<String>();
                self.advanced.designer = None;
                self.new_query(
                    Some(format!(
                        "-- Review this schema change, then Run to execute.\n{warnings}{}",
                        plan.sql()
                    )),
                    window,
                    cx,
                );
                self.tabs[self.active_tab].title = "Create table".into();
                self.status = "DDL generated for review; nothing has been executed".into();
            }
            Err(e) => self.fail(e.to_string()),
        }
        cx.notify();
    }

    pub(crate) fn set_designer_disabled(&self, disabled: bool, cx: &mut Context<Self>) {
        if let Some(design) = &self.advanced.designer {
            for field in [&design.schema, &design.name]
                .into_iter()
                .chain(design.columns.iter().flat_map(|c| [&c.name, &c.data_type]))
            {
                field.update(cx, |f, cx| f.set_disabled(disabled, cx));
            }
        }
    }

    fn apply_table_design(&mut self, cx: &mut Context<Self>) {
        let result = (|| {
            self.management_guard(false)?;
            let target = self.management_target(false).ok_or("Connection closed")?;
            let review = self
                .advanced
                .designer
                .as_ref()
                .and_then(|d| d.review.as_ref())
                .ok_or("Review the table design first")?;
            review
                .validate(
                    &target,
                    crate::management::Operation::CreateTable,
                    self.profile.as_ref().unwrap().read_only,
                    self.busy,
                    !self.changes.is_empty() || self.has_row_draft(),
                    self.transaction_known.then_some(self.transaction),
                )
                .map_err(|e| e.to_string())
        })();
        if let Err(error) = result {
            self.fail(error);
            cx.notify();
            return;
        }
        let review = self
            .advanced
            .designer
            .as_mut()
            .unwrap()
            .review
            .take()
            .unwrap();
        self.start_management_apply(review, cx);
    }

    pub fn render_tools(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        use rowboat_admin::providers::Operation as Tool;
        let t = cx.omarchy().clone();
        let free = !self.busy;
        let connected = self.database.is_some();
        let engine = self.profile.as_ref().map(|p| p.engine).unwrap_or_default();
        let mut body=column().p_6().gap_3().child(row().child(column().gap_1().child(div().text_size(px(23.)).font_weight(FontWeight::SEMIBOLD).child("Database tools")).child(label("Live inspection, server counters, and review-first schema changes.",cx))).child(div().flex_1())
            .child(action("design-table","+ Create table…",true,connected&&free&&self.admin_supported(Tool::SchemaDdl)&&self.profile.as_ref().is_some_and(|p|!p.read_only),cx,|s,w,cx|s.show_designer(w,cx))))
            .child(section("SERVER",cx))
            .child(row().gap_3()
                .child(action("server-info","Connection info",false,connected&&free&&self.admin_supported(Tool::ConnectionInfo),cx,|s,w,cx|s.inspect_admin(Tool::ConnectionInfo,w,cx)))
                .child(action("server-activity","Server activity",false,connected&&free&&self.admin_supported(Tool::ServerActivity),cx,|s,w,cx|s.inspect_admin(Tool::ServerActivity,w,cx)))
                .child(action("server-databases","Databases",false,connected&&free&&self.admin_supported(Tool::Namespaces),cx,|s,w,cx|s.inspect_admin(Tool::Namespaces,w,cx)))
                .child(action("server-metrics","Sample metrics",false,connected&&free&&self.admin_supported(Tool::ServerMetrics),cx,|s,_,cx|s.sample_metrics(cx)))
                .child(action("backup-database","Backup…",false,connected&&free&&self.service_supported(rowboat_services::ServiceOperation::Backup),cx,|s,w,cx|s.show_transfer(crate::services_ui::TransferKind::Backup,w,cx)))
                .child(action("restore-database","Restore…",false,connected&&free&&self.service_supported(rowboat_services::ServiceOperation::Restore)&&self.profile.as_ref().is_some_and(|p|!p.read_only),cx,|s,w,cx|s.show_transfer(crate::services_ui::TransferKind::Restore,w,cx))));
        for (name, support) in [
            ("Administration", engine.descriptor().tools.administration),
            ("Server metrics", engine.descriptor().tools.metrics),
            ("Schema designer", engine.descriptor().tools.schema),
            ("Backup / restore", engine.descriptor().tools.backup),
        ] {
            if let rowboat_core::ToolSupport::Unsupported(reason) = support {
                body = body.child(label(format!("{name} unavailable · {reason}"), cx));
            }
        }
        if let Some(sample) = &self.advanced.metrics {
            body = body.child(label(
                format!(
                    "Snapshot · {} · Unix {} ms · cumulative counters, not rates",
                    sample.scope, sample.sampled_at_unix_ms
                ),
                cx,
            ));
            let mut metrics = row().flex_wrap().gap_3();
            for metric in &sample.metrics {
                metrics = metrics.child(
                    column()
                        .w(px(210.))
                        .p_3()
                        .gap_2()
                        .bg(t.surface)
                        .border_1()
                        .border_color(t.divider())
                        .child(
                            div()
                                .text_size(px(23.))
                                .text_color(t.accent)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(metric.value.to_string()),
                        )
                        .child(div().text_size(px(11.)).child(metric.name.clone()))
                        .child(label(format!("{} · {:?}", metric.unit, metric.kind), cx)),
                );
            }
            body = body.child(metrics);
        }
        body = body.child(section("DATABASE OBJECTS", cx));
        body = body.child(
            row()
                .flex_wrap()
                .child(action(
                    "effective-access-matrix",
                    "Effective permissions matrix…",
                    true,
                    connected && free && self.admin_supported(Tool::AccessManagement),
                    cx,
                    |s, w, cx| s.show_access_matrix(w, cx),
                ))
                .child(action(
                    "manage-namespaces",
                    "Create / drop schema or database…",
                    false,
                    connected && free && self.admin_supported(Tool::NamespaceDdl),
                    cx,
                    |s, w, cx| {
                        s.show_management(crate::management::Operation::CreateDatabase, None, w, cx)
                    },
                ))
                .child(action(
                    "manage-access",
                    "Roles, users & privileges…",
                    false,
                    connected && free && self.admin_supported(Tool::AccessManagement),
                    cx,
                    |s, w, cx| {
                        s.show_management(
                            crate::management::Operation::InspectPrivileges,
                            None,
                            w,
                            cx,
                        )
                    },
                )),
        );
        let kinds = [
            ("Functions", ObjectKind::Function),
            ("Procedures", ObjectKind::Procedure),
            ("Triggers", ObjectKind::Trigger),
            ("Views", ObjectKind::View),
            ("Materialized views", ObjectKind::MaterializedView),
            ("Roles / users", ObjectKind::Role),
            ("Sequences", ObjectKind::Sequence),
        ];
        let mut kinds_row = row().flex_wrap();
        for (name, kind) in kinds {
            if !rowboat_admin::providers::supported_object_kinds(engine).contains(&kind) {
                continue;
            }
            kinds_row = kinds_row.child(
                action(name, name, false, connected && free, cx, move |s, _, cx| {
                    s.load_objects(kind, cx)
                })
                .selected(self.advanced.object_kind == kind && self.advanced.objects_loaded),
            );
        }
        body = body.child(kinds_row).child(ui::input(
            "objects-filter",
            &self.advanced.object_filter,
            window,
            cx,
        ));
        let filter = self.advanced.object_filter.read(cx).value().to_lowercase();
        if !self.advanced.objects_loaded {
            body = body.child(label(
                if rowboat_admin::providers::supported_object_kinds(engine).is_empty() {
                    "Advanced object catalog unavailable. Tables and views are available in the explorer."
                } else if self.busy {
                    "Reading database catalog…"
                } else {
                    "Choose an object category to inspect the connected database."
                },
                cx,
            ));
        } else if self.advanced.objects.is_empty() {
            body = body.child(label("No objects returned for this category.", cx));
        }
        for (i, object) in self.advanced.objects.iter().enumerate().filter(|(_, o)| {
            format!("{}.{}", o.schema.as_deref().unwrap_or(""), o.name)
                .to_lowercase()
                .contains(&filter)
        }) {
            body = body.child(
                action(
                    ("database-object", i),
                    format!(
                        "{}{}{}",
                        object
                            .schema
                            .as_ref()
                            .map(|s| format!("{s}. "))
                            .unwrap_or_default(),
                        object.name,
                        object
                            .details
                            .as_ref()
                            .map(|d| format!("    {d}"))
                            .unwrap_or_default()
                    ),
                    false,
                    true,
                    cx,
                    move |s, _, cx| {
                        s.advanced.selected_object = Some(i);
                        cx.notify();
                    },
                )
                .w_full()
                .justify_start()
                .selected(self.advanced.selected_object == Some(i)),
            );
        }
        if let Some(object) = self
            .advanced
            .selected_object
            .and_then(|i| self.advanced.objects.get(i))
        {
            let definition = object.definition.clone();
            let owner = object.owner.as_deref().unwrap_or("not reported");
            let title = object.name.clone();
            body=body.child(section(format!("{} · owner: {}",title,owner),cx)).child(row()
                .child(action("copy-object","Copy definition",false,definition.is_some(),cx,{let definition=definition.clone();move|_,_,cx|{if let Some(sql)=&definition{cx.write_to_clipboard(ClipboardItem::new_string(sql.clone()));}}}))
                .child(action("edit-object","Open definition in SQL tab",false,definition.is_some(),cx,{let definition=definition.clone();move|s,w,cx|{if let Some(sql)=&definition{s.new_query(Some(sql.clone()),w,cx);s.tabs[s.active_tab].title=title.clone();}}})))
                .child(div().p_4().bg(t.inset).font_family(t.mono_font.clone()).text_size(px(12.)).child(definition.unwrap_or_else(||"This object does not expose a definition. Inspect its catalog details above.".into())));
        }
        div()
            .id("tools-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(body)
            .into_any_element()
    }

    pub fn render_designer(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let design = self.advanced.designer.as_ref().unwrap();
        let sqlite = self
            .profile
            .as_ref()
            .is_some_and(|p| p.engine == Engine::Sqlite);
        if let Some(review) = &design.review {
            let theme = cx.omarchy().clone();
            let mut body = column()
                .p_6()
                .gap_3()
                .child(section("CREATE TABLE · REVIEW · NOTHING EXECUTED", cx))
                .child(label(review.target.label(), cx))
                .child(label(review.execution_note(), cx));
            for warning in &review.display.warnings {
                body = body.child(label(warning.clone(), cx));
            }
            body = body
                .child(
                    div()
                        .p_3()
                        .bg(theme.inset)
                        .font_family(theme.mono_font.clone())
                        .child(review.display.sql()),
                )
                .child(
                    row()
                        .child(
                            action(
                                "design-back",
                                "← Edit form",
                                false,
                                !self.busy,
                                cx,
                                |s, _, cx| {
                                    s.advanced.designer.as_mut().unwrap().review = None;
                                    s.set_designer_disabled(false, cx);
                                    cx.notify();
                                },
                            )
                            .debug_selector(|| "design-back".into()),
                        )
                        .child(div().flex_1())
                        .child(
                            action(
                                "design-apply",
                                "Apply reviewed change",
                                true,
                                !self.busy,
                                cx,
                                |s, _, cx| s.apply_table_design(cx),
                            )
                            .debug_selector(|| "design-apply".into()),
                        ),
                );
            return self.modal(
                div()
                    .id("designer-review-scroll")
                    .w(px(820.))
                    .max_h(window.viewport_size().height - px(64.))
                    .overflow_y_scroll()
                    .child(body)
                    .into_any_element(),
                cx,
            );
        }
        let mut body = column()
            .p_6()
            .gap_4()
            .child(
                row()
                    .child(
                        div()
                            .text_size(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Create table"),
                    )
                    .child(div().flex_1())
                    .child(action(
                        "close-designer",
                        "✕",
                        false,
                        !self.busy,
                        cx,
                        |s, w, cx| {
                            s.advanced.designer = None;
                            w.focus(&s.focus, cx);
                            cx.notify();
                        },
                    )),
            )
            .child(label(
                "Build a typed table definition, then review the generated SQL before execution.",
                cx,
            ))
            .child(
                row()
                    .gap_3()
                    .child(
                        column()
                            .flex_1()
                            .gap_2()
                            .child(label(
                                if sqlite {
                                    "SQLITE NAMESPACE (main / temp / attached)"
                                } else {
                                    "SCHEMA / DATABASE"
                                },
                                cx,
                            ))
                            .child(ui::input("design-schema", &design.schema, window, cx)),
                    )
                    .child(
                        column()
                            .flex_1()
                            .gap_2()
                            .child(label("TABLE NAME", cx))
                            .child(
                                ui::input("design-table", &design.name, window, cx)
                                    .debug_selector(|| "design-table".into()),
                            ),
                    ),
            )
            .child(section("COLUMNS", cx));
        if sqlite {
            body = body.child(label(
                design
                    .target
                    .as_ref()
                    .map(|t| format!("SQLite file: {}", t.database))
                    .unwrap_or_default(),
                cx,
            ));
        }
        if let Some(error) = &self.error {
            body = body.child(label(error.clone(), cx));
        }
        for (i, col) in design.columns.iter().enumerate() {
            body = body.child(
                row()
                    .gap_3()
                    .child(div().w(px(200.)).child(ui::input(
                        ("design-column", i),
                        &col.name,
                        window,
                        cx,
                    )))
                    .child(div().w(px(180.)).child(ui::input(
                        ("design-type", i),
                        &col.data_type,
                        window,
                        cx,
                    )))
                    .child(action(
                        ("design-null", i),
                        if col.nullable { "NULL" } else { "NOT NULL" },
                        false,
                        !self.busy && !col.primary,
                        cx,
                        move |s, _, cx| {
                            if let Some(d) = &mut s.advanced.designer {
                                d.columns[i].nullable = !d.columns[i].nullable;
                            }
                            cx.notify();
                        },
                    ))
                    .child(
                        action(
                            ("design-primary", i),
                            "Primary",
                            false,
                            !self.busy,
                            cx,
                            move |s, _, cx| {
                                if let Some(d) = &mut s.advanced.designer {
                                    d.columns[i].primary = !d.columns[i].primary;
                                    if d.columns[i].primary {
                                        d.columns[i].nullable = false;
                                    }
                                }
                                cx.notify();
                            },
                        )
                        .selected(col.primary),
                    )
                    .child(action(
                        ("design-remove", i),
                        "−",
                        false,
                        !self.busy && design.columns.len() > 1,
                        cx,
                        move |s, _, cx| {
                            if let Some(d) = &mut s.advanced.designer {
                                d.columns.remove(i);
                            }
                            cx.notify();
                        },
                    )),
            );
        }
        body = body.child(
            row()
                .child(action(
                    "design-add",
                    "+ Column",
                    false,
                    !self.busy,
                    cx,
                    |s, w, cx| {
                        if let Some(d) = &mut s.advanced.designer {
                            d.columns.push(DesignColumn {
                                name: forms::input("", "Column name", w, cx),
                                data_type: forms::input("text", "Data type", w, cx),
                                nullable: true,
                                primary: false,
                            });
                        }
                        cx.notify();
                    },
                ))
                .child(div().flex_1())
                .child(
                    action(
                        "design-preview",
                        if sqlite {
                            "Review SQL →"
                        } else {
                            "Generate SQL →"
                        },
                        true,
                        !self.busy,
                        cx,
                        |s, w, cx| s.preview_table_design(w, cx),
                    )
                    .debug_selector(|| "design-preview".into()),
                ),
        );
        self.modal(
            div()
                .id("designer-scroll")
                .w(px(820.))
                .max_h((window.viewport_size().height - px(64.)).min(px(740.)))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
}
