use rowboat_services::{AppState, HISTORY_LIMIT, HistoryEntry, Snippet, Store, query_library::*};
use std::fs;

fn query(id: &str, sql: &str) -> SavedQuery {
    SavedQuery {
        snippet: Snippet {
            id: id.into(),
            name: "Daily report".into(),
            sql: sql.into(),
        },
        metadata: QueryMetadata::default(),
    }
}

#[test]
fn legacy_saved_queries_migrate_losslessly_and_update_the_same_identity() {
    let directory = tempfile::tempdir().unwrap();
    let raw =
        r#"{"snippets":[{"id":"old-id","name":"旧 report","sql":"SELECT 'λ';\n-- retained"}]}"#;
    fs::write(directory.path().join("state.json"), raw).unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    let mut state = store.load().unwrap();
    let before = saved_query(&state, "old-id").unwrap();
    assert_eq!(before.snippet.name, "旧 report");
    assert_eq!(before.snippet.sql, "SELECT 'λ';\n-- retained");
    assert_eq!(before.metadata, QueryMetadata::default());
    let mut after = before.clone();
    after.snippet.sql = "SELECT 2".into();
    after.snippet.name = "Renamed".into();
    after.metadata.folder = "Reports/Daily".into();
    after.metadata.connection_id = Some("descriptive-only".into());
    save_query(&mut state, after.clone(), Some(&before)).unwrap();
    store.save(&state).unwrap();
    let restored = store.load().unwrap();
    assert_eq!(restored.snippets.len(), 1);
    assert_eq!(saved_query(&restored, "old-id"), Some(after));
    assert!(restored.connections.is_empty());
    assert_eq!(restored.last_connection_id, None);
}

#[test]
fn stale_updates_and_deletes_never_replace_another_windows_changes() {
    let mut state = AppState::default();
    let original = query("id", "SELECT 1");
    save_query(&mut state, original.clone(), None).unwrap();
    let mut winner = original.clone();
    winner.snippet.sql = "SELECT 2".into();
    save_query(&mut state, winner.clone(), Some(&original)).unwrap();
    let mut stale = original.clone();
    stale.snippet.name = "Stale rename".into();
    assert!(save_query(&mut state, stale, Some(&original)).is_err());
    assert!(remove_query(&mut state, &original).is_err());
    assert_eq!(saved_query(&state, "id"), Some(winner.clone()));
    remove_query(&mut state, &winner).unwrap();
    assert!(save_query(&mut state, winner.clone(), Some(&winner)).is_err());
    assert!(state.snippets.is_empty());
    assert!(state.query_metadata.is_empty());
}

#[test]
fn save_as_is_explicit_new_identity_and_updates_preserve_unrelated_state() {
    let mut state = AppState {
        last_connection_id: Some("keep".into()),
        ..Default::default()
    };
    let a = query("a", "SELECT 1");
    let b = query("b", "SELECT 1");
    save_query(&mut state, a.clone(), None).unwrap();
    save_query(&mut state, b.clone(), None).unwrap();
    assert!(save_query(&mut state, b.clone(), None).is_err());
    assert!(save_query(&mut state, b, Some(&a)).is_err());
    assert_eq!(state.snippets.len(), 2);
    assert_eq!(state.last_connection_id.as_deref(), Some("keep"));
}

#[test]
fn name_sql_and_moved_folder_are_searchable_after_rename() {
    let mut q = query("id", "SELECT 'café' FROM metrics");
    q.snippet.name = "Renamed weekly".into();
    q.metadata.folder = "Archive/Revenue".into();
    assert!(query_matches(&q, "RENAMED revenue café"));
    assert!(!query_matches(&q, "missing"));
}

#[test]
fn history_search_reaches_oldest_retained_entry_and_retention_remains_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    let mut state = AppState::default();
    for i in 0..1200 {
        state.history.push(HistoryEntry {
            id: i.to_string(),
            sql: format!("SELECT {i}"),
            connection_id: if i == 200 { "special" } else { "other" }.into(),
            executed_at: "2026-10-25T23:30:00-02:00".into(),
            error: (i == 200).then(|| "old failure".into()),
            ..Default::default()
        });
    }
    store.save(&state).unwrap();
    let state = store.load().unwrap();
    assert_eq!(state.history.len(), HISTORY_LIMIT);
    assert_eq!(state.history.first().unwrap().id, "200");
    let filter = HistoryFilter {
        text: "old failure".into(),
        connection_id: Some("special".into()),
        from: Some("2026-10-26".parse().unwrap()),
        through: Some("2026-10-26".parse().unwrap()),
        status: HistoryStatus::Failure,
    };
    let matches = history_matches(&state, &filter).collect::<Vec<_>>();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].id, "200");
    assert!(
        !HistoryFilter {
            status: HistoryStatus::Success,
            ..filter.clone()
        }
        .matches(matches[0])
    );
    assert!(
        !HistoryFilter {
            through: Some("2026-10-25".parse().unwrap()),
            ..filter.clone()
        }
        .matches(matches[0])
    );
}

#[test]
fn history_unknown_dates_are_available_without_date_filter() {
    let entry = HistoryEntry {
        executed_at: "legacy unknown".into(),
        ..Default::default()
    };
    assert!(HistoryFilter::default().matches(&entry));
    assert!(
        !HistoryFilter {
            from: Some("2020-01-01".parse().unwrap()),
            ..Default::default()
        }
        .matches(&entry)
    );
}

#[test]
fn external_modification_blocks_save_and_preserves_both_sql_versions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("query.sql");
    let document = create_sql_document(&path, "SELECT 'original'").unwrap();
    fs::write(&path, "SELECT 'external'").unwrap();
    let draft = "SELECT 'draft'";
    assert!(document.dirty(draft));
    assert!(
        save_sql_document(&document, draft)
            .unwrap_err()
            .to_string()
            .contains("externally")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "SELECT 'external'");
    let new_path = directory.path().join("draft.sql");
    let saved = create_sql_document(&new_path, draft).unwrap();
    assert!(!saved.dirty(draft));
    assert_eq!(fs::read_to_string(new_path).unwrap(), draft);
}

#[test]
fn recovery_retains_original_fingerprint_and_dirty_draft_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("query.sql");
    let document = create_sql_document(&path, "SELECT 1").unwrap();
    // Session recovery persists this binding alongside the editor's SQL, not disk SQL.
    let raw = serde_json::to_string(&(document, "SELECT 'unsaved λ'".to_string())).unwrap();
    fs::write(&path, "SELECT 'external after crash'").unwrap();
    let (restored, draft): (SqlDocument, String) = serde_json::from_str(&raw).unwrap();
    restored.validate().unwrap();
    assert_eq!(draft, "SELECT 'unsaved λ'");
    assert!(restored.dirty(&draft));
    assert!(save_sql_document(&restored, &draft).is_err());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "SELECT 'external after crash'"
    );
}

#[test]
fn file_save_refreshes_identity_and_stale_second_document_cannot_overwrite_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("query.sql");
    let document = create_sql_document(&path, "SELECT 1").unwrap();
    let second = open_sql_document(&path).unwrap().1;
    let saved = save_sql_document(&document, "SELECT 2").unwrap();
    assert!(!saved.dirty("SELECT 2"));
    assert!(saved.dirty("SELECT 3"));
    assert!(save_sql_document(&second, "SELECT 3").is_err());
    let saved = save_sql_document(&saved, "SELECT 4").unwrap();
    assert!(!saved.dirty("SELECT 4"));
    assert_eq!(fs::read_to_string(path).unwrap(), "SELECT 4");
}

#[test]
fn save_as_never_overwrites_existing_files_and_missing_files_are_conflicts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("query.sql");
    let document = create_sql_document(&path, "SELECT 1").unwrap();
    assert!(create_sql_document(&path, "SELECT 2").is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "SELECT 1");
    fs::remove_file(&path).unwrap();
    assert!(save_sql_document(&document, "SELECT 3").is_err());
    assert!(!path.exists());
}

#[test]
fn replacement_with_identical_bytes_is_still_an_external_change() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("query.sql");
    let document = create_sql_document(&path, "SELECT 1").unwrap();
    let replacement = directory.path().join("replacement.sql");
    fs::write(&replacement, "SELECT 1").unwrap();
    fs::rename(replacement, &path).unwrap();
    assert!(save_sql_document(&document, "SELECT 2").is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "SELECT 1");
}

#[test]
fn symbolic_links_non_utf8_and_oversized_documents_are_rejected() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("query.sql");
    fs::write(&path, "SELECT 1").unwrap();
    let link = directory.path().join("link.sql");
    symlink(&path, &link).unwrap();
    assert!(open_sql_document(&link).is_err());
    assert!(create_sql_document(&link, "overwrite").is_err());
    fs::write(&path, [0xff]).unwrap();
    assert!(open_sql_document(&path).is_err());
    fs::write(&path, vec![b'x'; SQL_DOCUMENT_LIMIT + 1]).unwrap();
    assert!(open_sql_document(&path).is_err());
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        (SQL_DOCUMENT_LIMIT + 1) as u64
    );
}
