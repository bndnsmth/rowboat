use anyhow::Result;
use rowboat_admin::{
    access, access_matrix,
    ddl::{self, DataType, DdlPlan, DefaultValue, PlannedStatement},
    explain, live_diff, metrics, object_editor,
    objects::{self, NamespaceOperation, ObjectKind},
    providers::{self, Atomicity, Operation, UnsupportedOperation},
    query,
};
use rowboat_core::{Cell, Engine, QueryResult, QuerySession};
use std::sync::Mutex;

struct NoQueries(Engine);
impl QuerySession for NoQueries {
    fn engine(&self) -> Engine {
        self.0
    }
    async fn query(&self, _: &str, _: usize) -> Result<Vec<QueryResult>> {
        panic!("Unsupported/mismatched provider attempted a query")
    }
}

fn unsupported<T: std::fmt::Debug>(result: Result<T>) {
    let error = result.unwrap_err();
    let reason = error
        .downcast_ref::<UnsupportedOperation>()
        .unwrap_or_else(|| panic!("Expected typed unsupported error, got {error:#}"));
    assert_eq!(reason.engine, Engine::Sqlite);
    assert!(!reason.reason.is_empty());
}

#[tokio::test]
async fn sqlite_optional_tools_reject_before_any_query() {
    let db = NoQueries(Engine::Sqlite);
    unsupported(objects::connection_info(&db).await);
    unsupported(objects::namespaces(&db).await);
    unsupported(objects::schemas(&db).await);
    unsupported(objects::server_activity(&db).await);
    unsupported(metrics::sample(&db).await);
    unsupported(metrics::sample_dashboard(&db).await);
    unsupported(explain::analyze(&db, "SELECT 1", explain::AnalyzeIntent::ExecuteQuery).await);
    unsupported(live_diff::snapshot_schema(&db, "main").await);
    unsupported(
        access_matrix::list_principals(
            &db,
            access_matrix::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await,
    );
    let principal = access_matrix::Principal::Postgres("role".into());
    unsupported(access::inspect_account(&db, &principal).await);
    unsupported(access::inspect_grants(&db, &principal).await);
    let object = access_matrix::AccessObject::Namespace("main".into());
    let request = access_matrix::AccessRequest {
        principal,
        objects: vec![object.clone()],
        context: access_matrix::AccessContext::Principal,
    };
    // In particular, unsupported must not be caught as a success-shaped report
    // whose rows merely say Unknown (the valid-provider permission-error policy).
    unsupported(access_matrix::inspect_access(&db, &request).await);
    unsupported(
        access_matrix::preview_change(
            &db,
            &request,
            &access_matrix::AccessChange {
                object,
                privileges: vec![access::Privilege::Select],
                revoke: false,
                grant_option: false,
            },
        )
        .await,
    );
    for kind in [
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Trigger,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Role,
        ObjectKind::Sequence,
    ] {
        unsupported(objects::browse_objects(&db, kind).await);
        unsupported(object_editor::list_objects(&db, "main", kind).await);
        unsupported(
            object_editor::inspect_object(
                &db,
                &object_editor::ObjectIdentity {
                    engine: Engine::Sqlite,
                    schema: "main".into(),
                    name: "obj".into(),
                    kind,
                    signature: None,
                    parent: None,
                },
            )
            .await,
        );
    }
}

#[test]
fn sqlite_server_only_builders_remain_unavailable() {
    unsupported(objects::namespace_sql(
        Engine::Sqlite,
        &NamespaceOperation::CreateSchema { name: "x".into() },
    ));
    unsupported(query::analyze_sql(Engine::Sqlite, "SELECT 1"));
    assert!(explain::decode(Engine::Sqlite, "not even JSON").is_err());
    unsupported(explain::decode_analyze(Engine::Sqlite, "not a native plan"));
    unsupported(metrics::decode_sample(Engine::Sqlite, vec![], 0));
    unsupported(access_matrix::principal(Engine::Sqlite, "".into(), None));
    let grant = access::GrantOperation {
        target: access::GrantTarget::Schema("main".into()),
        grantee: access::Grantee::PgRole("role".into()),
        privileges: vec![access::Privilege::Select],
        revoke: false,
        grant_option: false,
    };
    unsupported(access::grant_sql(Engine::Sqlite, &grant));
    unsupported(access::grant_sql_with_mysql_mode(
        Engine::Sqlite,
        &grant,
        access::MySqlDatabaseGrantMode::LiteralNames,
    ));
    unsupported(access::column_grant_sql(Engine::Sqlite, &grant, &[]));
    unsupported(rowboat_admin::diff::compare(Engine::Sqlite, &[], &[]));
    assert!(providers::operation_support(Engine::Sqlite, Operation::SchemaComparison).is_err());
}

#[test]
fn sqlite_native_tools_have_narrow_transactional_capabilities() {
    assert!(ddl::preview(Engine::Sqlite, &[]).unwrap().is_empty());
    assert_eq!(
        DataType::parse(Engine::Sqlite, "integer").unwrap(),
        DataType::Integer
    );
    assert_eq!(DataType::Integer.sql(Engine::Sqlite).unwrap(), "INTEGER");
    assert_eq!(
        DefaultValue::Literal(Cell::Null)
            .sql(Engine::Sqlite)
            .unwrap(),
        "NULL"
    );
    assert!(
        query::explain_sql(Engine::Sqlite, "SELECT 1")
            .unwrap()
            .starts_with("EXPLAIN QUERY PLAN")
    );
    let semantics = providers::operation_support(Engine::Sqlite, Operation::SchemaDdl).unwrap();
    assert_eq!(semantics.atomicity, Atomicity::Transactional);
    assert!(semantics.confirmation_required);
    assert!(!semantics.implicit_commit && !semantics.rebuild_supported);
    assert_eq!(
        DdlPlan::default()
            .execution_semantics(Engine::Sqlite, Operation::SchemaDdl)
            .unwrap()
            .atomicity,
        Atomicity::ReadOnly
    );
}

#[test]
fn consumers_can_use_descriptors_and_plan_semantics_without_engine_arms() {
    assert_eq!(
        providers::descriptor(Engine::Sqlite).tools.administration,
        Engine::Sqlite.descriptor().tools.administration
    );
    let reason =
        providers::operation_support(Engine::Sqlite, Operation::ServerMetrics).unwrap_err();
    let rowboat_core::ToolSupport::Unsupported(core_reason) =
        Engine::Sqlite.descriptor().tools.metrics
    else {
        panic!("SQLite fixture must have no metrics provider");
    };
    assert_eq!(reason.reason, core_reason);
    for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
        for kind in providers::supported_object_kinds(engine) {
            assert!(providers::operation_support(engine, Operation::ObjectCatalog(*kind)).is_ok());
            assert!(objects::object_sql(engine, *kind).is_ok());
        }
    }
    assert!(providers::supported_object_kinds(Engine::Sqlite).is_empty());
    assert!(
        providers::operation_support(
            Engine::MySql,
            Operation::ObjectCatalog(ObjectKind::Sequence)
        )
        .is_err()
    );
    let mut plan = DdlPlan {
        statements: vec![PlannedStatement {
            sql: "ALTER TABLE t ADD COLUMN x INTEGER;".into(),
            destructive: false,
            reason: "test".into(),
        }],
        warnings: vec![],
    };
    let pg = plan
        .execution_semantics(Engine::Postgres, Operation::SchemaDdl)
        .unwrap();
    assert_eq!(pg.atomicity, Atomicity::Transactional);
    assert!(!pg.implicit_commit);
    assert!(pg.confirmation_required && pg.rebuild_supported);
    let mysql = plan
        .execution_semantics(Engine::MySql, Operation::SchemaDdl)
        .unwrap();
    assert_eq!(mysql.atomicity, Atomicity::NonTransactional);
    assert!(mysql.implicit_commit && mysql.confirmation_required);
    plan.statements[0].sql = "CREATE DATABASE x;".into();
    assert_eq!(
        plan.execution_semantics(Engine::Postgres, Operation::SchemaDdl)
            .unwrap()
            .atomicity,
        Atomicity::NonTransactional
    );
    assert!(plan.review(Engine::Postgres).requires_outside_transaction);
}

struct Recording {
    engine: Engine,
    queries: Mutex<Vec<String>>,
}
impl QuerySession for Recording {
    fn engine(&self) -> Engine {
        self.engine
    }
    async fn query(&self, sql: &str, _: usize) -> Result<Vec<QueryResult>> {
        self.queries.lock().unwrap().push(sql.into());
        Ok(vec![QueryResult::default()])
    }
}

#[tokio::test]
async fn principal_facade_preserves_validation_output_order_and_supplied_session() {
    assert!(
        access_matrix::principal(Engine::Postgres, "role".into(), Some("host".into())).is_err()
    );
    assert!(access_matrix::principal(Engine::MySql, "user".into(), None).is_err());
    let pg = access_matrix::principal(Engine::Postgres, "role".into(), None).unwrap();
    let mysql =
        access_matrix::principal(Engine::MySql, "user".into(), Some("localhost".into())).unwrap();
    assert!(
        access::inspect_grants(&NoQueries(Engine::Postgres), &mysql)
            .await
            .is_err()
    );
    assert!(
        access::inspect_account(&NoQueries(Engine::MySql), &pg)
            .await
            .is_err()
    );
    let session = Recording {
        engine: Engine::Postgres,
        queries: Mutex::new(vec![]),
    };
    assert_eq!(
        access::inspect_grants(&session, &pg).await.unwrap().len(),
        2
    );
    let queries = session.queries.lock().unwrap();
    assert_eq!(queries.len(), 2);
    assert!(queries[0].contains("aclexplode(o.acl)"));
    assert!(
        queries[0].starts_with("SELECT "),
        "Lease inspection accepts SELECT/SHOW only"
    );
    assert!(queries[1].contains("pg_auth_members"));
}

#[test]
fn public_orchestration_does_not_select_native_sql_by_engine() {
    for (name, source) in [
        ("objects", include_str!("../src/objects.rs")),
        ("object_editor", include_str!("../src/object_editor.rs")),
        ("metrics", include_str!("../src/metrics.rs")),
        ("explain", include_str!("../src/explain.rs")),
        ("ddl", include_str!("../src/ddl.rs")),
        ("access", include_str!("../src/access.rs")),
        ("access_matrix", include_str!("../src/access_matrix.rs")),
    ] {
        for forbidden in [
            "Engine::Postgres",
            "Engine::MySql",
            "Engine::Sqlite",
            "pg_catalog.",
            "information_schema.",
        ] {
            assert!(
                !source.contains(forbidden),
                "{name} owns provider detail {forbidden}"
            );
        }
    }
    let provider = include_str!("../src/providers/objects.rs");
    for forbidden in [
        "administrative_operation(",
        "Database::connect",
        "Command::new",
    ] {
        assert!(
            !provider.contains(forbidden),
            "Object provider acquired its own execution context"
        );
    }
}

#[test]
fn sqlite_editor_literals_follow_sqlite_quote_and_comment_rules() {
    let values = [("v".into(), Cell::Text("actual".into()))]
        .into_iter()
        .collect();
    let sql = "SELECT [a:v], `b:v`, \":v\", ':v' --:v\r:v\n, :v /*:v*/";
    let expanded = query::expand_named(Engine::Sqlite, sql, &values).unwrap();
    assert!(expanded.contains("[a:v], `b:v`, \":v\", ':v' --:v\r:v\n"));
    let literal = rowboat_core::sql::literal(Engine::Sqlite, &Cell::Text("actual".into()));
    assert_eq!(expanded.matches(&literal).count(), 1);
    assert!(expanded.ends_with(" /*:v*/"));
}
