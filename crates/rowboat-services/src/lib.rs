//! Private application state, import/export, keyring, SSH, and database tools.
pub mod ai;
pub mod ai_context;
mod completion_dialect;
mod connection_issue;
mod credentials;
mod data;
mod identity;
mod native;
mod preferences;
mod profiles;
mod providers;
pub mod query_library;
mod relay;
mod snippets;
mod sql_assist;
mod sqlite_backup;
mod sqlite_format;
mod store;
mod transfer;
mod tunnel;

pub use connection_issue::{ConnectionIssue, ConnectionStage, connection_issue};
#[cfg(target_os = "macos")]
pub use credentials::credential_helper_main;
pub use credentials::{CredentialLookup, credential_delete, credential_get, credential_set};
pub use data::{
    export_csv, export_json, export_sql, format_sql, format_sql_for_engine, import_csv, import_json,
};
pub use identity::{environment_name, environment_os, environment_text, resolve_data_directory};
pub use native::{ClientRuntime, backup, restore};
pub use preferences::{
    CustomMetricDefinition, Dashboard, DashboardCard, MetricBuffer, MetricPoint, Preferences,
    SavedFilter, builtin_metrics, canonical_key_chord, default_shortcuts, validate_app_state,
};
pub use profiles::{
    ExportOptions, ImportIdPolicy, ImportOptions, ImportReport, MergePolicy, ParsedConnectionUrl,
    connection_url, export_connections, export_connections_with_options, import_connections,
    import_connections_with_options, merge_connections, merge_connections_with_policy,
    parse_connection_url, validate_connection_host, validate_profile,
};
pub use providers::{
    ServiceOperation, UnsupportedServiceOperation, default_backup_image, service_support,
};
pub use snippets::{SnippetExpansion, SnippetTabstop, SnippetTemplate, expand_snippet};
pub use sql_assist::{
    CatalogTable, CompletionCatalog, CompletionContext, CompletionKind, SqlCompletion,
    SqlCompletions, complete_sql,
};
pub use store::{AppState, HISTORY_LIMIT, HistoryEntry, Snippet, Store};
pub use transfer::{
    ExportRequest, ImportPreview, TransferControl, TransferFormat, TransferOptions, TransferPhase,
    TransferProgress, TransferSummary, apply_import, export_query, preview_import,
};
pub use tunnel::{Tunnel, TunnelOptions};
