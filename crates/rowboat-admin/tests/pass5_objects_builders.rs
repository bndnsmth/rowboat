use rowboat_admin::ddl::*;
use rowboat_core::Cell;
fn base() -> MySqlColumnDefinition {
    MySqlColumnDefinition {
        column: ColumnDesign {
            name: "g".into(),
            data_type: DataType::Integer,
            nullable: true,
            default: None,
        },
        unsigned: false,
        auto_increment: false,
        character_set: None,
        collation: None,
        comment: "preserved".into(),
        on_update_current_timestamp_precision: None,
        invisible: false,
    }
}
#[test]
fn pass5_generated_column_rejects_hidden_statements_and_incompatible_attributes() {
    let mut d = AdvancedMySqlColumnDefinition {
        base: base(),
        specialized_type: None,
        generation: Some(MySqlGeneration {
            expression: "qty * 2".into(),
            storage: GeneratedStorage::Stored,
        }),
    };
    assert!(
        d.sql()
            .unwrap()
            .contains("GENERATED ALWAYS AS (qty * 2) STORED")
    );
    for expression in [
        "qty); DROP TABLE t; --",
        "qty /* hidden */",
        "qty + (1",
        "'ambiguous\\quote'",
    ] {
        d.generation.as_mut().unwrap().expression = expression.into();
        assert!(d.sql().is_err());
    }
    d.generation.as_mut().unwrap().expression = "qty + 1".into();
    d.base.column.default = Some(DefaultValue::Literal(Cell::Text("1".into())));
    assert!(d.sql().is_err());
    d.base.column.default = None;
    d.base.auto_increment = true;
    d.base.column.nullable = false;
    assert!(d.sql().is_err());
}
#[test]
fn pass5_specialized_types_preserve_case_literals_and_reject_unsafe_combinations() {
    let kind = MySqlSpecialType::Enum(vec!["Small".into(), "LARGE".into(), "owner's".into()]);
    let mut b = base();
    b.column.data_type = kind.base_type();
    b.column.default = Some(DefaultValue::Literal(Cell::Text("Small".into())));
    b.character_set = Some("utf8mb4".into());
    b.collation = Some("utf8mb4_bin".into());
    let mut d = AdvancedMySqlColumnDefinition {
        base: b,
        specialized_type: Some(kind),
        generation: None,
    };
    let sql = d.sql().unwrap();
    assert!(sql.contains("ENUM('Small', 'LARGE', 'owner''s')"));
    assert!(sql.contains("DEFAULT 'Small'"));
    assert!(sql.contains("COLLATE `utf8mb4_bin`"));
    d.specialized_type = Some(MySqlSpecialType::Enum(vec!["bad\\value".into()]));
    assert!(d.sql().is_err());
    d.specialized_type = Some(MySqlSpecialType::Set(vec!["same".into(), "same".into()]));
    assert!(d.sql().is_err());
    d.specialized_type = Some(MySqlSpecialType::DateTime { precision: 7 });
    d.base.column.data_type = d.specialized_type.as_ref().unwrap().base_type();
    d.base.character_set = None;
    d.base.collation = None;
    assert!(d.sql().is_err());
}
