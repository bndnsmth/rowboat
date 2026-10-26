use rowboat_admin::{ddl::*, live_diff::*};
use rowboat_core::{Engine, TableRef};
use serde_json::json;
use std::collections::BTreeMap;

fn snapshot(schema: &str, columns: &[&str]) -> SchemaSnapshot {
    SchemaSnapshot {
        engine: Engine::Postgres,
        schema: schema.into(),
        origin: json!({"user":"tester"}),
        tables: vec![TableSnapshot {
            name: "t".into(),
            definition: Some(TableDesign {
                table: TableRef {
                    schema: schema.into(),
                    name: "t".into(),
                },
                columns: columns
                    .iter()
                    .map(|n| ColumnDesign {
                        name: (*n).into(),
                        data_type: DataType::Integer,
                        nullable: true,
                        default: None,
                    })
                    .collect(),
                primary_key: None,
                indexes: vec![],
                foreign_keys: vec![],
            }),
            mysql_columns: vec![],
            mysql_options: None,
            uniques: vec![],
            checks: vec![],
            unsupported: vec![],
            catalog: BTreeMap::new(),
        }],
        unsupported: vec![],
        warnings: vec![],
        catalog: BTreeMap::new(),
    }
}
fn mapping() -> NamespaceMapping {
    NamespaceMapping {
        source: "source".into(),
        destination: "destination".into(),
    }
}
#[test]
fn explicit_mapping_and_same_engine_are_required() {
    let source = snapshot("source", &["id"]);
    let mut target = snapshot("destination", &["id"]);
    assert!(
        compare_snapshots(
            &source,
            &target,
            &NamespaceMapping {
                source: "guess".into(),
                destination: "destination".into()
            },
            CompareOptions::default()
        )
        .is_err()
    );
    target.engine = Engine::MySql;
    assert!(compare_snapshots(&source, &target, &mapping(), CompareOptions::default()).is_err());
}
#[test]
fn destination_only_columns_are_preserved_unless_opted_in() {
    let source = snapshot("source", &["id"]);
    let target = snapshot("destination", &["id", "extra"]);
    let before = target.clone();
    let additive =
        compare_snapshots(&source, &target, &mapping(), CompareOptions::default()).unwrap();
    assert!(!additive.is_blocked());
    assert!(additive.plan.is_empty());
    assert!(
        additive
            .objects
            .iter()
            .any(|o| o.name == "extra" && o.change == ChangeKind::Preserve)
    );
    let destructive = compare_snapshots(
        &source,
        &target,
        &mapping(),
        CompareOptions {
            drop_destination_only: true,
        },
    )
    .unwrap();
    assert!(!destructive.is_blocked());
    assert!(destructive.plan.sql().contains("DROP COLUMN \"extra\""));
    assert_eq!(destructive.original_destination(), &before);
    assert_eq!(target, before);
    assert!(
        serde_json::to_value(&destructive)
            .unwrap()
            .get("original_destination")
            .is_some()
    );
}
#[test]
fn unsupported_features_and_middle_insertion_never_leave_partial_sql() {
    let mut source = snapshot("source", &["id", "middle", "last"]);
    let target = snapshot("destination", &["id", "last"]);
    for options in [
        CompareOptions::default(),
        CompareOptions {
            drop_destination_only: true,
        },
    ] {
        let blocked = compare_snapshots(&source, &target, &mapping(), options).unwrap();
        assert!(blocked.is_blocked());
        assert!(blocked.plan.is_empty());
    }
    source.tables[0]
        .unsupported
        .push("Generated identity requires sequence migration".into());
    source.tables[0]
        .definition
        .as_mut()
        .unwrap()
        .columns
        .remove(1);
    let blocked =
        compare_snapshots(&source, &target, &mapping(), CompareOptions::default()).unwrap();
    assert!(blocked.is_blocked());
    assert!(blocked.plan.is_empty());
}
#[test]
fn expression_parse_environment_mismatch_is_blocked() {
    let mut source = snapshot("source", &["id"]);
    let mut target = snapshot("destination", &["id"]);
    source.origin["date_style"] = json!("ISO, MDY");
    target.origin["date_style"] = json!("ISO, DMY");
    let blocked =
        compare_snapshots(&source, &target, &mapping(), CompareOptions::default()).unwrap();
    assert!(blocked.is_blocked() && blocked.plan.is_empty());
    assert!(blocked.blockers.iter().any(|s| s.contains("date_style")));
}
