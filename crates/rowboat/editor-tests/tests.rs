#![cfg(test)]
#![allow(dead_code)] // Application entry points are intentionally not launched by TestPlatform.
extern crate gpui_kit as gpui;
mod ai_performance;
mod ai_ux;
mod assistant;
#[path = "../src/assistant_context.rs"]
mod assistant_context;
#[path = "../src/assistant_ui.rs"]
mod assistant_ui;
mod close_lifecycle_tests;
mod connection_navigation;
mod connection_polish;
#[path = "../src/connection_ui.rs"]
mod connection_ui;
mod gui_pass16;
mod item3_result_tests;
mod item5_record_tests;
mod navigation_recovery;
mod pass13_connections;
mod pass14_tools;
mod pass17_preferences;
mod pass19_navigation;
mod pass20_connections;
mod pass21_themes;
mod pass22_diagram;
mod pass22_execution; // Scoped execution, error navigation, and admission regressions.
mod pass22_library;

// Tests use the same helper that production completion and snippet insertion call.
#[path = "../src/access_ui.rs"]
mod access_ui;
#[path = "../src/advanced.rs"]
mod advanced;
#[path = "../src/ai_ui.rs"]
mod ai_ui;
#[path = "../src/app.rs"]
mod app;
#[path = "../src/app_runtime.rs"]
mod app_runtime;
#[path = "../src/brand.rs"]
mod brand;
#[path = "../src/chrome_ui.rs"]
mod chrome_ui;
#[path = "../src/columns_ui.rs"]
mod columns_ui;
#[path = "../src/completion_popup.rs"]
mod completion_popup;
#[path = "../src/completion_ui.rs"]
mod completion_ui;
#[path = "../src/custom_metrics_ui.rs"]
mod custom_metrics_ui;
#[path = "../src/dashboards_ui.rs"]
mod dashboards_ui;
#[path = "../src/data_transfer_ui.rs"]
mod data_transfer_ui;
#[path = "../src/diagram_model.rs"]
mod diagram_model;
#[path = "../src/diagram_ui.rs"]
mod diagram_ui;
#[path = "../src/dialog.rs"]
mod dialog;
#[path = "../src/editor_replacement_tests.rs"]
mod editor_replacement_tests;
#[path = "../src/execution_ui.rs"]
mod execution_ui;
#[path = "../src/explain_ui.rs"]
mod explain_ui;
#[path = "../src/filters_ui.rs"]
mod filters_ui;
#[path = "../src/forms.rs"]
mod forms;
#[path = "../src/grid_ui.rs"]
mod grid_ui;
#[path = "../src/inline_completion.rs"]
mod inline_completion;
#[path = "../src/keyboard_ui.rs"]
mod keyboard_ui;
#[path = "../src/library_ui.rs"]
mod library_ui;
#[path = "../src/loading_ui.rs"]
mod loading_ui;
#[path = "../src/management.rs"]
mod management;
#[path = "../src/management_ui.rs"]
mod management_ui;
#[path = "../src/namespace_ui.rs"]
mod namespace_ui;
#[path = "../src/objects_ui.rs"]
mod objects_ui;
#[path = "../src/palette.rs"]
mod palette;
mod pass10_tests;
mod pass11_tests;
mod pass12_tests;
mod pass13_keyboard;
mod pass13_results;
#[path = "../src/pass5_tests.rs"]
mod pass5_tests;
mod pass7_tests;
mod pass8_close_tests;
mod pass8_tests;
mod pass9_query_edit_tests;
mod pass9_tests;
#[path = "../src/preferences_ui.rs"]
mod preferences_ui;
#[path = "../src/profiles_ui.rs"]
mod profiles_ui;
#[path = "../src/provider_ui.rs"]
mod provider_ui;
#[path = "../src/query_edit_ui.rs"]
mod query_edit_ui;
#[path = "../src/query_stream_ui.rs"]
mod query_stream_ui;
#[path = "../src/query_timing_ui.rs"]
mod query_timing_ui;
#[path = "../src/recovery_ui.rs"]
mod recovery_ui;
#[path = "../src/relationships_ui.rs"]
mod relationships_ui;
#[path = "../src/result_compare_ui.rs"]
mod result_compare_ui;
#[path = "../src/result_snapshots.rs"]
mod result_snapshots;
#[path = "../src/review_ui.rs"]
mod review_ui;
#[path = "../src/services_ui.rs"]
mod services_ui;
#[path = "../src/session.rs"]
mod session;
#[path = "../src/snippets_ui.rs"]
mod snippets_ui;
#[path = "../src/split_editors.rs"]
mod split_editors;
#[path = "../src/sql_editor.rs"]
mod sql_editor;
#[path = "../src/staging.rs"]
mod staging;
mod test_theme;
#[path = "../src/theme_controller.rs"]
mod theme_controller;
#[path = "../src/view.rs"]
mod view;
mod workbench_tests;
#[path = "../src/workspace_model.rs"]
mod workspace_model;
#[path = "../src/workspace_shared.rs"]
mod workspace_shared;
#[path = "../src/workspace_shell.rs"]
mod workspace_shell;
#[path = "../src/workspace_storage.rs"]
mod workspace_storage;
