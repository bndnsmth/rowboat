//! In-memory TestPlatform only: exercise the production EditorState mutation and undo.
use gpui_kit::{
    AnyWindowHandle, AppContext, Context, Entity, TestAppContext, Window, WindowOptions,
    base::input::EditorState, div, prelude::*,
};
use rowboat_core::{Engine, TableRef};
use rowboat_services::{CatalogTable, CompletionCatalog, SnippetTemplate};

struct EditorFixture(Entity<EditorState>);
impl Render for EditorFixture {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(crate::sql_editor::render(&self.0, 14., cx))
    }
}
fn fixture(cx: &mut TestAppContext, sql: &str) -> (AnyWindowHandle, Entity<EditorState>) {
    cx.update(crate::test_theme::init);
    cx.update(|cx| {
        let (window, content) =
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| EditorFixture(crate::sql_editor::create(sql.to_string(), window, cx)))
            })
            .unwrap();
        (window, content.read(cx).0.clone())
    })
}
fn assert_undo(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    editor: &Entity<EditorState>,
    original: &str,
) {
    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |e, cx| e.focus(window, cx));
        window.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(window, "ctrl-z");
    cx.read(|cx| assert_eq!(editor.read(cx).value().as_ref(), original));
}

#[gpui_kit::test]
fn actual_editor_completion_replaces_prefix_and_whole_quoted_utf8_token_and_undoes(
    cx: &mut TestAppContext,
) {
    let sql = "SELECT c.na FROM rowboat_native5.customers c";
    let (window, editor) = fixture(cx, sql);
    let catalog = CompletionCatalog::new(
        Engine::Postgres,
        vec![CatalogTable {
            table: TableRef {
                schema: "rowboat_native5".into(),
                name: "customers".into(),
            },
            columns: vec!["name".into(), "naïve".into()],
        }],
    )
    .unwrap();
    for (original, caret, wanted, expected) in [
        (
            sql,
            11,
            "\"name\"",
            "SELECT c.\"name\" FROM rowboat_native5.customers c",
        ),
        (
            "SELECT 'é', c.\"naï\" FROM rowboat_native5.customers c",
            "SELECT 'é', c.\"na".len(),
            "\"naïve\"",
            "SELECT 'é', c.\"naïve\" FROM rowboat_native5.customers c",
        ),
    ] {
        let found = rowboat_services::complete_sql(original, caret, &catalog, 20).unwrap();
        let candidate = found
            .items
            .iter()
            .find(|item| item.insert_text == wanted)
            .unwrap();
        cx.update_window(window, |_, window, cx| {
            editor.update(cx, |e, cx| {
                e.set_value(original.to_string(), window, cx);
                // Existing extra carets must not receive the completion intended for one identifier.
                e.set_selected_range(0..0, cx);
                e.add_selection(caret..caret, cx);
                crate::sql_editor::replace_range(
                    e,
                    found.replacement.clone(),
                    candidate.insert_text.clone(),
                    window,
                    cx,
                );
                assert_eq!(e.value().as_ref(), expected);
                assert_eq!(e.cursor(), found.replacement.start + wanted.len());
            })
        })
        .unwrap();
        assert_undo(cx, window, &editor, original);
    }
}

#[gpui_kit::test]
fn actual_editor_snippet_replaces_selected_text_preserves_suffix_and_undoes(
    cx: &mut TestAppContext,
) {
    let original = "SELECT é_REPLACE FROM customers; -- suffix";
    let (window, editor) = fixture(cx, original);
    let start = original.find('é').unwrap();
    let end = original.find(" FROM").unwrap();
    let expansion = rowboat_services::expand_snippet(
        &SnippetTemplate {
            id: "test".into(),
            name: "Linked".into(),
            trigger: "linked".into(),
            body: "${column:name} || ${column}${0}".into(),
        },
        &Default::default(),
    )
    .unwrap();
    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |e, cx| {
            crate::sql_editor::replace_range(e, start..end, expansion.text.clone(), window, cx);
            assert_eq!(
                e.value().as_ref(),
                "SELECT name || name FROM customers; -- suffix"
            );
            assert_eq!(e.cursor(), start + expansion.final_caret);
            for range in &expansion.tabstops[0].ranges {
                assert_eq!(&e.value()[start + range.start..start + range.end], "name");
            }
        })
    })
    .unwrap();
    assert_undo(cx, window, &editor, original);
}

#[gpui_kit::test]
fn actual_two_window_editors_reject_typing_and_escape_until_coordinator_unfreezes(
    cx: &mut TestAppContext,
) {
    let main = fixture(cx, "SELECT main");
    let satellite = fixture(cx, "SELECT satellite");
    for (window, editor) in [&main, &satellite] {
        cx.update_window(*window, |_, window, cx| {
            crate::sql_editor::set_frozen(editor, true, cx);
            editor.update(cx, |e, cx| e.focus(window, cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let original = cx.read(|cx| editor.read(cx).value().to_string());
        cx.simulate_keystrokes(*window, "x escape y ctrl-z");
        cx.read(|cx| assert_eq!(editor.read(cx).value().as_ref(), original));
        cx.update_window(*window, |_, window, cx| {
            crate::sql_editor::set_frozen(editor, false, cx);
            editor.update(cx, |e, cx| e.focus(window, cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.simulate_keystrokes(*window, "z");
        cx.read(|cx| assert_ne!(editor.read(cx).value().as_ref(), original));
    }
}

#[gpui_kit::test]
fn actual_snippet_linked_selections_type_together_and_undo(cx: &mut TestAppContext) {
    let (window, editor) = fixture(cx, "name || name");
    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |e, cx| {
            e.set_selected_range(0..4, cx);
            e.add_selection(8..12, cx);
            e.focus(window, cx);
        });
        window.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(window, "i d");
    cx.read(|cx| assert_eq!(editor.read(cx).value().as_ref(), "id || id"));
    // Selection replacement and the subsequent typing have separate native undo intents.
    assert_undo(cx, window, &editor, "i || i");
    assert_undo(cx, window, &editor, "name || name");
}
