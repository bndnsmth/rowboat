use crate::{model::*, sql::quote_ident};
use anyhow::{Result, anyhow, bail, ensure};
use std::collections::HashSet;

pub const MAX_FILTER_DEPTH: usize = 16;
pub const MAX_FILTER_NODES: usize = 256;
pub const MAX_FILTER_IN_VALUES: usize = 256;
pub const MAX_FILTER_PARAMETERS: usize = 4096;
pub const MAX_FILTER_BYTES: usize = 1024 * 1024;

impl FilterExpr {
    pub fn validate(&self) -> Result<()> {
        validate_filters(&[], Some(self))
    }
}

pub(crate) fn validate_filters(flat: &[Filter], expression: Option<&FilterExpr>) -> Result<()> {
    let mut budget = Budget::default();
    for filter in flat {
        budget.node(1)?;
        budget.value(&filter.value)?;
        budget.name(&filter.column)?;
    }
    if let Some(expr) = expression {
        budget.walk(expr, 1)?;
    }
    Ok(())
}

#[derive(Default)]
struct Budget {
    nodes: usize,
    values: usize,
    bytes: usize,
}
impl Budget {
    fn node(&mut self, depth: usize) -> Result<()> {
        ensure!(
            depth <= MAX_FILTER_DEPTH,
            "Filter exceeds maximum depth {MAX_FILTER_DEPTH}"
        );
        self.nodes += 1;
        ensure!(
            self.nodes <= MAX_FILTER_NODES,
            "Filter exceeds {MAX_FILTER_NODES} nodes"
        );
        Ok(())
    }
    fn name(&mut self, name: &str) -> Result<()> {
        ensure!(
            !name.is_empty() && !name.contains('\0'),
            "Filter column must be a nonempty identifier without NUL"
        );
        self.bytes = self.bytes.saturating_add(name.len());
        self.check_bytes()
    }
    fn check_bytes(&self) -> Result<()> {
        ensure!(
            self.bytes <= MAX_FILTER_BYTES,
            "Filter payload exceeds {MAX_FILTER_BYTES} bytes"
        );
        Ok(())
    }
    fn value(&mut self, value: &Cell) -> Result<()> {
        self.values += 1;
        ensure!(
            self.values <= MAX_FILTER_PARAMETERS,
            "Filter exceeds {MAX_FILTER_PARAMETERS} values"
        );
        self.bytes = self.bytes.saturating_add(match value {
            Cell::Null => 0,
            Cell::Text(s) => s.len(),
            Cell::Binary(b) => b.len(),
        });
        self.check_bytes()
    }
    fn walk(&mut self, expr: &FilterExpr, depth: usize) -> Result<()> {
        self.node(depth)?;
        match expr {
            FilterExpr::Leaf(f) => {
                self.name(&f.column)?;
                self.value(&f.value)?;
            }
            FilterExpr::And(nodes) | FilterExpr::Or(nodes) => {
                ensure!(
                    nodes.len() <= MAX_FILTER_NODES,
                    "Filter group exceeds {MAX_FILTER_NODES} nodes"
                );
                for node in nodes {
                    self.walk(node, depth + 1)?;
                }
            }
            FilterExpr::In { column, values, .. } => {
                self.name(column)?;
                ensure!(
                    values.len() <= MAX_FILTER_IN_VALUES,
                    "IN exceeds {MAX_FILTER_IN_VALUES} values"
                );
                for value in values {
                    self.value(value)?;
                }
            }
            FilterExpr::Between {
                column, low, high, ..
            } => {
                self.name(column)?;
                ensure!(
                    *low != Cell::Null && *high != Cell::Null,
                    "BETWEEN endpoints cannot be NULL"
                );
                self.value(low)?;
                self.value(high)?;
            }
        }
        Ok(())
    }
}

fn column<'a>(columns: &'a [Column], name: &str) -> Result<&'a Column> {
    columns
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow!("Unknown filter column: {name}"))
}
fn bind(engine: Engine, col: &Column, value: &Cell, params: &mut Vec<Cell>) -> String {
    params.push(value.clone());
    match engine {
        Engine::MySql | Engine::Sqlite => "?".into(),
        Engine::Postgres if matches!(value, Cell::Binary(_)) => format!("${}::bytea", params.len()),
        Engine::Postgres => format!("CAST(${}::text AS {})", params.len(), col.data_type),
    }
}

pub(crate) fn compile_filters(
    engine: Engine,
    columns: &[Column],
    flat: &[Filter],
    expression: Option<&FilterExpr>,
    params: &mut Vec<Cell>,
) -> Result<Vec<String>> {
    validate_filters(flat, expression)?;
    let mut predicates = flat
        .iter()
        .map(|f| leaf(engine, columns, f, params))
        .collect::<Result<Vec<_>>>()?;
    if let Some(expr) = expression {
        predicates.push(compile(engine, columns, expr, params)?);
    }
    Ok(predicates)
}

fn leaf(
    engine: Engine,
    columns: &[Column],
    filter: &Filter,
    params: &mut Vec<Cell>,
) -> Result<String> {
    let col = column(columns, &filter.column)?;
    let name = quote_ident(engine, &col.name);
    Ok(match filter.op {
        FilterOp::IsNull => format!("{name} IS NULL"),
        FilterOp::IsNotNull => format!("{name} IS NOT NULL"),
        FilterOp::Eq | FilterOp::NotEq if filter.value == Cell::Null => format!(
            "{name} IS {}NULL",
            if matches!(filter.op, FilterOp::NotEq) {
                "NOT "
            } else {
                ""
            }
        ),
        FilterOp::Contains | FilterOp::StartsWith => {
            let Cell::Text(value) = &filter.value else {
                bail!("Text filters require a text value");
            };
            let value = value
                .replace('!', "!!")
                .replace('%', "!%")
                .replace('_', "!_");
            params.push(Cell::Text(if matches!(filter.op, FilterOp::Contains) {
                format!("%{value}%")
            } else {
                format!("{value}%")
            }));
            match engine {
                Engine::Postgres => format!("{name}::text LIKE ${}::text ESCAPE '!'", params.len()),
                Engine::MySql => format!("CAST({name} AS CHAR) LIKE ? ESCAPE '!'"),
                Engine::Sqlite => format!("CAST({name} AS TEXT) LIKE ? ESCAPE '!'"),
            }
        }
        _ => {
            ensure!(
                filter.value != Cell::Null,
                "NULL supports only equality or IS NULL filters"
            );
            let op = match filter.op {
                FilterOp::Eq => "=",
                FilterOp::NotEq => "<>",
                FilterOp::Less => "<",
                FilterOp::Greater => ">",
                FilterOp::LessEq => "<=",
                FilterOp::GreaterEq => ">=",
                _ => unreachable!(),
            };
            format!("{name} {op} {}", bind(engine, col, &filter.value, params))
        }
    })
}
fn compile(
    engine: Engine,
    columns: &[Column],
    expr: &FilterExpr,
    params: &mut Vec<Cell>,
) -> Result<String> {
    Ok(match expr {
        FilterExpr::Leaf(f) => leaf(engine, columns, f, params)?,
        FilterExpr::And(nodes) | FilterExpr::Or(nodes) => {
            let and = matches!(expr, FilterExpr::And(_));
            if nodes.is_empty() {
                if and { "TRUE" } else { "FALSE" }.into()
            } else {
                format!(
                    "({})",
                    nodes
                        .iter()
                        .map(|n| compile(engine, columns, n, params))
                        .collect::<Result<Vec<_>>>()?
                        .join(if and { " AND " } else { " OR " })
                )
            }
        }
        FilterExpr::In {
            column: name,
            values,
            negated,
        } => {
            let col = column(columns, name)?;
            let name = quote_ident(engine, name);
            let has_null = values.contains(&Cell::Null);
            let values = values
                .iter()
                .filter(|v| **v != Cell::Null)
                .map(|v| bind(engine, col, v, params))
                .collect::<Vec<_>>();
            if values.is_empty() {
                if has_null {
                    format!("{name} IS {}NULL", if *negated { "NOT " } else { "" })
                } else {
                    if *negated { "TRUE" } else { "FALSE" }.into()
                }
            } else {
                let membership = format!(
                    "{name} {}IN ({})",
                    if *negated { "NOT " } else { "" },
                    values.join(", ")
                );
                match (*negated, has_null) {
                    (false, true) => format!("({name} IS NULL OR {membership})"),
                    (false, false) => format!("({name} IS NOT NULL AND {membership})"),
                    (true, true) => format!("({name} IS NOT NULL AND {membership})"),
                    (true, false) => format!("({name} IS NULL OR {membership})"),
                }
            }
        }
        FilterExpr::Between {
            column: name,
            low,
            high,
            negated,
        } => {
            let col = column(columns, name)?;
            format!(
                "({} {}BETWEEN {} AND {})",
                quote_ident(engine, name),
                if *negated { "NOT " } else { "" },
                bind(engine, col, low, params),
                bind(engine, col, high, params)
            )
        }
    })
}

pub fn relationship_filter(
    relationship: &ForeignKeyRelationship,
    direction: RelationshipDirection,
    row: &[(String, Cell)],
) -> Result<Option<RelationshipTarget>> {
    ensure!(
        !relationship.columns.is_empty(),
        "Relationship has no key columns"
    );
    let mut seen = HashSet::new();
    for (name, _) in row {
        ensure!(seen.insert(name), "Duplicate row column: {name}");
    }
    let mut filters = Vec::new();
    let mut missing_link = false;
    let mut keys = HashSet::new();
    for pair in &relationship.columns {
        let (from, to) = match direction {
            RelationshipDirection::Referenced => (&pair.source, &pair.referenced),
            RelationshipDirection::Referencing => (&pair.referenced, &pair.source),
        };
        ensure!(keys.insert(from), "Duplicate relationship key: {from}");
        let value = &row
            .iter()
            .find(|(name, _)| name == from)
            .ok_or_else(|| anyhow!("Row is missing relationship key {from}"))?
            .1;
        missing_link |= *value == Cell::Null;
        filters.push(FilterExpr::Leaf(Filter {
            column: to.clone(),
            op: FilterOp::Eq,
            value: value.clone(),
        }));
    }
    let filter = FilterExpr::And(filters);
    filter.validate()?;
    Ok((!missing_link).then(|| RelationshipTarget {
        table: match direction {
            RelationshipDirection::Referenced => relationship.referenced.clone(),
            RelationshipDirection::Referencing => relationship.source.clone(),
        },
        filter,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_are_checked_before_recursing_or_binding() {
        let mut expr = FilterExpr::And(vec![]);
        for _ in 0..MAX_FILTER_DEPTH {
            expr = FilterExpr::Or(vec![expr]);
        }
        assert!(expr.validate().is_err());
        assert!(
            FilterExpr::And(vec![FilterExpr::And(vec![]); MAX_FILTER_NODES])
                .validate()
                .is_err()
        );
        assert!(
            FilterExpr::In {
                column: "id".into(),
                values: vec![Cell::Null; 257],
                negated: false
            }
            .validate()
            .is_err()
        );
        assert!(
            FilterExpr::Between {
                column: "id".into(),
                low: Cell::Null,
                high: Cell::Text("3".into()),
                negated: false
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn nesting_is_parenthesized_and_values_never_become_sql() {
        let columns = vec![Column {
            name: "a\"b`".into(),
            data_type: "text".into(),
            nullable: true,
            primary_key: false,
            default_value: None,
        }];
        let expr = FilterExpr::Or(vec![
            FilterExpr::In {
                column: columns[0].name.clone(),
                values: vec![Cell::Text("'); DROP TABLE x; --".into()), Cell::Null],
                negated: false,
            },
            FilterExpr::And(vec![]),
        ]);
        for engine in [Engine::Postgres, Engine::MySql] {
            let mut params = Vec::new();
            let sql = compile_filters(engine, &columns, &[], Some(&expr), &mut params)
                .unwrap()
                .join(" AND ");
            assert!(!sql.contains("DROP"));
            assert!(sql.contains("IS NULL OR"));
            assert!(sql.ends_with(" OR TRUE)"));
            assert_eq!(params.len(), 1);
        }
    }
}
