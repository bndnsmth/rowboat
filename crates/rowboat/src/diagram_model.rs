//! Bounded, deterministic graph of declared foreign keys. No I/O or inferred edges.
use rowboat_core::{Column, ForeignKeyRelationship, TableRef};

pub const MAX_NODES: usize = 24;
pub const MAX_EDGES: usize = 64;
pub const CARD_WIDTH: f32 = 250.;
pub const CARD_HEIGHT: f32 = 220.;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Identity {
    pub schema: String,
    pub name: String,
}
impl From<&TableRef> for Identity {
    fn from(t: &TableRef) -> Self {
        Self {
            schema: t.schema.clone(),
            name: t.name.clone(),
        }
    }
}
impl Identity {
    pub fn table(&self) -> TableRef {
        TableRef {
            schema: self.schema.clone(),
            name: self.name.clone(),
        }
    }
    pub fn label(&self) -> String {
        // Quoted components keep identifiers containing dots unambiguous.
        format!(
            "\"{}\".\"{}\"",
            self.schema.replace('"', "\"\""),
            self.name.replace('"', "\"\"")
        )
    }
}

#[derive(Clone)]
pub struct Node {
    pub identity: Identity,
    pub columns: Vec<Column>,
    pub inspected: bool,
    pub x: f32,
    pub y: f32,
}
#[derive(Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<ForeignKeyRelationship>,
    pub limited: bool,
}
impl Graph {
    pub fn ensure_node(&mut self, table: &TableRef) -> Option<usize> {
        let key = Identity::from(table);
        if let Some(i) = self.nodes.iter().position(|n| n.identity == key) {
            return Some(i);
        }
        if self.nodes.len() >= MAX_NODES {
            self.limited = true;
            return None;
        }
        let index = self.nodes.len();
        self.nodes.push(Node {
            identity: key,
            columns: vec![],
            inspected: false,
            x: 24. + (index % 3) as f32 * 330.,
            y: 24. + (index / 3) as f32 * 290.,
        });
        Some(index)
    }
    pub fn merge(
        &mut self,
        table: &TableRef,
        columns: Vec<Column>,
        edges: Vec<ForeignKeyRelationship>,
    ) {
        let Some(index) = self.ensure_node(table) else {
            return;
        };
        self.nodes[index].columns = columns;
        self.nodes[index].inspected = true;
        for edge in edges {
            // Reject malformed/unrelated reports; never infer a relationship from
            // matching labels, primary-key names or display-qualified strings.
            if edge.columns.is_empty() || (edge.source != *table && edge.referenced != *table) {
                continue;
            }
            if self.edges.iter().any(|old| same_edge(old, &edge)) {
                continue;
            }
            if self.edges.len() == MAX_EDGES {
                self.limited = true;
                break;
            }
            let missing = [&edge.source, &edge.referenced]
                .iter()
                .map(|t| Identity::from(*t))
                .collect::<std::collections::BTreeSet<_>>()
                .iter()
                .filter(|id| !self.nodes.iter().any(|n| &n.identity == *id))
                .count();
            if self.nodes.len() + missing > MAX_NODES {
                self.limited = true;
                continue;
            }
            self.ensure_node(&edge.source);
            self.ensure_node(&edge.referenced);
            self.edges.push(edge);
        }
    }
    pub fn key_columns(&self, index: usize) -> Vec<(String, String, bool, bool)> {
        let node = &self.nodes[index];
        let table = node.identity.table();
        let mut columns: Vec<_> = node
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone(), c.primary_key, false))
            .collect();
        for edge in &self.edges {
            for pair in &edge.columns {
                for (matches, name, fk) in [
                    (edge.source == table, &pair.source, true),
                    (edge.referenced == table, &pair.referenced, false),
                ] {
                    if !matches {
                        continue;
                    }
                    if let Some(c) = columns.iter_mut().find(|c| &c.0 == name) {
                        c.3 |= fk;
                    } else {
                        columns.push((name.clone(), "type not inspected".into(), false, fk));
                    }
                }
            }
        }
        columns.sort_by_key(|c| (!c.2, !c.3));
        columns
    }
}
fn same_edge(a: &ForeignKeyRelationship, b: &ForeignKeyRelationship) -> bool {
    a.source == b.source
        && a.referenced == b.referenced
        && a.name == b.name
        && a.columns.len() == b.columns.len()
        && a.columns
            .iter()
            .zip(&b.columns)
            .all(|(a, b)| a.source == b.source && a.referenced == b.referenced)
}

#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub x: f32,
    pub y: f32,
    pub zoom: f32,
}
impl Default for Camera {
    fn default() -> Self {
        Self {
            x: 0.,
            y: 0.,
            zoom: 1.,
        }
    }
}
impl Camera {
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.x = (self.x + dx).clamp(-10000., 10000.);
        self.y = (self.y + dy).clamp(-10000., 10000.);
    }
    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.5, 1.5);
    }
    pub fn center_node(&mut self, node: &Node) {
        self.x = 24. - node.x * self.zoom;
        self.y = 24. - node.y * self.zoom;
    }
}

#[cfg(test)]
#[path = "diagram_model_tests.rs"]
mod tests;
