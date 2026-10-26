//! Schema, database-object, and administrative workbench operations.
//!
//! DDL/access builders only produce reviewable SQL. They never execute it.

pub mod access;
pub mod access_matrix;
mod columns;
pub mod ddl;
pub mod diff;
pub mod explain;
pub mod live_diff;
pub mod metrics;
pub mod object_editor;
pub mod objects;
pub mod providers;
pub mod query;
#[path = "providers/sqlite_schema.rs"]
pub mod sqlite_schema;
