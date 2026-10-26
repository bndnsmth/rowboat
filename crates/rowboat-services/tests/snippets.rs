use rowboat_services::*;
use std::collections::BTreeMap;

fn template(body: &str) -> SnippetTemplate {
    SnippetTemplate {
        id: "one".into(),
        name: "Select rows".into(),
        trigger: "sel".into(),
        body: body.into(),
    }
}

#[test]
fn expansion_tracks_unicode_mirrors_numeric_order_and_final_caret() {
    let draft = template(
        "SELECT ${2:name}, ${1:id} FROM ${table:日本語} WHERE ${1} = ${value};${0} -- ${table}",
    );
    let expanded = expand_snippet(
        &draft,
        &BTreeMap::from([("value".into(), "'literal ${other}'".into())]),
    )
    .unwrap();
    assert_eq!(
        expanded.text,
        "SELECT name, id FROM 日本語 WHERE id = 'literal ${other}'; -- 日本語"
    );
    assert_eq!(
        expanded
            .tabstops
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["1", "2", "table", "value"]
    );
    for stop in &expanded.tabstops {
        let first = &expanded.text[stop.ranges[0].clone()];
        for range in &stop.ranges {
            assert_eq!(&expanded.text[range.clone()], first);
        }
    }
    assert_eq!(&expanded.text[expanded.final_caret..], " -- 日本語");
    assert_eq!(expanded.tabstops[0].ranges.len(), 2);
}

#[test]
fn escapes_defaults_and_literal_favorites_have_distinct_semantics() {
    let expanded = expand_snippet(
        &template("SELECT $${literal}, $$$$, $1, ${unset}, ${name}, ${name:later}"),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(expanded.text, "SELECT ${literal}, $$, $1, , later, later");
    assert_eq!(expanded.final_caret, expanded.text.len());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_at(dir.path()).unwrap();
    std::fs::write(dir.path().join("state.json"), r#"{"snippets":[{"id":"old","name":"Favorite","sql":"SELECT ${not-a-template}, $$body$$"}]}"#).unwrap();
    let mut state = store.load().unwrap();
    assert!(state.snippet_templates.is_empty());
    let old = state.snippets[0].sql.clone();
    state
        .snippet_templates
        .push(template("SELECT ${column:id} FROM ${table}"));
    store.save(&state).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.snippets[0].sql, old);
    assert_eq!(
        loaded.snippet_templates[0].body,
        "SELECT ${column:id} FROM ${table}"
    );
}

#[test]
fn invalid_drafts_and_duplicate_metadata_preserve_atomic_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_at(dir.path()).unwrap();
    let good = AppState {
        snippet_templates: vec![template("SELECT ${name:column}")],
        ..Default::default()
    };
    store.save(&good).unwrap();
    let path = dir.path().join("state.json");
    let before = std::fs::read(&path).unwrap();
    for body in [
        "${",
        "${bad-name}",
        "${100}",
        "${01}",
        "${0:bad}",
        "${0}${0}",
        "${a:first}${a:second}",
        "${a:${b}}",
    ] {
        let mut bad = good.clone();
        bad.snippet_templates[0].body = body.into();
        assert!(store.save(&bad).is_err(), "accepted {body}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let mut bad = good.clone();
    bad.snippet_templates.push(template("SELECT 1"));
    assert!(store.save(&bad).is_err());
    bad.snippet_templates[1].id = "different".into();
    assert!(store.save(&bad).is_err());
    bad.snippet_templates[1].trigger = "other".into();
    store.save(&bad).unwrap();
    assert!(
        expand_snippet(
            &template("${name}"),
            &BTreeMap::from([("unknown".into(), "secret-not-echoed".into())])
        )
        .unwrap_err()
        .to_string()
        .find("secret-not-echoed")
        .is_none()
    );
}

#[test]
fn expansion_and_storage_limits_reject_excess_without_truncation() {
    assert!(template(&"x".repeat(65537)).validate().is_err());
    assert!(template(&"${x}".repeat(129)).validate().is_err());
    let huge = BTreeMap::from([("x".into(), "x".repeat(65536))]);
    assert!(expand_snippet(&template(&"${x}".repeat(5)), &huge).is_err());
    assert!(
        expand_snippet(
            &template("${x}"),
            &BTreeMap::from([("x".into(), "x".repeat(65537))])
        )
        .is_err()
    );
    let mut state = AppState::default();
    for n in 0..257 {
        state.snippet_templates.push(SnippetTemplate {
            id: n.to_string(),
            trigger: n.to_string(),
            ..template("SELECT 1")
        });
    }
    assert!(validate_app_state(&state).is_err());
}
