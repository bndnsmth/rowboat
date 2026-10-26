//! Actual GPUI controls backed by disposable SQLite schema metadata.
use crate::{app::Workbench, workbench_tests::workspace};
use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext, px, size,
};
use rowboat_core::*;
use std::time::{Duration, Instant};

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}
fn click(cx: &mut TestAppContext, window: AnyWindowHandle, name: &'static str) {
    draw(cx, window);
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual
        .debug_bounds(name)
        .unwrap_or_else(|| panic!("Missing {name}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
    draw(cx, window);
}
fn drain(cx: &mut TestAppContext, entity: &Entity<Workbench>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.update(|cx| entity.update(cx, |s, cx| s.poll_diagram(cx)));
        if cx.read(|cx| !entity.read(cx).diagram.is_loading()) {
            break;
        }
        assert!(Instant::now() < deadline, "diagram metadata did not drain");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn setup(
    cx: &mut TestAppContext,
    id: u64,
) -> (
    crate::app_runtime::AppRuntime,
    tempfile::TempDir,
    AnyWindowHandle,
    Entity<Workbench>,
) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let profile = ConnectionProfile {
        id: format!("profile-{id}"),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: dir.path().join("diagram.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = rt
        .handle()
        .block_on(Database::connect(&profile, ""))
        .unwrap();
    rt.handle().block_on(db.query("CREATE TABLE pass22_parent(tenant INTEGER,id INTEGER,PRIMARY KEY(tenant,id)); CREATE TABLE pass22_child(id INTEGER PRIMARY KEY,tenant INTEGER,parent INTEGER,FOREIGN KEY(tenant,parent) REFERENCES pass22_parent(tenant,id));",1)).unwrap();
    let catalog = rt.handle().block_on(db.catalog()).unwrap();
    let (window, entity) = workspace(cx, id, rt.handle());
    cx.simulate_window_resize(window, size(px(960.), px(640.)));
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.profile = Some(profile);
            s.database = Some(db);
            s.catalog = catalog;
            s.busy = false;
            s.session_ready = true;
            s.table = Some(TableRef {
                schema: "main".into(),
                name: "pass22_child".into(),
            });
            s.show_diagram(w, cx);
        })
    })
    .unwrap();
    (rt, dir, window, entity)
}
#[gpui_kit::test]
fn pass22_diagram_actual_metadata_canvas_zoom_keyboard_and_close_at_960(cx: &mut TestAppContext) {
    let (_rt, _dir, window, entity) = setup(cx, 2241);
    drain(cx, &entity);
    draw(cx, window);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.diagram.open);
        assert!(!s.busy);
        assert_eq!(s.diagram.graph.nodes.len(), 2);
        assert_eq!(s.diagram.graph.edges[0].columns.len(), 2);
        assert_eq!(s.query_run_invocations, 0);
        assert!(s.table_result.rows.is_empty());
        assert!(s.has_extension_dialog());
    });
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual.debug_bounds("diagram-canvas").unwrap();
    assert!(
        bounds.left() >= px(0.) && bounds.right() <= px(960.) && bounds.bottom() <= px(640.),
        "{bounds:?}"
    );
    click(cx, window, "diagram-plus");
    cx.read(|cx| assert!(entity.read(cx).diagram.camera.zoom > 1.));
    // Click the lower blank strip of the canvas, then exercise its key handler.
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.simulate_click(
        gpui_kit::point(bounds.left() + px(3.), bounds.bottom() - px(3.)),
        Modifiers::default(),
    );
    cx.simulate_keystrokes(window, "right");
    cx.read(|cx| assert!(entity.read(cx).diagram.camera.x < 0.));
    cx.simulate_keystrokes(window, "home");
    cx.read(|cx| assert_eq!(entity.read(cx).diagram.camera.zoom, 1.));
    click(cx, window, "diagram-find");
    draw(cx, window);
    cx.simulate_keystrokes(window, "escape");
    cx.read(|cx| assert!(!entity.read(cx).diagram.open));
}
#[gpui_kit::test]
fn pass22_diagram_escape_from_canvas_routes_to_overlay(cx: &mut TestAppContext) {
    let (_rt, _dir, window, entity) = setup(cx, 2249);
    drain(cx, &entity);
    draw(cx, window);
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual.debug_bounds("diagram-canvas").unwrap();
    visual.simulate_click(
        gpui_kit::point(bounds.left() + px(3.), bounds.bottom() - px(3.)),
        Modifiers::default(),
    );
    cx.simulate_keystrokes(window, "escape");
    cx.read(|cx| {
        assert!(!entity.read(cx).diagram.open);
        assert!(!entity.read(cx).busy);
    });
}

#[gpui_kit::test]
fn pass22_diagram_dismiss_inflight_drains_without_merging(cx: &mut TestAppContext) {
    let (_rt, _dir, window, entity) = setup(cx, 2242);
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            assert!(s.busy);
            assert!(s.diagram.is_loading());
            assert!(s.dismiss_diagram(w, cx));
            assert!(s.busy, "Close must not lend out a live metadata read");
        })
    })
    .unwrap();
    drain(cx, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.busy);
        assert!(!s.diagram.open);
        assert!(s.diagram.graph.edges.is_empty());
        assert_eq!(s.query_run_invocations, 0);
    });
}
#[gpui_kit::test]
fn pass22_diagram_reconnect_rejects_late_result_and_preserves_new_busy(cx: &mut TestAppContext) {
    let (_rt, _dir, _window, entity) = setup(cx, 2243);
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.session_generation += 1;
            s.tx.route.generation = s.session_generation;
            s.busy = true;
        })
    });
    drain(cx, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.diagram.open);
        assert!(s.busy);
        assert!(s.diagram.graph.edges.is_empty());
    });
}

#[gpui_kit::test]
fn pass22_diagram_search_expand_and_structure_navigation(cx: &mut TestAppContext) {
    let (_rt, _dir, window, entity) = setup(cx, 2244);
    drain(cx, &entity);
    click(cx, window, "diagram-search");
    cx.simulate_input(window, "pass22_parent");
    click(cx, window, "diagram-find");
    click(cx, window, "diagram-add-0");
    drain(cx, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(
            s.diagram
                .graph
                .nodes
                .iter()
                .find(|n| n.identity.name == "pass22_parent")
                .unwrap()
                .inspected
        );
        assert_eq!(
            s.diagram.graph.edges.len(),
            1,
            "Repeated incoming/outgoing discovery deduplicates a composite key"
        );
        assert_eq!(s.query_run_invocations, 0);
        assert!(s.table_result.rows.is_empty());
    });
    click(cx, window, "diagram-structure");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.diagram.open);
        assert_eq!(s.table.as_ref().unwrap().name, "pass22_parent");
        assert!(s.table_page == crate::app::TablePage::Structure);
    });
}

#[gpui_kit::test]
fn pass22_diagram_stale_catalog_failure_is_rendered_and_recoverable(cx: &mut TestAppContext) {
    let (_rt, _dir, window, entity) = setup(cx, 2245);
    drain(cx, &entity);
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.catalog.push(TableInfo {
                table: TableRef {
                    schema: "main".into(),
                    name: "pass22_dropped".into(),
                },
                kind: "TABLE".into(),
            });
        })
    });
    click(cx, window, "diagram-search");
    cx.simulate_input(window, "pass22_dropped");
    click(cx, window, "diagram-find");
    click(cx, window, "diagram-add-0");
    drain(cx, &entity);
    draw(cx, window);
    let mut visual = VisualTestContext::from_window(window, cx);
    assert!(visual.debug_bounds("diagram-error").is_some());
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.diagram.open);
        assert!(!s.busy);
        assert!(s.database.is_some());
    });
    click(cx, window, "diagram-close");
}

#[gpui_kit::test]
fn pass22_diagram_host_change_discards_result_but_drains_its_session(cx: &mut TestAppContext) {
    let (_rt, _dir, _window, entity) = setup(cx, 2246);
    cx.update(|cx| entity.update(cx, |s, _| s.host_epoch += 1));
    drain(cx, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.diagram.open);
        assert!(!s.busy);
        assert!(s.diagram.graph.edges.is_empty());
    });
}

#[gpui_kit::test]
fn pass22_diagram_workspace_change_never_releases_another_workspace(cx: &mut TestAppContext) {
    let (_rt, _dir, _window, entity) = setup(cx, 2247);
    cx.update(|cx| entity.update(cx, |s, _| s.workspace_id += 1));
    drain(cx, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.diagram.open);
        assert!(s.busy);
        assert!(s.diagram.graph.edges.is_empty());
    });
}

#[gpui_kit::test]
fn pass22_diagram_data_navigation_uses_the_selected_table(cx: &mut TestAppContext) {
    let (_rt, _dir, window, entity) = setup(cx, 2248);
    drain(cx, &entity);
    click(cx, window, "diagram-data");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.diagram.open);
        assert_eq!(s.table.as_ref().unwrap().name, "pass22_child");
        assert!(s.table_page == crate::app::TablePage::Content);
        assert!(s.page == crate::app::Page::Table);
        assert_eq!(s.query_run_invocations, 0);
    });
}
