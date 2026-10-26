//! Bounded human summary; exact values remain in the staged batch and detail copy.
use rowboat_core::{Cell, Column, RowChange};
fn brief(value: &Cell) -> String {
    match value {
        Cell::Null => "NULL".into(),
        Cell::Binary(bytes) => format!("binary ({} bytes)", bytes.len()),
        Cell::Text(text) => {
            let prefix = text.chars().take(80).collect::<String>();
            format!(
                "{}{}",
                serde_json::to_string(&prefix).unwrap_or_default(),
                if prefix.len() < text.len() { "…" } else { "" }
            )
        }
    }
}
pub fn summary(change: &RowChange, columns: &[Column]) -> String {
    let (verb, original, values) = match change {
        RowChange::Insert { values } => ("Add row", None, Some(values)),
        RowChange::Update { original, values } => ("Update", Some(original), Some(values)),
        RowChange::Delete { original } => ("Delete row", Some(original), None),
    };
    let keys = original
        .map(|row| {
            row.iter()
                .filter(|(name, _)| columns.iter().any(|c| c.primary_key && &c.name == name))
                .take(4)
                .map(|(n, v)| format!("{n} = {}", brief(v)))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let mut text = if keys.is_empty() {
        verb.into()
    } else {
        format!("{verb} · {keys}")
    };
    if let Some(values) = values {
        for (name, value) in values.iter().take(3) {
            let before = original
                .and_then(|row| row.iter().find(|(n, _)| n == name))
                .map(|(_, v)| format!("{} → ", brief(v)))
                .unwrap_or_default();
            text.push_str(&format!("\n{name}: {before}{}", brief(value)));
        }
        if values.len() > 3 {
            text.push_str(&format!(
                "\n+ {} more fields · open details",
                values.len() - 3
            ));
        }
        if values.is_empty() {
            text.push_str(" · server defaults");
        }
    }
    text
}
