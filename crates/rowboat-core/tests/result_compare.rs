use rowboat_core::{Cell, Column, QueryResult, result_compare::*};
use std::sync::atomic::AtomicBool;

fn result(names: &[(&str, &str)], rows: Vec<Vec<Cell>>) -> QueryResult {
    QueryResult {
        columns: names
            .iter()
            .map(|(name, ty)| Column {
                name: (*name).into(),
                data_type: (*ty).into(),
                nullable: true,
                primary_key: false,
                default_value: None,
            })
            .collect(),
        rows,
        ..Default::default()
    }
}
fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}
fn pair(left: usize, right: usize) -> ColumnPair {
    ColumnPair { left, right }
}
fn mapping() -> Mapping {
    Mapping {
        keys: vec![pair(0, 0)],
        columns: vec![pair(0, 0), pair(1, 1)],
    }
}
fn run(l: &QueryResult, r: &QueryResult, m: &Mapping) -> Result<Comparison, CompareError> {
    compare(l, r, m, [false; 2], usize::MAX, &AtomicBool::new(false))
}

#[test]
fn composite_keys_match_reordered_rows_and_renamed_reordered_columns() {
    let l = result(
        &[("tenant", "int"), ("id", "int"), ("amount", "numeric")],
        vec![
            vec![text("1"), text("2"), text("9007199254740993.0000000001")],
            vec![text("2"), text("2"), text("4")],
        ],
    );
    let r = result(
        &[
            ("new_amount", "NUMERIC"),
            ("new_id", "int"),
            ("tenant", "int"),
        ],
        vec![
            vec![text("5"), text("2"), text("2")],
            vec![text("9007199254740993.0000000001"), text("2"), text("1")],
        ],
    );
    let m = Mapping {
        keys: vec![pair(0, 2), pair(1, 1)],
        columns: vec![pair(2, 0)],
    };
    let out = run(&l, &r, &m).unwrap();
    assert_eq!(
        (out.unchanged, out.changed, out.added, out.removed),
        (1, 1, 0, 0)
    );
    assert_eq!(
        out.rows,
        vec![RowDifference {
            left: Some(1),
            right: Some(0),
            kind: DifferenceKind::Changed
        }]
    );
}
#[test]
fn exact_numerics_null_empty_text_and_binary_are_never_coerced() {
    let values = [
        Cell::Null,
        text(""),
        Cell::Binary(vec![]),
        text("9007199254740993.0000000001"),
        text("1.00"),
    ];
    let changed = [
        text("NULL"),
        Cell::Binary(vec![]),
        text(""),
        text("9007199254740993.0000000002"),
        text("1.0"),
    ];
    let rows = |v: &[Cell]| {
        v.iter()
            .enumerate()
            .map(|(i, c)| vec![text(&i.to_string()), c.clone()])
            .collect()
    };
    let l = result(&[("key", "text"), ("value", "text")], rows(&values));
    let r = result(&[("key", "text"), ("value", "text")], rows(&changed));
    let out = run(&l, &r, &mapping()).unwrap();
    assert_eq!(out.changed, 5);
    assert_eq!(l.rows[3][1], text("9007199254740993.0000000001"));
}
#[test]
fn null_keys_and_duplicate_composite_keys_are_explicit_on_both_sides() {
    let good = result(
        &[("a", "text"), ("b", "text")],
        vec![vec![text("a"), text("1")]],
    );
    let m = Mapping {
        keys: vec![pair(0, 0), pair(1, 1)],
        columns: vec![pair(0, 0)],
    };
    let null = result(
        &[("a", "text"), ("b", "text")],
        vec![vec![text("a"), Cell::Null]],
    );
    assert!(matches!(
        run(&null, &good, &m),
        Err(CompareError::NullKey {
            side: Side::Left,
            column: 1,
            ..
        })
    ));
    assert!(matches!(
        run(&good, &null, &m),
        Err(CompareError::NullKey {
            side: Side::Right,
            ..
        })
    ));
    let mut duplicate = good.clone();
    duplicate.rows.push(good.rows[0].clone());
    assert!(matches!(
        run(&duplicate, &good, &m),
        Err(CompareError::DuplicateKey {
            side: Side::Left,
            ..
        })
    ));
    assert!(matches!(
        run(&good, &duplicate, &m),
        Err(CompareError::DuplicateKey {
            side: Side::Right,
            ..
        })
    ));
}
#[test]
fn keys_have_unambiguous_component_boundaries_and_cell_variants() {
    let r = result(
        &[("a", "text"), ("b", "text")],
        vec![
            vec![text("ab"), text("c")],
            vec![text("a"), text("bc")],
            vec![text("a\0"), text("bc")],
            vec![Cell::Binary(vec![97]), text("bc")],
        ],
    );
    let m = Mapping {
        keys: vec![pair(0, 0), pair(1, 1)],
        columns: vec![pair(1, 1)],
    };
    assert_eq!(run(&r, &r, &m).unwrap().unchanged, 4);
}
#[test]
fn partial_results_never_claim_whole_result_equality() {
    let r = result(
        &[("id", "text"), ("value", "text")],
        vec![vec![text("1"), text("x")]],
    );
    let out = compare(
        &r,
        &r,
        &mapping(),
        [true, false],
        usize::MAX,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(out.partial && out.retained_rows_only);
    let mut truncated = r.clone();
    truncated.truncated = true;
    assert!(run(&r, &truncated, &mapping()).unwrap().partial);
    assert!(run(&r, &r, &mapping()).unwrap().retained_rows_only);
}
#[test]
fn mapped_values_only_and_added_removed_rows_are_reported() {
    let l = result(
        &[("id", "text"), ("value", "text")],
        vec![
            vec![text("1"), text("old")],
            vec![text("2"), text("removed")],
        ],
    );
    let r = result(
        &[("id", "text"), ("value", "text")],
        vec![vec![text("3"), text("added")], vec![text("1"), text("new")]],
    );
    let out = run(&l, &r, &mapping()).unwrap();
    assert_eq!((out.changed, out.added, out.removed), (1, 1, 1));
    let mut m = mapping();
    m.columns = vec![pair(0, 0)];
    assert_eq!(run(&l, &r, &m).unwrap().unchanged, 1);
}
#[test]
fn incompatible_types_duplicate_mappings_and_malformed_rows_fail_closed() {
    let l = result(
        &[("id", "text"), ("value", "text")],
        vec![vec![text("1"), text("x")]],
    );
    let mut r = l.clone();
    r.columns[1].data_type = "numeric".into();
    assert!(matches!(
        run(&l, &r, &mapping()),
        Err(CompareError::Mapping(_))
    ));
    let mut m = mapping();
    m.columns.push(pair(1, 1));
    assert!(matches!(run(&l, &l, &m), Err(CompareError::Mapping(_))));
    m.keys.clear();
    assert!(run(&l, &l, &m).is_err());
    r = l.clone();
    r.rows[0].pop();
    assert!(matches!(
        run(&l, &r, &mapping()),
        Err(CompareError::RowWidth {
            side: Side::Right,
            row: 0
        })
    ));
}
#[test]
fn budget_is_checked_before_allocating_and_includes_output() {
    let r = result(
        &[("id", "text"), ("value", "text")],
        vec![vec![text("1"), text("x")]],
    );
    let bytes = comparison_bytes(1, 1);
    assert!(
        matches!(compare(&r,&r,&mapping(),[false;2],bytes-1,&AtomicBool::new(false)),Err(CompareError::Budget{required,..}) if required==bytes)
    );
    assert!(
        compare(
            &r,
            &r,
            &mapping(),
            [false; 2],
            bytes,
            &AtomicBool::new(false)
        )
        .is_ok()
    );
    assert_eq!(comparison_bytes(usize::MAX, 1), usize::MAX);
    let mut grown = r.clone();
    grown.rows.reserve(100);
    grown.rows[0].reserve(100);
    assert!(result_bytes(&grown) > result_bytes(&r));
}
#[test]
fn cancellation_interrupts_sorting_and_returns_no_partial_comparison() {
    let r = result(
        &[("id", "text"), ("value", "text")],
        (0..100000)
            .rev()
            .map(|i| vec![text(&i.to_string()), text("x")])
            .collect(),
    );
    let cancel = AtomicBool::new(true);
    assert_eq!(
        compare(&r, &r, &mapping(), [false; 2], usize::MAX, &cancel).unwrap_err(),
        CompareError::Cancelled
    );
    let during_sort = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let cancel = &during_sort;
        scope.spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        assert_eq!(
            compare(&r, &r, &mapping(), [false; 2], usize::MAX, cancel).unwrap_err(),
            CompareError::Cancelled
        );
    });
}
