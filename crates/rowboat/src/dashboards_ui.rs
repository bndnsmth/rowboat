//! Saved builtin metric cards; polling is serialized and pauses while parked or transactional.
use crate::{
    app::{Event, Page, Workbench},
    forms,
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_admin::metrics::{DashboardDelta, DashboardSample, DeltaState, MetricKind};
use rowboat_services::{Dashboard, DashboardCard};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct DashboardState {
    selected: Option<String>,
    route: Option<Route>,
    enabled: bool,
    next: Option<Instant>,
    pending: Option<uuid::Uuid>,
    sample: Option<DashboardSample>,
    delta: Option<DashboardDelta>,
    series: BTreeMap<String, VecDeque<(u64, f64)>>,
    error: Option<String>,
    pub editor: Option<DashboardEditor>,
}
pub struct DashboardEditor {
    original: Option<Dashboard>,
    connection: String,
    name: Entity<InputState>,
    interval: Entity<InputState>,
    columns: u8,
    metrics: BTreeSet<String>,
}
pub struct Sampled {
    request: uuid::Uuid,
    result: Result<DashboardSample, String>,
    transaction: Result<bool, String>,
}
fn rate_text(delta: u64, elapsed_ms: u64) -> String {
    if elapsed_ms == 0 {
        return "Unavailable interval".into();
    }
    let scaled = u128::from(delta) * 1_000_000 / u128::from(elapsed_ms);
    format!("{}.{:03}/s", scaled / 1000, scaled % 1000)
}
fn append(series: &mut VecDeque<(u64, f64)>, timestamp: u64, value: f64) {
    if series.back().is_some_and(|(last, _)| *last >= timestamp) {
        series.clear();
    }
    series.push_back((timestamp, value));
    while series.len() > 120 {
        series.pop_front();
    }
}
impl Workbench {
    fn selected_dashboard(&self) -> Option<&Dashboard> {
        self.state.dashboards.iter().find(|d| {
            Some(&d.id) == self.dashboards.selected.as_ref()
                && Some(&d.connection_id) == self.owner.as_ref()
        })
    }
    pub fn show_dashboards(&mut self, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::ServerMetrics, cx) {
            return;
        }
        if !self.can_leave() {
            cx.notify();
            return;
        }
        self.page = Page::Dashboard;
        if self.selected_dashboard().is_none() {
            self.dashboards.selected = self
                .state
                .dashboards
                .iter()
                .find(|d| Some(&d.connection_id) == self.owner.as_ref())
                .map(|d| d.id.clone());
        }
        cx.notify();
    }
    fn dashboard_can_sample(&self) -> bool {
        self.page == Page::Dashboard
            && self.admin_supported(rowboat_admin::providers::Operation::ServerMetrics)
            && self.shared.borrow().active_workspace == self.workspace_id
            && !self.shared.borrow().shell_modal_open
            && self.database.is_some()
            && !self.busy
            && !self.transaction
            && self.transaction_known
            && !self.close_requested
            && !self.has_extension_dialog()
            && !self.has_row_draft()
            && self.changes.is_empty()
            && self.form.is_none()
            && self.management.is_none()
            && self.transfer.is_none()
            && !self.quick_open
            && self.advanced.designer.is_none()
    }
    pub fn poll_dashboard(&mut self, cx: &mut Context<Self>) {
        if self.dashboards.route.is_some_and(|r| r != self.tx.route) {
            let selected = self.dashboards.selected.clone();
            self.dashboards = DashboardState {
                selected,
                ..Default::default()
            };
        }
        if self.dashboards.enabled
            && self.dashboard_can_sample()
            && self.dashboards.next.is_none_or(|t| Instant::now() >= t)
        {
            self.sample_dashboard(cx);
        }
    }
    fn sample_dashboard(&mut self, cx: &mut Context<Self>) {
        if !self.dashboard_can_sample() || self.dashboards.pending.is_some() {
            return;
        }
        let Some(definition) = self.selected_dashboard() else {
            return;
        };
        let interval = definition.refresh_interval_secs;
        let Some(db) = self.database.clone() else {
            return;
        };
        let request = uuid::Uuid::new_v4();
        self.dashboards.route = Some(self.tx.route);
        self.dashboards.pending = Some(request);
        self.dashboards.error = None;
        self.dashboards.next = Some(Instant::now() + Duration::from_secs(u64::from(interval)));
        self.busy = true;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = async {
                anyhow::ensure!(
                    !db.in_transaction().await?,
                    "Server transaction active; dashboard refresh paused"
                );
                rowboat_admin::metrics::sample_dashboard(&db).await
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Dashboard(Box::new(Sampled {
                request,
                result,
                transaction,
            })));
        });
        cx.notify();
    }
    pub fn receive_dashboard(&mut self, event: Sampled, cx: &mut Context<Self>) {
        if self.dashboards.pending != Some(event.request) {
            return;
        }
        self.dashboards.pending = None;
        self.busy = false;
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
        match event.result {
            Err(error) => {
                self.dashboards.error = Some(error);
                self.dashboards.enabled = false;
            }
            Ok(sample) => {
                let delta = self
                    .dashboards
                    .sample
                    .as_ref()
                    .map(|old| rowboat_admin::metrics::dashboard_delta(old, &sample));
                let timestamp = sample.sample.sampled_at_unix_ms;
                for metric in &sample.sample.metrics {
                    let series = self
                        .dashboards
                        .series
                        .entry(metric.name.clone())
                        .or_default();
                    if metric.kind == MetricKind::Gauge {
                        append(series, timestamp, metric.value as f64);
                    } else if let Some((d, interval)) = delta.as_ref().and_then(|d| {
                        Some((
                            d.metrics.iter().find(|m| m.name == metric.name)?,
                            d.elapsed_ms?,
                        ))
                    }) && d.state == DeltaState::Counter
                        && let Some(value) = d.delta
                    {
                        append(series, timestamp, value as f64 * 1000. / interval as f64);
                    } else {
                        series.clear();
                    }
                }
                self.dashboards
                    .series
                    .retain(|name, _| sample.sample.metrics.iter().any(|m| &m.name == name));
                self.dashboards.sample = Some(sample);
                self.dashboards.delta = delta;
            }
        }
        cx.notify();
    }
    fn edit_dashboard(&mut self, new: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.has_extension_dialog() || self.close_requested {
            return;
        }
        let Some(connection) = self.owner.clone() else {
            return;
        };
        let original = if new {
            None
        } else {
            self.selected_dashboard().cloned()
        };
        self.dashboards.editor = Some(DashboardEditor {
            connection,
            name: forms::input(
                original
                    .as_ref()
                    .map(|d| d.name.as_str())
                    .unwrap_or("Server overview"),
                "Dashboard name",
                window,
                cx,
            ),
            interval: forms::input(
                original
                    .as_ref()
                    .map(|d| d.refresh_interval_secs)
                    .unwrap_or(5)
                    .to_string(),
                "Seconds (2–3600)",
                window,
                cx,
            ),
            columns: original.as_ref().map(|d| d.columns).unwrap_or(2),
            metrics: original
                .as_ref()
                .map(|d| d.cards.iter().map(|c| c.metric.clone()).collect())
                .unwrap_or_default(),
            original,
        });
        self.error = None;
        cx.notify();
    }
    fn save_dashboard(&mut self, cx: &mut Context<Self>) {
        let result = (|| {
            let editor = self
                .dashboards
                .editor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No dashboard draft"))?;
            anyhow::ensure!(
                Some(&editor.connection) == self.owner.as_ref(),
                "Dashboard connection changed"
            );
            let definition = Dashboard {
                id: editor
                    .original
                    .as_ref()
                    .map(|d| d.id.clone())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: editor.name.read(cx).value().trim().into(),
                connection_id: editor.connection.clone(),
                refresh_interval_secs: editor.interval.read(cx).value().trim().parse()?,
                columns: editor.columns,
                cards: editor
                    .metrics
                    .iter()
                    .enumerate()
                    .map(|(index, metric)| DashboardCard {
                        id: uuid::Uuid::new_v4().to_string(),
                        title: metric.replace('_', " "),
                        metric: metric.clone(),
                        column: (index % usize::from(editor.columns)) as u8,
                        row: (index / usize::from(editor.columns)) as u16,
                        column_span: 1,
                    })
                    .collect(),
            };
            let shared = self.shared.borrow();
            anyhow::ensure!(shared.writable, "App data is not writable");
            if let Some(original) = &editor.original {
                anyhow::ensure!(
                    shared
                        .state
                        .dashboards
                        .iter()
                        .find(|d| d.id == original.id)
                        .map(serde_json::to_value)
                        .transpose()?
                        == Some(serde_json::to_value(original)?),
                    "Dashboard changed elsewhere; reopen the editor"
                );
            }
            let mut state = shared.state.clone();
            state.dashboards.retain(|d| d.id != definition.id);
            state.dashboards.push(definition.clone());
            rowboat_services::validate_app_state(&state)?;
            Ok::<_, anyhow::Error>(definition)
        })();
        match result {
            Ok(definition) => {
                self.dashboards.selected = Some(definition.id.clone());
                self.edit_state(|state| {
                    state.dashboards.retain(|d| d.id != definition.id);
                    state.dashboards.push(definition);
                });
                self.dashboards.editor = None;
                self.dashboards.next = None;
            }
            Err(e) => self.fail(format!("Dashboard: {e:#}")),
        }
        cx.notify();
    }
    pub fn render_dashboard_editor(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self.dashboards.editor.as_ref().unwrap();
        let theme = cx.omarchy().clone();
        let engine = self
            .profile
            .as_ref()
            .map(|p| p.engine)
            .unwrap_or(rowboat_core::Engine::Postgres);
        let mut body=column().p_4().gap_3().text_size(px(13.)).child(row().child(ui::icon(ui::IconName::ChartLine).text_color(theme.accent)).child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Saved dashboard")).child(div().flex_1()).child(action("close-dashboard-editor","Cancel",false,true,cx,|s,_,cx|{s.dashboards.editor=None;cx.notify();})))
            .child(label("Choose the metrics and refresh cadence for this connection.", cx))
            .child(div().h(px(1.)).bg(theme.divider()))
            .child(column().gap_1().child(label("Dashboard name", cx)).child(ui::input("dashboard-name",&editor.name,window,cx)))
            .child(row().flex_wrap().child(column().gap_1().flex_1().min_w(px(180.)).child(label("Refresh interval · seconds",cx)).child(ui::input("dashboard-refresh",&editor.interval,window,cx)))
                .child(action("dashboard-columns",format!("{} layout columns",editor.columns),false,true,cx,|s,_,cx|{if let Some(e)=&mut s.dashboards.editor{e.columns=e.columns%4+1;}cx.notify();})))
            .child(div().pt_3().font_weight(FontWeight::SEMIBOLD).child("Metric cards"))
            .child(label("Counters display rates after two compatible samples. Samples stay in memory, up to 120 points per metric.",cx));
        let mut metrics = row().flex_wrap();
        for name in rowboat_services::builtin_metrics(engine) {
            let metric = name.to_string();
            metrics = metrics.child(
                action(
                    SharedString::from(format!("dashboard-metric-{name}")),
                    name.replace('_', " "),
                    false,
                    true,
                    cx,
                    move |s, _, cx| {
                        if let Some(e) = &mut s.dashboards.editor
                            && !e.metrics.remove(&metric)
                        {
                            e.metrics.insert(metric.clone());
                        }
                        cx.notify();
                    },
                )
                .selected(editor.metrics.contains(*name))
                .styles(|s| s.selected(|s| s.text_color(theme.accent).border_color(theme.accent))),
            );
        }
        for definition in &self.shared.borrow().state.custom_metrics {
            if definition.connection_id != editor.connection {
                continue;
            }
            let key = definition.metric_key();
            let chosen = editor.metrics.contains(&key);
            metrics = metrics.child(
                action(
                    format!("dashboard-custom-{}", definition.id),
                    format!("{} · custom / manual", definition.name),
                    false,
                    true,
                    cx,
                    move |s, _, cx| {
                        if let Some(e) = &mut s.dashboards.editor
                            && !e.metrics.remove(&key)
                        {
                            e.metrics.insert(key.clone());
                        }
                        cx.notify();
                    },
                )
                .selected(chosen)
                .styles(|s| s.selected(|s| s.text_color(theme.accent).border_color(theme.accent))),
            );
        }
        body = body.child(metrics).child(
            row()
                .pt_3()
                .border_t_1()
                .border_color(theme.divider())
                .child(div().flex_1())
                .child(action(
                    "save-dashboard",
                    "Save dashboard",
                    true,
                    true,
                    cx,
                    |s, _, cx| s.save_dashboard(cx),
                )),
        );
        if let Some(error) = &self.error {
            body = body.child(label(error.clone(), cx));
        }
        self.modal(
            div()
                .id("dashboard-editor-scroll")
                .w(px(800.).min(window.viewport_size().width - px(48.)))
                .max_h(window.viewport_size().height - px(65.))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
    pub fn render_dashboard(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let mut body = column()
            .p_4()
            .gap_4()
            .text_size(px(13.))
            .child(
                row()
                    .flex_wrap()
                    .child(ui::icon(ui::IconName::ChartLine).text_color(theme.accent))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Metrics dashboards"),
                    )
                    .child(div().flex_1())
                    .child(action(
                        "new-dashboard",
                        "New dashboard…",
                        false,
                        !self.busy && self.owner.is_some(),
                        cx,
                        |s, w, cx| s.edit_dashboard(true, w, cx),
                    ))
                    .child(action(
                        "edit-dashboard",
                        "Edit cards…",
                        false,
                        !self.busy && self.selected_dashboard().is_some(),
                        cx,
                        |s, w, cx| s.edit_dashboard(false, w, cx),
                    )),
            )
            .child(label(
                "Connection-specific metrics with explicit sampling and optional refresh.",
                cx,
            ))
            .child(div().h(px(1.)).bg(theme.divider()));
        body = body.child(action(
            "new-custom-metric",
            "New custom SQL metric…",
            false,
            !self.busy && self.owner.is_some(),
            cx,
            |s, w, cx| s.edit_custom_metric(None, w, cx),
        ));
        let mut tabs = row().flex_wrap().p_2().bg(theme.surface);
        for dashboard in self
            .state
            .dashboards
            .iter()
            .filter(|d| Some(&d.connection_id) == self.owner.as_ref())
        {
            let id = dashboard.id.clone();
            tabs = tabs.child(
                action(
                    SharedString::from(format!("dashboard-{id}")),
                    dashboard.name.clone(),
                    false,
                    !self.busy,
                    cx,
                    move |s, _, cx| {
                        s.dashboards.selected = Some(id.clone());
                        s.dashboards.next = None;
                        cx.notify();
                    },
                )
                .selected(Some(&dashboard.id) == self.dashboards.selected.as_ref())
                .styles(|s| s.selected(|s| s.text_color(theme.accent).border_color(theme.accent))),
            );
        }
        body = body.child(tabs);
        if let Some(definition) = self.selected_dashboard() {
            body=body.child(row().flex_wrap().child(action("dashboard-auto",if self.dashboards.enabled{"Pause refresh"}else{"Start refresh"},false,true,cx,|s,_,cx|{s.dashboards.enabled = !s.dashboards.enabled;s.dashboards.next=None;cx.notify();}))
                .child(action("dashboard-sample","Sample now",false,self.dashboard_can_sample(),cx,|s,_,cx|s.sample_dashboard(cx)))
                .child(label(format!("{}s cadence · {}",definition.refresh_interval_secs,if self.dashboards.pending.is_some(){"Sampling; no overlapping queries"}else if !self.dashboards.enabled{"Paused"}else if !self.dashboard_can_sample(){"Paused by workspace / operation / transaction"}else{"Refresh active"}),cx)))
                .child(label("Auto-refresh pauses on other pages, parked workspaces, dialogs, pending edits and open/unknown transactions. An already-running sample finishes on its owning session.",cx));
            if let Some(error) = &self.dashboards.error {
                body = body.child(label(format!("Sampling stopped: {error}"), cx));
            }
            if let Some(delta) = &self.dashboards.delta {
                for warning in &delta.warnings {
                    body = body.child(label(warning.clone(), cx));
                }
            }
            let rows = definition
                .cards
                .iter()
                .map(|c| c.row)
                .collect::<BTreeSet<_>>();
            for r in rows {
                let mut cards = row().items_stretch().flex_wrap();
                let mut col = 0;
                while col < definition.columns {
                    if let Some(card) = definition
                        .cards
                        .iter()
                        .find(|c| c.row == r && c.column == col)
                    {
                        cards = cards.child(
                            self.render_metric_card(card, cx)
                                .flex_grow(f32::from(card.column_span))
                                .flex_basis(px(0.)),
                        );
                        col += card.column_span;
                    } else {
                        cards = cards.child(div().flex_1());
                        col += 1;
                    }
                }
                body = body.child(cards);
            }
        } else {
            body=body.child(label("Create a saved dashboard for this connection to choose metric cards and refresh cadence.",cx));
        }
        let definitions = self
            .shared
            .borrow()
            .state
            .custom_metrics
            .iter()
            .filter(|m| Some(&m.connection_id) == self.owner.as_ref())
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        if !definitions.is_empty() {
            body = body.child(label(
                "Custom definitions · explicit samples only (independent of builtin refresh)",
                cx,
            ));
        }
        for id in definitions {
            body = body.child(
                div()
                    .id(format!("definition-list-{id}"))
                    .child(self.render_custom_card(&id, cx)),
            );
        }
        div()
            .id("dashboards-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(body)
            .into_any_element()
    }
    fn render_metric_card(&self, card: &DashboardCard, cx: &mut Context<Self>) -> Div {
        if let Some(id) = card.metric.strip_prefix("custom:") {
            return self.render_custom_card(id, cx);
        }
        let theme = cx.omarchy().clone();
        let metric = self
            .dashboards
            .sample
            .as_ref()
            .and_then(|s| s.sample.metrics.iter().find(|m| m.name == card.metric));
        let delta = self.dashboards.delta.as_ref().and_then(|d| {
            Some((
                d.metrics.iter().find(|m| m.name == card.metric)?,
                d.elapsed_ms,
            ))
        });
        let value = match metric {
            None => "No sample".into(),
            Some(m) if m.kind == MetricKind::Gauge => format!("{} {}", m.value, m.unit),
            Some(m) => match delta {
                Some((d, Some(interval))) if d.state == DeltaState::Counter => d
                    .delta
                    .map(|n| format!("{} {}", rate_text(n, interval), m.unit))
                    .unwrap_or_else(|| "Rate unavailable".into()),
                Some((d, _)) => format!("{:?} · rate unavailable", d.state),
                None => "Awaiting second sample".into(),
            },
        };
        let series = self
            .dashboards
            .series
            .get(&card.metric)
            .cloned()
            .unwrap_or_default();
        let color = theme.accent;
        let points = series.len();
        column().min_w(px(220.)).p_4().gap_2().bg(theme.surface)
            .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(card.title.clone())).child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).child(value))
            .child(label(metric.map(|m|format!("Raw: {} · {:?}",m.value,m.kind)).unwrap_or_default(),cx))
            .child(canvas(move|_,_,_|{},move|bounds,_,window,_|{
                if series.len()<2{return;}let min=series.iter().map(|(_,v)|*v).fold(f64::INFINITY,f64::min);let max=series.iter().map(|(_,v)|*v).fold(f64::NEG_INFINITY,f64::max);
                let first=series.front().unwrap().0;let duration=series.back().unwrap().0.saturating_sub(first).max(1);
                let mut path=PathBuilder::stroke(px(2.));
                for (i,(time,value)) in series.iter().enumerate(){let x=(*time-first) as f64/duration as f64;let y=if max>min{(*value-min)/(max-min)}else{0.5};let p=point(bounds.left()+bounds.size.width*x as f32,bounds.bottom()-bounds.size.height*y as f32);if i==0{path.move_to(p);}else{path.line_to(p);}}
                if let Ok(path)=path.build(){window.paint_path(path,color);}
            }).w_full().h(px(85.)))
            .child(label(format!("{points}/120 points · time → · chart autoscaled; numeric labels are authoritative"),cx))
    }
}
#[cfg(test)]
mod tests {
    use super::{VecDeque, append, rate_text};
    #[test]
    fn rates_keep_u64_precision_and_chart_history_is_bounded_and_resets_on_nonincreasing_time() {
        assert_eq!(
            rate_text(9_007_199_254_740_993, 1000),
            "9007199254740993.000/s"
        );
        assert_eq!(rate_text(3, 2000), "1.500/s");
        let mut series = VecDeque::new();
        for timestamp in 0..500 {
            append(&mut series, timestamp, 1.);
        }
        assert_eq!(series.len(), 120);
        assert_eq!(series.front().unwrap().0, 380);
        append(&mut series, 10, 2.);
        assert_eq!(series.len(), 1);
    }
}
