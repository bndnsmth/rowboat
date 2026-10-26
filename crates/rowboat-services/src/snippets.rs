use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnippetTemplate {
    pub id: String,
    pub name: String,
    pub trigger: String,
    pub body: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetTabstop {
    pub name: String,
    pub ranges: Vec<Range<usize>>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetExpansion {
    pub text: String,
    pub tabstops: Vec<SnippetTabstop>,
    pub final_caret: usize,
}

enum Part {
    Text(String),
    Stop(String, Option<String>),
}
fn name_valid(name: &str) -> bool {
    if let Ok(number) = name.parse::<u8>() {
        return number <= 99 && number.to_string() == name;
    }
    let mut bytes = name.bytes();
    name.len() <= 64
        && bytes
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn parse(body: &str) -> Result<Vec<Part>> {
    ensure!(
        body.len() <= 64 * 1024 && !body.contains('\0'),
        "Snippet body exceeds 64KiB or contains NUL"
    );
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut at = 0;
    let mut stops = 0;
    let mut defaults = BTreeMap::new();
    let mut final_seen = false;
    while at < body.len() {
        let tail = &body[at..];
        if tail.starts_with("$$") {
            literal.push('$');
            at += 2;
        } else if tail.starts_with("${") {
            if !literal.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut literal)));
            }
            let end = tail
                .find('}')
                .ok_or_else(|| anyhow::anyhow!("Unclosed snippet placeholder"))?;
            let definition = &tail[2..end];
            let (name, default) = definition
                .split_once(':')
                .map_or((definition, None), |(name, default)| (name, Some(default)));
            ensure!(name_valid(name), "Invalid snippet placeholder name");
            ensure!(
                default.is_none_or(|s| !s.contains("${")),
                "Nested snippet placeholders are unsupported"
            );
            if name == "0" {
                ensure!(
                    default.is_none() && !final_seen,
                    "Final caret must be a single ${{0}} without a default"
                );
                final_seen = true;
            }
            if let Some(default) = default
                && let Some(previous) = defaults.insert(name.to_owned(), default.to_owned())
            {
                ensure!(
                    previous == default,
                    "Repeated placeholder has conflicting defaults"
                );
            }
            stops += 1;
            ensure!(stops <= 128, "Snippet exceeds 128 placeholder occurrences");
            parts.push(Part::Stop(name.to_owned(), default.map(str::to_owned)));
            at += end + 1;
        } else {
            let c = tail.chars().next().expect("nonempty tail");
            literal.push(c);
            at += c.len_utf8();
        }
    }
    if !literal.is_empty() {
        parts.push(Part::Text(literal));
    }
    Ok(parts)
}
impl SnippetTemplate {
    pub fn validate(&self) -> Result<()> {
        for value in [&self.id, &self.name] {
            ensure!(
                !value.trim().is_empty()
                    && value.len() <= 256
                    && !value.chars().any(char::is_control),
                "Snippet ID/name must be nonempty, control-free, and <=256 bytes"
            );
        }
        ensure!(
            !self.trigger.is_empty()
                && self.trigger.len() <= 64
                && self
                    .trigger
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c)),
            "Snippet trigger must contain 1–64 ASCII letters/digits/underscore/dot/hyphen"
        );
        parse(&self.body)?;
        Ok(())
    }
}

pub(crate) fn validate_templates(templates: &[SnippetTemplate]) -> Result<()> {
    ensure!(templates.len() <= 256, "Too many snippet templates");
    let mut ids = HashSet::new();
    let mut triggers = HashSet::new();
    for template in templates {
        template.validate()?;
        ensure!(ids.insert(&template.id), "Duplicate snippet template ID");
        ensure!(
            triggers.insert(&template.trigger),
            "Duplicate snippet template trigger"
        );
    }
    Ok(())
}

pub fn expand_snippet(
    template: &SnippetTemplate,
    values: &BTreeMap<String, String>,
) -> Result<SnippetExpansion> {
    template.validate()?;
    let parts = parse(&template.body)?;
    let mut defaults = BTreeMap::<String, String>::new();
    for part in &parts {
        if let Part::Stop(name, default) = part
            && name != "0"
        {
            let entry = defaults.entry(name.clone()).or_default();
            if let Some(default) = default {
                *entry = default.clone();
            }
        }
    }
    for (name, value) in values {
        ensure!(defaults.contains_key(name), "Unknown snippet value name");
        ensure!(
            value.len() <= 64 * 1024 && !value.contains('\0'),
            "Snippet value exceeds 64KiB or contains NUL"
        );
        defaults.insert(name.clone(), value.clone());
    }
    let mut text = String::new();
    let mut tabstops: Vec<SnippetTabstop> = Vec::new();
    let mut final_caret = None;
    for part in parts {
        let (value, stop) = match part {
            Part::Text(value) => (value, None),
            Part::Stop(name, _) if name == "0" => {
                final_caret = Some(text.len());
                continue;
            }
            Part::Stop(name, _) => (defaults[&name].clone(), Some(name)),
        };
        ensure!(
            text.len() + value.len() <= 256 * 1024,
            "Expanded snippet exceeds 256KiB"
        );
        let start = text.len();
        text.push_str(&value);
        if let Some(name) = stop {
            let range = start..text.len();
            if let Some(existing) = tabstops.iter_mut().find(|s| s.name == name) {
                existing.ranges.push(range);
            } else {
                tabstops.push(SnippetTabstop {
                    name,
                    ranges: vec![range],
                });
            }
        }
    }
    tabstops.sort_by_key(|stop| stop.name.parse::<u8>().ok().unwrap_or(100));
    Ok(SnippetExpansion {
        final_caret: final_caret.unwrap_or(text.len()),
        text,
        tabstops,
    })
}
