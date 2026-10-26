# Native SQL editor integration

`src/sql_editor.rs` uses the existing GPUI Kit 0.7.3 Base `EditorState` and
`Editor`. No manifest changes or additional dependencies are needed.

```rust
pub fn create(
    sql: impl Into<gpui_kit::SharedString>,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::App,
) -> gpui_kit::Entity<gpui_kit::base::input::EditorState>;

pub fn render(
    state: &gpui_kit::Entity<gpui_kit::base::input::EditorState>,
    cx: &mut gpui_kit::App,
) -> impl gpui_kit::IntoElement + use<>;
```

Integration:

1. Declare `mod sql_editor;` in `main.rs`.
2. Change only `QueryTab.editor` to `Entity<EditorState>` and construct it with
   `crate::sql_editor::create(sql, window, cx)`. `Context<Workbench>` coerces to
   `App` for these calls. Keep connection/form controls as `TextareaState`.
3. In the query editor's height-constrained container, use
   `.child(crate::sql_editor::render(&tab.editor, cx))`. The editor fills that
   container; give it a real height or a constrained flex layout.
4. Existing `value()`, `set_value()`, `focus()`, `InputEvent` subscriptions and
   focus handles remain available. Use `selected_value()` for run-selection in
   the GUI execution handler; this module does not choose execution targets.

The editable text itself is highlighted. Line numbers, horizontal/vertical
scrolling, native undo, clipboard, search and multi-caret behavior are provided
by Base; keep the app's existing `gpui_omarchy::init` initialization. Creation
does not request focus. Theme colors and the monospace font refresh at render.
The highlighter resolves semantic styles from the current palette, so changing
themes does not require editing the SQL. SQL folding is intentionally disabled
because this lightweight lexer does not build a syntax tree.

The lexer handles Unicode byte offsets, case-insensitive keywords, numbers,
quoted identifiers, doubled/backslash escapes, PostgreSQL dollar strings and
parameters, CR/LF line comments, and nested block comments. It accepts a visual
union of PostgreSQL/MySQL conventions: double quotes are identifier-colored,
and backslash escapes are recognized regardless of connection SQL mode.
It is **presentation only**, never a SQL validator, statement splitter, or
security parser. Each change reparses the buffer; a multi-caret batch reparses
only the final snapshot. Visible-range lookup uses binary search over tokens.

Pure tests in this module cover escaped and unfinished constructs, Unicode
prefixes, number boundaries, complete/clipped style coverage, and theme changes.
They require neither a running window nor a database.

Run `cargo test --locked -p rowboat sql_editor::tests` for module regressions and
the separate [headless GUI suite](editor-tests/README.md) for input/undo integration.
