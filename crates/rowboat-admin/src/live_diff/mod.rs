//! Same-engine live table-DDL comparison with explicit namespace mapping.
//! Unsupported catalog features block the entire plan. Never applies SQL.
pub mod columns;
pub mod definitions;
mod inspect;
mod plan;

use crate::ddl::{CheckDesign, DdlPlan, MySqlColumnDefinition, TableDesign, UniqueDesign};
use anyhow::{Result, ensure};
use rowboat_core::{Database, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub use crate::object_editor::{ObjectAttributeComparison, compare_object_attributes};
pub use inspect::snapshot_schema;
pub use plan::compare_snapshots;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchemaSnapshot {
    pub engine: Engine,
    pub schema: String,
    /// Server/database/user identity, excluding session IDs and credentials.
    pub origin: Value,
    pub tables: Vec<TableSnapshot>,
    pub unsupported: Vec<String>,
    pub warnings: Vec<String>,
    /// Stable catalog evidence used by stale detection, not by SQL rendering.
    pub catalog: BTreeMap<String, Vec<Value>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableSnapshot {
    pub name: String,
    /// None when a column cannot be represented. Native evidence is retained.
    pub definition: Option<TableDesign>,
    pub mysql_columns: Vec<MySqlColumnDefinition>,
    pub mysql_options: Option<MySqlTableOptions>,
    pub uniques: Vec<UniqueDesign>,
    pub checks: Vec<CheckDesign>,
    pub unsupported: Vec<String>,
    pub catalog: BTreeMap<String, Vec<Value>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MySqlTableOptions {
    pub character_set: String,
    pub collation: String,
    pub comment: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceMapping {
    pub source: String,
    pub destination: String,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareOptions {
    /// Applies to destination-only tables AND columns/keys/indexes/constraints.
    pub drop_destination_only: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DifferenceKind {
    Table,
    Column,
    PrimaryKey,
    ForeignKey,
    Index,
    Unique,
    Check,
    TableOptions,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ChangeKind {
    Create,
    Alter,
    Drop,
    Preserve,
    Unchanged,
    Blocked,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectDifference {
    pub table: String,
    pub kind: DifferenceKind,
    pub name: String,
    pub change: ChangeKind,
    pub detail: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct LiveComparison {
    pub plan: DdlPlan,
    pub objects: Vec<ObjectDifference>,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
    original_destination: SchemaSnapshot,
}
impl LiveComparison {
    pub fn is_blocked(&self) -> bool {
        !self.blockers.is_empty()
    }
    pub fn original_destination(&self) -> &SchemaSnapshot {
        &self.original_destination
    }
}

pub async fn compare_live(
    source: &Database,
    destination: &Database,
    mapping: &NamespaceMapping,
    options: CompareOptions,
) -> Result<LiveComparison> {
    ensure!(
        source.engine() == destination.engine(),
        "Cross-engine live comparison is not supported"
    );
    let source = snapshot_schema(source, &mapping.source).await?;
    let destination = snapshot_schema(destination, &mapping.destination).await?;
    compare_snapshots(&source, &destination, mapping, options)
}

/// Compares catalog state and connection origin, not table data. Does not hold
/// locks: callers must account for a race between this check and applying DDL.
pub async fn destination_is_current(db: &Database, comparison: &LiveComparison) -> Result<bool> {
    let original = comparison.original_destination();
    if db.engine() != original.engine {
        return Ok(false);
    }
    let current = snapshot_schema(db, &original.schema).await?;
    Ok(&current == original)
}
