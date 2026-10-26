//! Read-only, schema-only FK neighborhood. Parent integration:
//! `mod diagram_model; mod diagram_ui;` + `diagram: DiagramState::default()`;
//! tick `poll_diagram(cx)`, action `show_diagram(window,cx)`, overlay
//! `render_diagram(window,cx)` when open, Escape `dismiss_diagram(window,cx)`.
//! Include `diagram.open || diagram.is_loading()` in extension/modal guards:
//! a dismissed in-flight read still owns the busy lease. No shared Event variants.
use crate::{
    app::{TablePage, Workbench},
    diagram_model::{CARD_HEIGHT, CARD_WIDTH, Camera, Graph, Identity, MAX_NODES},
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::{
    TableRef,
    database::{DiagramCancellation, DiagramMetadata, DiagramReadError},
};
use std::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Owner {
    workspace: u64,
    route: Route,
    session: u64,
    host: u64,
    profile: Option<String>,
    table: Option<TableRef>,
}
struct Pending {
    request: uuid::Uuid,
    owner: Owner,
    cancel: DiagramCancellation,
    receiver: mpsc::Receiver<Loaded>,
}
struct Loaded {
    request: uuid::Uuid,
    result: Result<DiagramMetadata, DiagramReadError>,
}
#[derive(Default)]
pub struct DiagramState {
    pub open: bool,
    pub(crate) graph: Graph,
    pub(crate) camera: Camera,
    selected: usize,
    owner: Option<Owner>,
    pending: Option<Pending>,
    search: Option<Entity<InputState>>,
    canvas_focus: Option<FocusHandle>,
    matches: Vec<TableRef>,
    searched: bool,
    warnings: Vec<String>,
    error: Option<String>,
    drag: Option<Point<Pixels>>,
}
impl Drop for DiagramState {
    fn drop(&mut self) {
        if let Some(p) = &self.pending {
            p.cancel.cancel();
        }
    }
}
impl DiagramState {
    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }
}
impl Workbench {
    fn diagram_owner(&self) -> Owner {
        Owner {
            workspace: self.workspace_id,
            route: self.tx.route,
            session: self.session_generation,
            host: self.host_epoch,
            profile: self.profile.as_ref().map(|p| p.id.clone()),
            table: self.table.clone(),
        }
    }
    pub fn show_diagram(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.diagram.open || self.diagram.pending.is_some() || !self.can_leave() {
            cx.notify();
            return;
        }
        let Some(table) = self.table.clone().filter(|_| self.database.is_some()) else {
            self.fail("Select a connected table to explore its FK neighborhood.".into());
            cx.notify();
            return;
        };
        self.diagram = DiagramState::default();
        self.diagram.open = true;
        self.diagram.owner = Some(self.diagram_owner());
        self.diagram.search =
            Some(cx.new(|cx| {
                InputState::new(window, cx).placeholder("Find table in loaded catalog…")
            }));
        self.diagram.canvas_focus = Some(cx.focus_handle());
        self.diagram.graph.ensure_node(&table);
        self.quick_open = false;
        self.dismiss_completions(cx);
        self.tabs[self.active_tab].show_parameters = false;
        window.focus(self.diagram.canvas_focus.as_ref().unwrap(), cx);
        self.load_diagram_table(table, cx);
    }
    fn load_diagram_table(&mut self, table: TableRef, cx: &mut Context<Self>) {
        if !self.diagram.open
            || self.diagram.pending.is_some()
            || self.busy
            || self.diagram.owner.as_ref() != Some(&self.diagram_owner())
        {
            return;
        }
        let Some(database) = self.database.clone() else {
            return;
        };
        let Some(index) = self.diagram.graph.ensure_node(&table) else {
            self.diagram.error = Some(format!(
                "Neighborhood is limited to {MAX_NODES} tables. Close and reopen from another table."
            ));
            cx.notify();
            return;
        };
        self.diagram.selected = index;
        self.diagram
            .camera
            .center_node(&self.diagram.graph.nodes[index]);
        self.diagram.error = None;
        // Each table is inspected once per open diagram. Reopen to refresh;
        // do not mix refreshed incident edges with possibly deleted old keys.
        if self.diagram.graph.nodes[index].inspected {
            cx.notify();
            return;
        }
        let request = uuid::Uuid::new_v4();
        let cancel = DiagramCancellation::default();
        let (sender, receiver) = mpsc::channel();
        self.diagram.pending = Some(Pending {
            request,
            owner: self.diagram_owner(),
            cancel: cancel.clone(),
            receiver,
        });
        self.busy = true;
        self.runtime.spawn(async move {
            let result = database.diagram_metadata(&table, &cancel).await;
            let _ = sender.send(Loaded { request, result });
        });
        cx.notify();
    }
    /// Always poll, even after dismissal: the owning metadata read must drain.
    pub fn poll_diagram(&mut self, cx: &mut Context<Self>) {
        let owner = self.diagram_owner();
        if self.diagram.open && self.diagram.owner.as_ref() != Some(&owner) {
            self.diagram.open = false;
            if let Some(p) = &self.diagram.pending {
                p.cancel.cancel();
            }
            cx.notify();
        }
        let Some(pending) = &self.diagram.pending else {
            return;
        };
        let loaded = match pending.receiver.try_recv() {
            Ok(loaded) => loaded,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Loaded {
                request: pending.request,
                result: Err(DiagramReadError {
                    message:
                        "Diagram metadata worker ended unexpectedly; reconnect before continuing."
                            .into(),
                    session_retired: true,
                    cancelled: false,
                }),
            },
        };
        let pending = self.diagram.pending.take().unwrap();
        // Host changes invalidate rendering, but a retired session must still be
        // retired by its new host. Never clear busy or retire another connection.
        let same_session = pending.owner.workspace == owner.workspace
            && pending.owner.route == owner.route
            && pending.owner.session == owner.session
            && pending.owner.profile == owner.profile;
        if same_session {
            self.busy = false;
        }
        let accepted = self.diagram.open
            && pending.owner == owner
            && loaded.request == pending.request
            && !pending.cancel.is_cancelled();
        match loaded.result {
            Err(error) if error.session_retired && same_session => {
                self.retire_unknown_connection(cx);
                self.diagram.open = false;
                self.fail(error.message);
            }
            Ok(metadata) if accepted => {
                self.diagram
                    .graph
                    .merge(&metadata.table, metadata.columns, metadata.relationships);
                for warning in metadata.warnings {
                    if self.diagram.warnings.len() < 16 && !self.diagram.warnings.contains(&warning)
                    {
                        self.diagram.warnings.push(warning);
                    }
                }
            }
            Err(error) if accepted => self.diagram.error = Some(error.message),
            _ if self.diagram.open && pending.cancel.is_cancelled() => {
                self.diagram.error = Some("Metadata read cancelled. Previously inspected metadata remains; Expand retries the selected table.".into());
            }
            _ => {}
        }
        cx.notify();
    }
    pub fn dismiss_diagram(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.diagram.open {
            return false;
        }
        self.diagram.open = false;
        self.diagram.drag = None;
        if let Some(p) = &self.diagram.pending {
            p.cancel.cancel();
            self.status = "Closing diagram · waiting for metadata read to drain…".into();
        }
        window.focus(&self.focus, cx);
        cx.notify();
        true
    }
    fn find_diagram_tables(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.diagram.search else {
            return;
        };
        let query = search.read(cx).value().to_lowercase();
        let mut tables = std::collections::BTreeSet::new();
        for table in self.catalog.iter().map(|t| &t.table) {
            tables.insert(Identity::from(table));
        }
        tables.extend(self.diagram.graph.nodes.iter().map(|n| n.identity.clone()));
        self.diagram.matches = tables
            .into_iter()
            .filter(|t| t.label().to_lowercase().contains(query.trim()))
            .take(12)
            .map(|t| t.table())
            .collect();
        self.diagram.searched = true;
        cx.notify();
    }
    fn diagram_select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.diagram.graph.nodes.len() {
            self.diagram.selected = index;
            self.diagram
                .camera
                .center_node(&self.diagram.graph.nodes[index]);
            cx.notify();
        }
    }
    fn diagram_navigate(&mut self, structure: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy
            || self.diagram.pending.is_some()
            || self.diagram.owner.as_ref() != Some(&self.diagram_owner())
        {
            return;
        }
        let Some(table) = self
            .diagram
            .graph
            .nodes
            .get(self.diagram.selected)
            .map(|n| n.identity.table())
        else {
            return;
        };
        self.dismiss_diagram(window, cx);
        self.open_table(table.clone(), cx);
        if structure && self.table.as_ref() == Some(&table) {
            self.table_page = TablePage::Structure;
        }
        cx.notify();
    }
    pub fn render_diagram(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let pending = self.diagram.pending.is_some();
        let mut body = column()
            .w(px(1100.))
            .max_w_full()
            .p_3()
            .gap_2()
            .child(
                row()
                    .child(div().text_size(px(19.)).child("Relationship neighborhood"))
                    .child(div().flex_1())
                    .child(
                        action("diagram-close", "Close", false, true, cx, |s, w, cx| {
                            s.dismiss_diagram(w, cx);
                        })
                        .debug_selector(|| "diagram-close".into()),
                    ),
            )
            .child(label(
                "Retained metadata · reopen to refresh · FK source → referenced key · max 24 tables / 64 keys",
                cx,
            ));
        if let Some(search) = &self.diagram.search {
            body = body.child(
                row()
                    .child(
                        div().flex_1().child(
                            ui::input("diagram-search", search, window, cx)
                                .debug_selector(|| "diagram-search".into()),
                        ),
                    )
                    .child(
                        action("diagram-find", "Find", false, true, cx, |s, _, cx| {
                            s.find_diagram_tables(cx)
                        })
                        .debug_selector(|| "diagram-find".into()),
                    ),
            );
        }
        if self.diagram.searched {
            let mut matches = column().gap_1().child(label(
                "First 12 matches in the already-loaded catalog and diagram (no full-schema fetch)",
                cx,
            ));
            for (i, table) in self.diagram.matches.iter().enumerate() {
                let table = table.clone();
                matches = matches.child(
                    action(
                        ("diagram-add", i),
                        Identity::from(&table).label(),
                        false,
                        !pending,
                        cx,
                        move |s, _, cx| {
                            s.diagram.searched = false;
                            s.load_diagram_table(table.clone(), cx);
                        },
                    )
                    .debug_selector(move || format!("diagram-add-{i}")),
                );
            }
            if self.diagram.matches.is_empty() {
                matches = matches.child("No matching loaded tables.");
            }
            body = body.child(
                div()
                    .id("diagram-matches")
                    .max_h(px(110.))
                    .overflow_y_scroll()
                    .child(matches),
            );
        }
        body = body.child(
            row()
                .flex_wrap()
                .child(action(
                    "diagram-previous",
                    "Previous",
                    false,
                    self.diagram.selected > 0,
                    cx,
                    |s, _, cx| s.diagram_select(s.diagram.selected.saturating_sub(1), cx),
                ))
                .child(action(
                    "diagram-next",
                    "Next",
                    false,
                    self.diagram.selected + 1 < self.diagram.graph.nodes.len(),
                    cx,
                    |s, _, cx| s.diagram_select(s.diagram.selected + 1, cx),
                ))
                .child(action(
                    "diagram-minus",
                    "−",
                    false,
                    true,
                    cx,
                    |s, _, cx| {
                        s.diagram.camera.zoom_by(1. / 1.2);
                        cx.notify();
                    },
                ))
                .child(label(
                    format!("{}%", (self.diagram.camera.zoom * 100.).round()),
                    cx,
                ))
                .child(
                    action("diagram-plus", "+", false, true, cx, |s, _, cx| {
                        s.diagram.camera.zoom_by(1.2);
                        cx.notify();
                    })
                    .debug_selector(|| "diagram-plus".into()),
                )
                .child(action(
                    "diagram-reset",
                    "Reset",
                    false,
                    true,
                    cx,
                    |s, _, cx| {
                        s.diagram.camera = Camera::default();
                        cx.notify();
                    },
                ))
                .child(label(
                    "Drag / scroll to pan · focus canvas: arrows, +/−, Home",
                    cx,
                )),
        );
        if let Some(error) = &self.diagram.error {
            body = body.child(
                div()
                    .debug_selector(|| "diagram-error".into())
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if self.diagram.graph.limited || !self.diagram.warnings.is_empty() {
            body = body.child(label(
                "Partial metadata / neighborhood · see limits below the canvas.",
                cx,
            ));
        }
        body = body.child(self.diagram_canvas(cx));
        if let Some(node) = self.diagram.graph.nodes.get(self.diagram.selected) {
            let table = node.identity.table();
            body = body.child(
                row()
                    .flex_wrap()
                    .child(div().flex_1().text_ellipsis().child(node.identity.label()))
                    .child(
                        action(
                            "diagram-expand",
                            if node.inspected {
                                "Expanded"
                            } else {
                                "Expand / retry"
                            },
                            false,
                            !pending && !self.busy && !node.inspected,
                            cx,
                            move |s, _, cx| s.load_diagram_table(table.clone(), cx),
                        )
                        .debug_selector(|| "diagram-expand".into()),
                    )
                    .child(
                        action(
                            "diagram-data",
                            "Data",
                            false,
                            !pending && !self.busy,
                            cx,
                            |s, w, cx| s.diagram_navigate(false, w, cx),
                        )
                        .debug_selector(|| "diagram-data".into()),
                    )
                    .child(
                        action(
                            "diagram-structure",
                            "Structure",
                            false,
                            !pending && !self.busy,
                            cx,
                            |s, w, cx| s.diagram_navigate(true, w, cx),
                        )
                        .debug_selector(|| "diagram-structure".into()),
                    ),
            );
            let table = node.identity.table();
            let mut details = column().gap_1();
            if !node.inspected {
                details = details.child(
                    "Placeholder from FK metadata · Expand reads its columns and neighborhood.",
                );
            }
            for edge in self
                .diagram
                .graph
                .edges
                .iter()
                .filter(|e| e.source == table || e.referenced == table)
            {
                details = details.child(format!(
                    "{}: {} → {} · {}",
                    edge.name,
                    Identity::from(&edge.source).label(),
                    Identity::from(&edge.referenced).label(),
                    edge.columns
                        .iter()
                        .map(|c| format!("{} → {}", c.source, c.referenced))
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
            if node.inspected && self.diagram.graph.edges.is_empty() {
                details = details.child(
                    "No declared foreign keys were reported within this metadata read's bounds.",
                );
            }
            let columns = self.diagram.graph.key_columns(self.diagram.selected);
            for (name, ty, pk, fk) in columns.iter().take(256) {
                details = details.child(format!(
                    "{}{} {name} · {ty}",
                    if *pk { "PK " } else { "" },
                    if *fk { "FK" } else { "" }
                ));
            }
            if columns.len() > 256 {
                details = details.child(
                    "Column list limited to 256 names; declared key mappings are shown above.",
                );
            }
            body = body.child(
                div()
                    .id("diagram-key-details")
                    .max_h(px(76.))
                    .overflow_y_scroll()
                    .text_size(px(12.))
                    .child(details),
            );
        }
        if pending {
            body = body.child(
                row()
                    .child("Reading metadata… cancellation drains the current read (20s deadline).")
                    .child(action(
                        "diagram-cancel",
                        "Cancel read",
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            if let Some(p) = &s.diagram.pending {
                                p.cancel.cancel();
                            }
                            cx.notify();
                        },
                    )),
            );
        }
        if self.diagram.graph.limited {
            body = body.child("Partial neighborhood: 24-table / 64-key bound reached. Reopen from a different table to explore further.");
        }
        for warning in &self.diagram.warnings {
            body = body.child(label(warning.clone(), cx));
        }
        self.modal(body.into_any_element(), cx)
    }
    fn diagram_canvas(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.omarchy().clone();
        let camera = self.diagram.camera;
        let mut viewport = div()
            .id("diagram-canvas")
            .debug_selector(|| "diagram-canvas".into())
            .relative()
            .h(px(250.))
            .w_full()
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .tab_index(0)
            .when_some(self.diagram.canvas_focus.as_ref(), |d, f| d.track_focus(f))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|s, e: &MouseDownEvent, w, cx| {
                    s.diagram.drag = Some(e.position);
                    if let Some(f) = &s.diagram.canvas_focus {
                        w.focus(f, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|s, e: &MouseMoveEvent, _, cx| {
                if let Some(last) = s.diagram.drag {
                    if e.dragging() {
                        s.diagram.camera.pan(
                            f32::from(e.position.x - last.x),
                            f32::from(e.position.y - last.y),
                        );
                        s.diagram.drag = Some(e.position);
                        cx.notify();
                    } else {
                        s.diagram.drag = None;
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|s, _, _, _| s.diagram.drag = None),
            )
            .on_scroll_wheel(cx.listener(|s, e: &ScrollWheelEvent, _, cx| {
                let delta = e.delta.pixel_delta(px(24.));
                s.diagram.camera.pan(f32::from(delta.x), f32::from(delta.y));
                cx.stop_propagation();
                cx.notify();
            }))
            .on_key_down(cx.listener(|s, e: &KeyDownEvent, _, cx| {
                match e.keystroke.key.as_str() {
                    "left" => s.diagram.camera.pan(60., 0.),
                    "right" => s.diagram.camera.pan(-60., 0.),
                    "up" => s.diagram.camera.pan(0., 60.),
                    "down" => s.diagram.camera.pan(0., -60.),
                    "+" | "=" => s.diagram.camera.zoom_by(1.2),
                    "-" => s.diagram.camera.zoom_by(1. / 1.2),
                    "home" => s.diagram.camera = Camera::default(),
                    _ => return,
                }
                cx.stop_propagation();
                cx.notify();
            }));
        let mut lines = Vec::new();
        for (i, edge) in self.diagram.graph.edges.iter().enumerate() {
            let Some(source) = self
                .diagram
                .graph
                .nodes
                .iter()
                .find(|n| n.identity == Identity::from(&edge.source))
            else {
                continue;
            };
            let Some(target) = self
                .diagram
                .graph
                .nodes
                .iter()
                .find(|n| n.identity == Identity::from(&edge.referenced))
            else {
                continue;
            };
            let lane = 12. + (i % 5) as f32 * 7.;
            let from = (source.x + CARD_WIDTH, source.y + 70. + (i % 6) as f32 * 15.);
            let to = (target.x, target.y + 70. + (i % 6) as f32 * 15.);
            let points = if source.identity == target.identity {
                vec![
                    from,
                    (from.0 + lane, from.1),
                    (from.0 + lane, source.y - 12.),
                    (source.x - 12., source.y - 12.),
                    (source.x - 12., to.1),
                    to,
                ]
            } else {
                vec![from, (from.0 + lane, from.1), (from.0 + lane, to.1), to]
            };
            lines.push(points);
        }
        let color = theme.accent;
        viewport = viewport.child(
            canvas(
                |_, _, _| (),
                move |bounds, _, w, _| {
                    for line in &lines {
                        let p = |v: (f32, f32)| {
                            point(
                                bounds.left() + px(v.0 * camera.zoom + camera.x),
                                bounds.top() + px(v.1 * camera.zoom + camera.y),
                            )
                        };
                        let mut path = PathBuilder::stroke(px(1.5));
                        path.move_to(p(line[0]));
                        for &v in &line[1..] {
                            path.line_to(p(v));
                        }
                        let end = *line.last().unwrap();
                        path.move_to(p((end.0 - 7., end.1 - 5.)));
                        path.line_to(p(end));
                        path.line_to(p((end.0 - 7., end.1 + 5.)));
                        if let Ok(path) = path.build() {
                            w.paint_path(path, color);
                        }
                    }
                },
            )
            .absolute()
            .size_full(),
        );
        for (i, node) in self.diagram.graph.nodes.iter().enumerate() {
            let columns = self.diagram.graph.key_columns(i);
            let mut card = column()
                .absolute()
                .left(px(node.x * camera.zoom + camera.x))
                .top(px(node.y * camera.zoom + camera.y))
                .w(px(CARD_WIDTH * camera.zoom))
                .h(px(CARD_HEIGHT * camera.zoom))
                .p(px(8. * camera.zoom))
                .gap(px(4. * camera.zoom))
                .overflow_hidden()
                .bg(theme.surface)
                .border_1()
                .border_color(if i == self.diagram.selected {
                    theme.accent
                } else {
                    theme.border
                })
                .text_size(px((12. * camera.zoom).max(9.)))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_ellipsis()
                        .child(node.identity.label()),
                );
            if !node.inspected {
                card = card.child("FK endpoint · not expanded");
            }
            for (name, ty, pk, fk) in columns.iter().take(6) {
                card = card.child(div().text_ellipsis().child(format!(
                    "{}{} {} · {}",
                    if *pk { "PK " } else { "" },
                    if *fk { "FK" } else { "" },
                    name,
                    ty
                )));
            }
            if columns.len() > 6 {
                card = card.child(format!(
                    "+{} columns · key mappings below",
                    columns.len() - 6
                ));
            }
            viewport = viewport.child(card.id(("diagram-node", i)).cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |s, _, _, cx| {
                    s.diagram.selected = i;
                    cx.stop_propagation();
                    cx.notify();
                }),
            ));
        }
        viewport
    }
}
