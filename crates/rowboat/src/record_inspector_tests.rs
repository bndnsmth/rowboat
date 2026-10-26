use super::{
    JsonFallback, JsonTree, MAX_JSON_BYTES, MAX_JSON_DEPTH, MAX_JSON_NODES, RecordSnapshot, prefix,
    preview, search_record,
};
use crate::forms::RowTarget;
use rowboat_core::{Cell, Column, QueryResult, RowChange, TableRef};
use std::collections::BTreeSet;

fn result(values: Vec<Cell>) -> QueryResult {
    QueryResult {
        columns: values
            .iter()
            .enumerate()
            .map(|(i, _)| Column {
                name: if i == 0 {
                    "雪 / field".into()
                } else {
                    format!("column_{i}")
                },
                data_type: "text".into(),
                nullable: true,
                primary_key: false,
                default_value: None,
            })
            .collect(),
        rows: vec![values],
        ..Default::default()
    }
}

#[test]
fn record_json_copies_source_lexemes_not_floating_point_or_reserialized_strings() {
    let raw = r#" {"integer":1234567890123456789012345678901234567890,"decimal":0.123456789012345678901234567890,"exponent":1e+9999,"negative":-0,"string":"\u0041","null":null} "#;
    let tree = JsonTree::parse(raw).unwrap();
    for (pointer, expected) in [
        ("/integer", "1234567890123456789012345678901234567890"),
        ("/decimal", "0.123456789012345678901234567890"),
        ("/exponent", "1e+9999"),
        ("/negative", "-0"),
        ("/string", r#""\u0041""#),
        ("/null", "null"),
    ] {
        let index = tree
            .nodes
            .iter()
            .position(|n| n.pointer == pointer)
            .unwrap();
        assert_eq!(tree.original(index), expected);
    }
    assert_eq!(tree.raw, raw);
    assert_eq!(tree.original(0), raw.trim());
}

#[test]
fn record_json_pointer_escaping_unicode_arrays_and_search_ancestors() {
    let tree = JsonTree::parse(r#"{"a/~":[0,{"\u96ea":"Été"}],"unrelated":false}"#).unwrap();
    assert!(tree.nodes.iter().any(|n| n.pointer == "/a~1~0/1/雪"));
    let collapsed = tree.visible_nodes(&BTreeSet::new(), "", 100);
    assert_eq!(collapsed, [0]);
    let visible = tree.visible_nodes(&BTreeSet::new(), "ÉTÉ", 100);
    let pointers: Vec<_> = visible
        .iter()
        .map(|&i| tree.nodes[i].pointer.as_str())
        .collect();
    assert_eq!(pointers, ["", "/a~1~0", "/a~1~0/1", "/a~1~0/1/雪"]);
    assert_eq!(tree.visible_nodes(&BTreeSet::new(), "雪", 2).len(), 2);
    let expanded = BTreeSet::from([tree.nodes[0].span.start.to_string()]);
    assert_eq!(tree.visible_nodes(&expanded, "", 100).len(), 3);
}

#[test]
fn record_json_bounds_reject_large_deep_invalid_and_cap_wide_trees() {
    assert_eq!(
        JsonTree::parse(&format!("\"{}\"", "x".repeat(MAX_JSON_BYTES))).unwrap_err(),
        JsonFallback::TooLarge
    );
    let deep = format!(
        "{}0{}",
        "[".repeat(MAX_JSON_DEPTH + 1),
        "]".repeat(MAX_JSON_DEPTH + 1)
    );
    assert_eq!(JsonTree::parse(&deep).unwrap_err(), JsonFallback::TooDeep);
    let allowed = format!(
        "{}0{}",
        "[".repeat(MAX_JSON_DEPTH),
        "]".repeat(MAX_JSON_DEPTH)
    );
    assert_eq!(
        JsonTree::parse(&allowed).unwrap().nodes.len(),
        MAX_JSON_DEPTH + 1
    );
    for raw in [
        "",
        "{",
        "[1,]",
        "01",
        r#"{"x": undefined}"#,
        "null garbage",
        r#"{"\uD800":1}"#,
        r#"{"a":"\uDC00"}"#,
    ] {
        assert_eq!(
            JsonTree::parse(raw).unwrap_err(),
            JsonFallback::Invalid,
            "{raw}"
        );
    }
    let wide = format!("[{}]", vec!["null"; 4000].join(","));
    let tree = JsonTree::parse(&wide).unwrap();
    assert_eq!(tree.nodes.len(), MAX_JSON_NODES);
    assert!(tree.truncated);
    assert_eq!(tree.original(0), wide);
    let braces = format!(r#"{{"text":"{}","escaped":"\\\""}}"#, "[".repeat(100));
    assert!(JsonTree::parse(&braces).is_ok());
    assert!(JsonTree::parse(r#"{"\uD83E\uDD80":"\uD83E\uDD80"}"#).is_ok());
}

#[test]
fn record_search_matches_unicode_decoded_values_and_reports_bounded_prefixes() {
    let result = result(vec![
        Cell::Null,
        Cell::Text(r#"{"data":["\u96ea","Été"]}"#.into()),
        Cell::Text(format!("{}needle", "x".repeat(MAX_JSON_BYTES + 1))),
        Cell::Text(String::new()),
        Cell::Binary(vec![0, 255]),
    ]);
    let snapshot = RecordSnapshot::retained("query", &result, 0).unwrap();
    assert_eq!(search_record(&snapshot, "雪").columns, [0, 1]);
    assert_eq!(search_record(&snapshot, "ÉTÉ").columns, [1]);
    assert!(search_record(&snapshot, "needle").columns.is_empty());
    assert!(search_record(&snapshot, "needle").limited);
    assert_eq!(search_record(&snapshot, "sql null").columns, [0]);
    assert_eq!(snapshot.values[3], Cell::Text(String::new()));
    assert_eq!(snapshot.values[4], Cell::Binary(vec![0, 255]));
    assert_ne!(preview(&Cell::Null), preview(&Cell::Text("null".into())));
    assert!(prefix("雪éabc", 4).is_char_boundary(3));
}

#[test]
fn record_staged_overlays_are_owned_and_never_replace_original_or_pinned_values() {
    let result = result(vec![
        Cell::Text("original".into()),
        Cell::Text("null".into()),
    ]);
    let target = RowTarget {
        connection_id: "connection-a".into(),
        session_generation: 9,
        table: TableRef {
            schema: "main".into(),
            name: "records".into(),
        },
    };
    let original: Vec<_> = result
        .columns
        .iter()
        .zip(&result.rows[0])
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect();
    let changes = vec![RowChange::Update {
        original: original.clone(),
        values: vec![("雪 / field".into(), Cell::Binary(vec![0, 128, 255]))],
    }];
    let snapshot = RecordSnapshot::retained("table", &result, 0)
        .unwrap()
        .with_table_changes(&result, 0, &changes, Some(&target), Some(&target));
    assert_eq!(
        snapshot.value(0, false),
        Some(&Cell::Text("original".into()))
    );
    assert_eq!(
        snapshot.value(0, true),
        Some(&Cell::Binary(vec![0, 128, 255]))
    );
    assert_eq!(search_record(&snapshot, "original").columns, [0]);
    for changed in [
        RowTarget {
            session_generation: 10,
            ..target.clone()
        },
        RowTarget {
            connection_id: "connection-b".into(),
            ..target.clone()
        },
        RowTarget {
            table: TableRef {
                name: "elsewhere".into(),
                ..target.table.clone()
            },
            ..target.clone()
        },
    ] {
        let snapshot = RecordSnapshot::retained("table", &result, 0)
            .unwrap()
            .with_table_changes(&result, 0, &changes, Some(&target), Some(&changed));
        assert!(snapshot.staged.is_empty());
        assert_eq!(snapshot.value(0, true), snapshot.value(0, false));
    }
    let pin = RecordSnapshot::retained("pinned", &result, 0).unwrap();
    assert!(pin.staged.is_empty());
    assert!(std::ptr::eq(pin.values, result.rows[0].as_slice()));
    let deletes = vec![RowChange::Delete { original }];
    let deleted = RecordSnapshot::retained("table", &result, 0)
        .unwrap()
        .with_table_changes(&result, 0, &deletes, Some(&target), Some(&target));
    assert!(deleted.deleted);
    assert_eq!(result.rows[0][0], Cell::Text("original".into()));
}

#[test]
fn record_tree_preserves_duplicate_keys_and_every_leaf_exactly() {
    let raw =
        r#"{"same":1,"same":2,"a":[true,false,null,"a\\b\"c",{"🦀":-12.300e-02}],"empty":{}}"#;
    let tree = JsonTree::parse(raw).unwrap();
    let duplicate: Vec<_> = tree
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.pointer == "/same")
        .map(|(i, _)| tree.original(i))
        .collect();
    assert_eq!(duplicate, ["1", "2"]);
    for (index, node) in tree.nodes.iter().enumerate() {
        assert!(node.span.end <= raw.len());
        assert!(serde_json::from_str::<serde::de::IgnoredAny>(tree.original(index)).is_ok());
        if let Some(parent) = node.parent {
            assert!(tree.nodes[parent].span.start < node.span.start);
            assert!(tree.nodes[parent].span.end > node.span.end);
        }
    }
}
