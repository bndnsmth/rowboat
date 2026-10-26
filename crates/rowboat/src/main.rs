extern crate gpui_kit as gpui;

mod access_ui;
mod advanced;
mod app;
mod app_runtime;
mod brand;
mod chrome_ui;
mod columns_ui;
mod completion_popup;
mod completion_ui;
mod custom_metrics_ui;
mod dashboards_ui;
mod data_transfer_ui;
mod diagram_model;
mod diagram_ui;
mod execution_ui;
mod explain_ui;
mod filters_ui;
mod forms;
mod grid_ui;
mod inline_completion;
mod keyboard_ui;
mod library_ui;
mod loading_ui;
mod management;
mod management_ui;
mod namespace_ui;
mod objects_ui;
mod palette;
#[cfg(test)]
mod pass5_tests;
mod preferences_ui;
mod profiles_ui;
mod provider_ui;
mod query_edit_ui;
mod query_stream_ui;
mod query_timing_ui;
mod recovery_ui;
mod relationships_ui;
mod result_compare_ui;
mod result_snapshots;
mod review_ui;
mod services_ui;
mod session;
mod snippets_ui;
mod split_editors;
mod sql_editor;
mod staging;
mod theme_controller;
mod view;
mod workspace_model;
mod workspace_shared;
mod workspace_shell;
mod workspace_storage;

use gpui_kit::*;

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(code) = rowboat_services::credential_helper_main() {
        std::process::exit(code);
    }
    let mut startup = app::Startup::default();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--connection" => {
                startup.connection = Some(arguments.next().ok_or_else(|| {
                    anyhow::anyhow!("--connection requires a saved profile name or ID")
                })?)
            }
            "--query" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--query requires a SQL file"))?;
                startup.sql = Some(std::fs::read_to_string(path)?);
            }
            "--version" | "-V" => {
                println!("Rowboat {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Rowboat — native PostgreSQL, MySQL and SQLite workbench\n\nUsage: rowboat [--connection <saved-name-or-id>] [--query <sql-file>]\n\nROWBOAT_PASSWORD supplies a session-only network password; SQLite uses its saved file path.\n--query loads SQL into the first tab; with --connection, it runs after connecting.\nROWBOAT_DATA_DIR selects the local state directory."
                );
                return Ok(());
            }
            _ => anyhow::bail!("Unknown argument: {argument}. Use --help."),
        }
    }
    let password = rowboat_services::environment_text("ROWBOAT_PASSWORD")?;
    startup.password_supplied = password.is_some();
    startup.password = password.unwrap_or_default();
    startup.autorun = startup.connection.is_some() && startup.sql.is_some();
    let runtime_owner = app_runtime::AppRuntime::new()?;
    let runtime = runtime_owner.handle();
    gpui_kit::application()
        .with_assets(brand::Assets)
        .run(move |cx| {
            cx.set_app_identity("dev.bndnsmth.rowboat", "Rowboat");
            gpui_omarchy::init(cx);
            theme_controller::init(cx);
            keyboard_ui::bind(cx);
            cx.bind_keys([
                KeyBinding::new("tab", app::NextSnippet, Some("RowboatSnippet > Input")),
                KeyBinding::new(
                    "shift-tab",
                    app::PreviousSnippet,
                    Some("RowboatSnippet > Input"),
                ),
                KeyBinding::new(
                    "ctrl-shift-space",
                    app::ExpandSnippet,
                    Some("RowboatSqlEditor > Input"),
                ),
                KeyBinding::new("alt-pageup", app::PreviousResult, Some("Rowboat")),
                KeyBinding::new("alt-pagedown", app::NextResult, Some("Rowboat")),
                KeyBinding::new("escape", app::Dismiss, Some("Rowboat")),
                KeyBinding::new("escape", app::Dismiss, Some("Rowboat > Input")),
                KeyBinding::new("escape", app::Dismiss, Some("RowboatSqlEditor > Input")),
            ]);
            preferences_ui::bind_shortcuts(&Default::default(), &Default::default(), cx)
                .expect("Valid built-in shortcuts");
            let options = WindowOptions {
                app_id: Some("dev.bndnsmth.rowboat".into()),
                window_background: WindowBackgroundAppearance::Opaque,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(940.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Rowboat — Database Workbench".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(960.), px(640.))),
                ..Default::default()
            };
            if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| workspace_shell::WorkspaceShell::new(runtime, startup, window, cx))
            }) {
                eprintln!("Unable to open Rowboat: {error:#}");
                cx.quit();
            }
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
    drop(runtime_owner);
    Ok(())
}
mod connection_ui;
