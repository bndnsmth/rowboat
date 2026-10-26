//! Client-side ordering for retained query results. Table browsing should prefer
//! server ORDER BY, which respects server collation and datatype semantics.
use crate::Cell;
use sqlx::types::BigDecimal;
use std::{cmp::Ordering, str::FromStr};

fn numeric_type(data_type: &str) -> bool {
    let normalized = data_type.trim().to_ascii_uppercase();
    let base = normalized.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "INT"
            | "INT2"
            | "INT4"
            | "INT8"
            | "INTEGER"
            | "SMALLINT"
            | "BIGINT"
            | "TINYINT"
            | "MEDIUMINT"
            | "DECIMAL"
            | "NUMERIC"
            | "FLOAT"
            | "FLOAT4"
            | "FLOAT8"
            | "DOUBLE"
            | "REAL"
            | "SERIAL"
            | "BIGSERIAL"
            | "SMALLSERIAL"
            | "YEAR"
    )
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
enum Numeric<'a> {
    NegativeInfinity,
    Finite(BigDecimal),
    PositiveInfinity,
    Nan,
    Invalid(&'a str),
}

fn number(text: &str) -> Numeric<'_> {
    let input = text.trim();
    if input.eq_ignore_ascii_case("-infinity") || input.eq_ignore_ascii_case("-inf") {
        return Numeric::NegativeInfinity;
    }
    if ["inf", "+inf", "infinity", "+infinity"]
        .iter()
        .any(|v| input.eq_ignore_ascii_case(v))
    {
        return Numeric::PositiveInfinity;
    }
    if input.eq_ignore_ascii_case("nan") {
        return Numeric::Nan;
    }
    // Cap synthetically large exponents before BigDecimal comparisons allocate
    // powers of ten; server numeric ranges are far smaller than this bound.
    let exponent_ok = input.find(['e', 'E']).is_none_or(|at| {
        input[at + 1..]
            .parse::<i64>()
            .is_ok_and(|v| v.unsigned_abs() <= 200_000)
    });
    if exponent_ok && let Ok(value) = BigDecimal::from_str(input) {
        Numeric::Finite(value)
    } else {
        Numeric::Invalid(text)
    }
}

/// Ascending local order: SQL NULL first, text next, binary last. Numeric SQL
/// types compare exact decimal values, never f64. Numeric NaN follows infinity;
/// malformed numeric display values follow all valid numbers in text order.
/// Other text uses Unicode byte order, not database locale/collation ordering.
pub fn compare_cells(data_type: &str, left: &Cell, right: &Cell) -> Ordering {
    match (left, right) {
        (Cell::Null, Cell::Null) => Ordering::Equal,
        (Cell::Null, _) => Ordering::Less,
        (_, Cell::Null) => Ordering::Greater,
        (Cell::Text(a), Cell::Text(b)) if numeric_type(data_type) => number(a).cmp(&number(b)),
        (Cell::Text(a), Cell::Text(b)) => a.cmp(b),
        (Cell::Binary(a), Cell::Binary(b)) => a.cmp(b),
        (Cell::Text(_), Cell::Binary(_)) => Ordering::Less,
        (Cell::Binary(_), Cell::Text(_)) => Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::compare_cells;
    use crate::Cell;
    use std::cmp::Ordering;

    fn text(value: &str) -> Cell {
        Cell::Text(value.into())
    }

    #[test]
    fn exact_numeric_order_preserves_large_integers_and_decimal_precision() {
        for ty in [
            "INT8",
            "BIGINT UNSIGNED",
            "NUMERIC(65,30)",
            "DECIMAL",
            "DOUBLE PRECISION",
        ] {
            assert_eq!(compare_cells(ty, &text("2"), &text("10")), Ordering::Less);
            assert_eq!(
                compare_cells(
                    ty,
                    &text("18446744073709551614"),
                    &text("18446744073709551615")
                ),
                Ordering::Less
            );
            assert_eq!(
                compare_cells(
                    ty,
                    &text("0.100000000000000000000000000001"),
                    &text("0.100000000000000000000000000002")
                ),
                Ordering::Less
            );
            assert_eq!(
                compare_cells(ty, &text("1e2"), &text("100.000")),
                Ordering::Equal
            );
            assert_eq!(compare_cells(ty, &text("-10"), &text("-2")), Ordering::Less);
        }
        assert_eq!(
            compare_cells("VARCHAR", &text("2"), &text("10")),
            Ordering::Greater
        );
    }

    #[test]
    fn null_binary_special_values_and_invalid_numbers_have_total_order() {
        let values = [
            Cell::Null,
            text("-inf"),
            text("-3.5"),
            text("0"),
            text("10"),
            text("inf"),
            text("NaN"),
            text("invalid"),
            Cell::Binary(vec![0]),
            Cell::Binary(vec![255]),
        ];
        for (i, left) in values.iter().enumerate() {
            for (j, right) in values.iter().enumerate() {
                assert_eq!(compare_cells("NUMERIC", left, right), i.cmp(&j));
            }
        }
        assert_eq!(
            compare_cells("NUMERIC", &text("1e999999999"), &text("2e999999999")),
            Ordering::Less
        );
    }
}
