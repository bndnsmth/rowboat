//! Custom scalar cards are explicit one-shot operations; no timer calls this module.
use crate::{
    app::{Event, Page, Workbench},
    forms,
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{
    base::input::{InputState, TextareaState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_admin::metrics::{CustomMetricSample, CustomMetricState};
use rowboat_services::CustomMetricDefinition;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct CustomMetrics {
    pub editor: Option<MetricEditor>,
    samples: BTreeMap<
        String,
        (
            Route,
            CustomMetricDefinition,
            Result<CustomMetricSample, String>,
        ),
    >,
    pending: Option<(uuid::Uuid, CustomMetricDefinition)>,
}
pub struct MetricEditor {
    original: Option<CustomMetricDefinition>,
    id: String,
    owner: String,
    name: Entity<InputState>,
    sql: Entity<TextareaState>,
    enabled: bool,
}
pub struct Sampled {
    id: uuid::Uuid,
    definition: CustomMetricDefinition,
    result: Result<CustomMetricSample, String>,
    transaction: Result<bool, String>,
}

impl Workbench {
    pub fn edit_custom_metric(
        &mut self,
        id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.close_requested || self.modal_open() {
            return;
        }
        let Some(owner) = self.owner.clone() else {
            return;
        };
        let original = id.and_then(|id| {
            self.shared
                .borrow()
                .state
                .custom_metrics
                .iter()
                .find(|m| m.id == id && m.connection_id == owner)
                .cloned()
        });
        if id.is_some() && original.is_none() {
            return;
        }
        self.custom_metrics.editor = Some(MetricEditor {
            id: original
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            owner,
            name: forms::input(
                original
                    .as_ref()
                    .map(|m| m.name.as_str())
                    .unwrap_or("Custom scalar"),
                "Card name",
                window,
                cx,
            ),
            sql: forms::textarea(
                original
                    .as_ref()
                    .map(|m| m.sql.as_str())
                    .unwrap_or("SELECT 1"),
                window,
                cx,
            ),
            enabled: original.as_ref().is_some_and(|m| m.enabled),
            original,
        });
        window.focus(&self.modal_focus, cx);
        cx.notify();
    }
    pub fn save_custom_metric(&mut self, delete: bool, cx: &mut Context<Self>) {
        let outcome = (|| {
            let e = self
                .custom_metrics
                .editor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No metric draft"))?;
            anyhow::ensure!(
                !self.busy && !self.close_requested && self.owner.as_ref() == Some(&e.owner),
                "Metric owner changed or workspace is busy"
            );
            let shared = self.shared.borrow();
            anyhow::ensure!(shared.writable, "Metadata storage unavailable");
            anyhow::ensure!(
                shared.state.custom_metrics.iter().find(|m| m.id == e.id) == e.original.as_ref(),
                "Metric changed in another window; reopen it"
            );
            let definition = CustomMetricDefinition {
                id: e.id.clone(),
                name: e.name.read(cx).value().to_string(),
                connection_id: e.owner.clone(),
                sql: e.sql.read(cx).value().to_string(),
                enabled: e.enabled,
            };
            let mut state = shared.state.clone();
            state.custom_metrics.retain(|m| m.id != definition.id);
            if delete {
                for dashboard in &mut state.dashboards {
                    dashboard
                        .cards
                        .retain(|c| c.metric != definition.metric_key());
                }
            } else {
                state.custom_metrics.push(definition.clone());
            }
            rowboat_services::validate_app_state(&state)?;
            anyhow::Ok((state, definition.id))
        })();
        match outcome {
            Ok((state, id)) => {
                self.edit_state(|s| *s = state);
                self.custom_metrics.samples.remove(&id);
                self.custom_metrics.editor = None;
                self.pending_focus = Some(self.focus.clone());
                self.status = "Custom metric metadata saved · samples are memory-only".into();
            }
            Err(e) => self.fail(format!("Custom metric: {e:#}")),
        }
        cx.notify();
    }
    pub fn custom_can_sample(&self, m: &CustomMetricDefinition) -> bool {
        m.enabled
            && self.page == Page::Dashboard
            && self.owner.as_ref() == Some(&m.connection_id)
            && self
                .profile
                .as_ref()
                .is_some_and(|p| p.id == m.connection_id)
            && self.database.is_some()
            && !self.busy
            && !self.transaction
            && self.transaction_known
            && !self.close_requested
            && !self.modal_open()
            && !self.has_row_draft()
            && self.changes.is_empty()
            && !self.shared.borrow().shell_modal_open
    }
    pub fn sample_custom_metric(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(definition) = self
            .shared
            .borrow()
            .state
            .custom_metrics
            .iter()
            .find(|m| m.id == id)
            .cloned()
        else {
            return;
        };
        if !self.custom_can_sample(&definition) || self.custom_metrics.pending.is_some() {
            return;
        }
        let db = self.database.clone().unwrap();
        let tx = self.tx.clone();
        let id = uuid::Uuid::new_v4();
        self.custom_metrics.pending = Some((id, definition.clone()));
        self.busy = true;
        self.status = "Sampling custom card once in a bounded READ ONLY transaction…".into();
        self.runtime.spawn(async move {
            let result = rowboat_admin::metrics::sample_custom(&db, &definition.sql)
                .await
                .map_err(|e| format!("{e:#}"));
            let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::CustomMetric(Box::new(Sampled {
                id,
                definition,
                result,
                transaction,
            })));
        });
        cx.notify();
    }
    pub fn receive_custom_metric(&mut self, event: Sampled, cx: &mut Context<Self>) {
        if self.custom_metrics.pending.as_ref().map(|p| p.0) != Some(event.id) {
            return;
        }
        self.custom_metrics.pending = None;
        self.busy = false;
        self.custom_metrics.samples.retain(|id, (route, _, _)| {
            *route == self.tx.route
                && self
                    .shared
                    .borrow()
                    .state
                    .custom_metrics
                    .iter()
                    .any(|m| &m.id == id)
        });
        match event.transaction {
            Ok(active) => {
                self.transaction = active;
                self.transaction_known = true;
            }
            Err(e) => {
                self.transaction_known = false;
                self.fail(e);
            }
        }
        if self.owner.as_ref() == Some(&event.definition.connection_id)
            && self
                .shared
                .borrow()
                .state
                .custom_metrics
                .iter()
                .any(|m| m == &event.definition)
        {
            self.custom_metrics.samples.insert(
                event.definition.id.clone(),
                (self.tx.route, event.definition, event.result),
            );
            self.status =
                "Custom sample finished · explicit sampling only; values not persisted".into();
        } else {
            self.status = "Custom metric changed during sampling; result discarded".into();
        }
        cx.notify();
    }
    pub fn render_custom_card(&self, id: &str, cx: &mut Context<Self>) -> Div {
        let m = self
            .shared
            .borrow()
            .state
            .custom_metrics
            .iter()
            .find(|m| m.id == id && Some(&m.connection_id) == self.owner.as_ref())
            .cloned();
        let Some(m) = m else {
            return column().p_3().child("Custom definition unavailable");
        };
        let sample = self
            .custom_metrics
            .samples
            .get(id)
            .filter(|(route, definition, _)| *route == self.tx.route && definition == &m);
        let text = match sample.map(|s| &s.2) {
            None => "Not sampled for this definition/session".into(),
            Some(Err(e)) => format!("Sample failed: {e}"),
            Some(Ok(s)) => match &s.state {
                CustomMetricState::Value { decimal } => {
                    format!("{decimal} · sampled at {} ms UTC", s.sampled_at_unix_ms)
                }
                CustomMetricState::Empty => "Empty result (not zero)".into(),
                CustomMetricState::Invalid { reason } => {
                    format!("Invalid scalar (not zero): {reason}")
                }
            },
        };
        let sample_id = m.id.clone();
        let edit_id = m.id.clone();
        column()
            .min_w(px(220.))
            .p_3()
            .gap_2()
            .border_1()
            .border_color(cx.omarchy().border)
            .child(label(
                format!(
                    "{} · {}",
                    m.name,
                    if m.enabled { "enabled" } else { "disabled" }
                ),
                cx,
            ))
            .child(label(text, cx))
            .child(label(
                "Last explicit sample only; never automatically refreshed",
                cx,
            ))
            .child(
                row()
                    .flex_wrap()
                    .child(action(
                        format!("sample-custom-{id}"),
                        "Sample once",
                        false,
                        self.custom_can_sample(&m),
                        cx,
                        move |s, _, cx| s.sample_custom_metric(&sample_id, cx),
                    ))
                    .child(action(
                        format!("edit-custom-{id}"),
                        "Edit definition…",
                        false,
                        !self.busy,
                        cx,
                        move |s, w, cx| s.edit_custom_metric(Some(&edit_id), w, cx),
                    )),
            )
    }
    pub fn render_custom_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let e = self.custom_metrics.editor.as_ref().unwrap();
        let body=column().w(px(760.).min(window.viewport_size().width-px(64.))).p_4().gap_3()
            .child(label("Custom SQL metric · explicit one-shot sampling",cx))
            .child(ui::input("custom-metric-name",&e.name,window,cx))
            .child(ui::textarea("custom-metric-sql",&e.sql,window,cx).h(px(180.)))
            .child(label("One numeric scalar only. Server READ ONLY, row/output/time bounds and rollback are enforced. SELECT functions may still have non-database side effects. SQL definitions are stored locally; do not embed secrets. Samples are never persisted.",cx))
            .child(action("custom-metric-enabled",if e.enabled{"Enabled · click to disable"}else{"Disabled · click to enable"},false,true,cx,|s,_,cx|{if let Some(e)=&mut s.custom_metrics.editor{e.enabled = !e.enabled;}cx.notify();}))
            .child(row().flex_wrap().child(action("save-custom-metric","Save definition",true,!self.busy,cx,|s,_,cx|s.save_custom_metric(false,cx)))
                .child(action("delete-custom-metric","Delete definition and its card references",false,!self.busy&&e.original.is_some(),cx,|s,_,cx|s.save_custom_metric(true,cx))));
        self.modal(body.into_any_element(), cx)
    }
}
