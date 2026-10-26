use rowboat_admin::metrics::{CustomMetricState, decode_custom_sample};
use rowboat_core::{Cell, Column, QueryResult};

fn result(cell: Cell) -> QueryResult {
    QueryResult {
        columns: vec![Column {
            name: "metric".into(),
            data_type: "NUMERIC".into(),
            nullable: true,
            primary_key: false,
            default_value: None,
        }],
        rows: vec![vec![cell]],
        ..Default::default()
    }
}

#[test]
fn decimal_metrics_preserve_precision_sign_scale_and_extreme_finite_exponents() {
    for value in [
        "18446744073709551615",
        "9007199254740993.000000000000000000000000000001",
        "-0.00000000000000000000000000000100",
        "1e100000",
        "1e-100000",
        "+.125",
        "-0.00",
        "10.",
    ] {
        let sample = decode_custom_sample(&[result(Cell::Text(value.into()))], 123);
        assert_eq!(sample.sampled_at_unix_ms, 123);
        assert_eq!(
            sample.state,
            CustomMetricState::Value {
                decimal: value.into()
            }
        );
    }
}

#[test]
fn nonfinite_nonnumeric_and_unbounded_decimals_are_invalid_not_zero() {
    let mut values: Vec<String> = [
        "NaN",
        "Infinity",
        "-inf",
        "",
        " ",
        " 1",
        "1\n",
        "0x10",
        "1,000",
        "true",
        "１２",
        "+",
        ".",
        "1e",
        "1e+",
        "1e100001",
        "1e-100001",
        "1e999999999999999999999999999",
        "1.2.3",
        "1e1e1",
        "1\0",
        "1_000",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    values.push("1".repeat(1025));
    for value in values {
        let sample = decode_custom_sample(&[result(Cell::Text(value.clone()))], 0);
        assert!(
            matches!(sample.state, CustomMetricState::Invalid { .. }),
            "accepted {value:?}"
        );
    }
    for cell in [Cell::Null, Cell::Binary(vec![49])] {
        assert!(matches!(
            decode_custom_sample(&[result(cell)], 0).state,
            CustomMetricState::Invalid { .. }
        ));
    }
}

#[test]
fn empty_is_distinct_from_null_and_malformed_or_truncated_result_shapes() {
    let scalar = result(Cell::Text("0".into()));
    let mut empty = scalar.clone();
    empty.rows.clear();
    assert_eq!(
        decode_custom_sample(&[empty.clone()], 0).state,
        CustomMetricState::Empty
    );
    let mut cases = vec![vec![], vec![scalar.clone(), scalar.clone()]];
    empty.truncated = true;
    cases.push(vec![empty]);
    for mutate in [
        (|r: &mut QueryResult| r.rows.push(r.rows[0].clone())) as fn(&mut QueryResult),
        |r| r.columns.push(r.columns[0].clone()),
        |r| r.columns.clear(),
        |r| r.rows[0].clear(),
        |r| r.rows[0].push(Cell::Null),
        |r| r.truncated = true,
    ] {
        let mut malformed = scalar.clone();
        mutate(&mut malformed);
        cases.push(vec![malformed]);
    }
    for raw in cases {
        assert!(matches!(
            decode_custom_sample(&raw, 0).state,
            CustomMetricState::Invalid { .. }
        ));
    }
}
