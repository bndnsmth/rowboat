//! Versioned, UI-independent AI evidence. Snapshots contain values and provenance,
//! never GPUI entities, database handles, credentials, or the application store.
use anyhow::{Result, ensure};
use rowboat_core::{Cell, Column, Engine, TableRef};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub workspace: u64,
    pub session_generation: u64,
    pub connection_id: Option<String>,
    pub engine: Option<Engine>,
    pub database: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryContext {
    pub tab: usize,
    pub title: String,
    pub revision: u64,
    pub sql: Option<String>,
    pub selected_bytes: Option<(usize, usize)>,
    pub truncated: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectedCell {
    pub row: usize,
    pub column: usize,
    pub value: Cell,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultContext {
    pub kind: String,
    pub result_index: usize,
    pub columns: Vec<Column>,
    pub retained_rows: usize,
    pub truncated: bool,
    pub completeness: String,
    pub source: Option<String>,
    pub database: Option<String>,
    pub selected_cells: Vec<SelectedCell>,
    pub omitted_selected_cells: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceContext {
    pub version: u32,
    pub captured_at: String,
    pub scope: Scope,
    pub destination: String,
    pub sql_context: Option<String>,
    pub selected_table: Option<TableRef>,
    pub query: Option<QueryContext>,
    pub result: Option<ResultContext>,
    pub schema_and_indexes: Option<serde_json::Value>,
    pub captured_plan: Option<serde_json::Value>,
    pub existing_metric_sample: Option<serde_json::Value>,
    pub transaction: Option<bool>,
    pub pending_edits: usize,
    pub limits: Vec<String>,
}
impl WorkspaceContext {
    pub fn to_json(&self) -> Result<String> {
        let json = serde_json::to_string_pretty(self)?;
        ensure!(
            json.len() <= super::ai::CONTEXT_LIMIT,
            "Workspace context exceeds 192 KiB. Exclude some evidence before sending."
        );
        Ok(json)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub text: String,
}

/// Re-send a bounded, explicit conversation, rather than resume an opaque CLI
/// session which may have permissions or a database target from another turn.
pub fn conversation(context: &str, history: &[Message], question: &str) -> Result<String> {
    ensure!(!question.trim().is_empty(), "Enter a question first");
    let input = serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "workspace_context": serde_json::from_str::<serde_json::Value>(context).unwrap_or_else(|_| serde_json::Value::String(context.into())),
        "previous_messages_untrusted": history,
        "user_question": question,
    }))?;
    ensure!(
        input.len() <= super::ai::CONTEXT_LIMIT,
        "Conversation exceeds 192 KiB. Start a new conversation or reduce context."
    );
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversation_preserves_redactions_and_never_implicitly_attaches_other_state() {
        let input = conversation("User-redacted preview", &[], "Write a SELECT").unwrap();
        let value: serde_json::Value = serde_json::from_str(&input).unwrap();
        assert_eq!(value["workspace_context"], "User-redacted preview");
        assert_eq!(value["user_question"], "Write a SELECT");
        assert!(conversation("{}", &[], " ").is_err());
        assert!(conversation(&"x".repeat(super::super::ai::CONTEXT_LIMIT), &[], "q").is_err());
    }
}
