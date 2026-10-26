use anyhow::{Context, Result, ensure};
use rowboat_admin::{
    live_diff::{NamespaceMapping, definitions::*},
    object_editor::resolve_object,
    objects::ObjectKind,
};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine};
use std::time::{SystemTime, UNIX_EPOCH};
async fn connect() -> Result<Database> {
    Database::connect(
        &ConnectionProfile {
            engine: Engine::Postgres,
            host: "127.0.0.1".into(),
            port: 15435,
            username: "postgres".into(),
            database: "rowboat".into(),
            ssl_mode: "disable".into(),
            ..Default::default()
        },
        "rowboat-test",
    )
    .await
}
async fn exec(db: &Database, sql: &str) -> Result<()> {
    db.query(sql, 10).await?;
    Ok(())
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    let r = db.query(sql, 1).await?;
    match &r[0].rows[0][0] {
        Cell::Text(s) => Ok(s.clone()),
        _ => anyhow::bail!("Expected textual scalar"),
    }
}
fn mapping() -> NamespaceMapping {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    NamespaceMapping {
        source: format!("rb6sa{n}a"),
        destination: format!("rb6sa{n}b"),
    }
}
async fn create(db: &Database, m: &NamespaceMapping) -> Result<()> {
    exec(
        db,
        &format!(
            "CREATE SCHEMA {}; CREATE SCHEMA {}",
            m.source, m.destination
        ),
    )
    .await
}
async fn cleanup(db: &Database, m: &NamespaceMapping) -> Result<()> {
    exec(
        db,
        &format!(
            "SET search_path=pg_catalog; DROP SCHEMA {} CASCADE; DROP SCHEMA {} CASCADE",
            m.source, m.destination
        ),
    )
    .await
}
async fn compare(
    a: &Database,
    b: &Database,
    m: &NamespaceMapping,
    name: &str,
) -> Result<DefinitionComparison> {
    let source = resolve_object(a, &m.source, ObjectKind::Function, name, None, None).await?;
    let destination =
        resolve_object(b, &m.destination, ObjectKind::Function, name, None, None).await?;
    compare_definitions(a, &source, b, &destination, m, Scope::RoutineBody).await
}

#[tokio::test]
#[ignore = "Parent-owned Rowboat PG15435"]
async fn pass6_quoted_and_positional_arguments_keep_native_sql_and_plpgsql_meaning() -> Result<()> {
    let a = connect().await?;
    let b = connect().await?;
    let m = mapping();
    create(&a, &m).await?;
    let result:Result<()>=async {
        for language in ["sql","plpgsql"] {
            for (case,args,expression) in [
                ("quoted","\"$2\" integer,y integer","\"$2\""),
                ("mixed","\"$2\" integer,\"$1\" integer","\"$2\"*100+$2*10+\"$1\"+$1"),
                ("named_high","\"$99\" integer,y integer","\"$99\""),
                ("positional","integer,integer","$2"),
            ] {
                let name=format!("{language}_{case}");
                // PL/pgSQL itself rejects a declared dollar-name colliding with
                // any generated positional alias. Exercise its legal named
                // aliases and out-of-range dollar names, while SQL tests the
                // reviewer-reproduced cross-position collisions directly.
                let args=if language=="plpgsql"&&case=="quoted"{"\"$2\" integer"}else if language=="plpgsql"&&case=="mixed"{"\"$99\" integer,\"$98\" integer"}else{args};
                let expression=if language=="plpgsql"&&case=="mixed"{"\"$99\"*100+$2*10+\"$98\"+$1"}else{expression};
                let values=if language=="plpgsql"&&case=="quoted"{"1"}else{"1,10"};
                for (schema,n) in [(&m.source,1),(&m.destination,0)] {
                    let body=if language=="sql"{format!("SELECT {expression}+{n}")}else{format!("BEGIN RETURN {expression}+{n}; END")};
                    exec(&a,&format!("CREATE FUNCTION {schema}.{name}({args}) RETURNS integer LANGUAGE {language} AS $b${body}$b$")).await.with_context(||format!("Creating native {language}/{case}"))?;
                }
                let native=scalar(&a,&format!("SELECT {}.{name}({values})",m.source)).await?;
                if (case=="quoted"&&language=="sql")||case=="named_high"{ensure!(native=="2","Native named argument reproduction differs: {language}/{case}={native}");}
                if case=="quoted"&&language=="plpgsql"{ensure!(native=="2");}
                if case=="positional"{ensure!(native=="11");}
                let review=compare(&a,&b,&m,&name).await?;
                ensure!(!review.is_blocked(),"{language}/{case}: {:?}",review.blockers());
                let applied=apply_definition_comparison(&a,&b,&review).await?;
                ensure!(applied.status==DefinitionApplyStatus::Applied,"{applied:?}");
                ensure!(scalar(&b,&format!("SELECT {}.{name}({values})",m.destination)).await?==native,"Named and positional parameters changed meaning: {language}/{case}");
                let converged=compare(&a,&b,&m,&name).await?;
                ensure!(!converged.is_blocked()&&converged.plan().is_empty(),"{language}/{case} did not converge");
            }
        }
        Ok(())
    }.await;
    cleanup(&a, &m).await?;
    result
}

#[tokio::test]
#[ignore = "Parent-owned Rowboat PG15435"]
async fn pass6_custom_operator_implementations_block_and_invalidate_both_routine_languages()
-> Result<()> {
    let a = connect().await?;
    let b = connect().await?;
    let m = mapping();
    create(&a, &m).await?;
    let result:Result<()>=async {
        // Both search paths are catalog-first. An exact custom (integer,text)
        // overload still resolves because pg_catalog has no matching overload.
        for (db,schema,label) in [(&a,&m.source,"source"),(&b,&m.destination,"destination")] {
            exec(db,&format!("SET search_path=pg_catalog,{schema}; CREATE FUNCTION {schema}.custom_add(integer,text) RETURNS text LANGUAGE sql IMMUTABLE AS $b$SELECT '{label}:' || $2$b$; CREATE OPERATOR {schema}.+ (LEFTARG=integer,RIGHTARG=text,FUNCTION={schema}.custom_add)")).await?;
            for language in ["sql","plpgsql"] {
                let expression=if label=="source"{"x+y"}else{"y"};
                let body=if language=="sql"{format!("SELECT {expression}")}else{format!("BEGIN RETURN {expression}; END")};
                exec(db,&format!("CREATE FUNCTION {schema}.op_{language}(x integer,y text) RETURNS text LANGUAGE {language} AS $b${body}$b$")).await?;
            }
        }
        for language in ["sql","plpgsql"] {
            let name=format!("op_{language}");
            ensure!(scalar(&a,&format!("SELECT {}.{name}(1,'value')",m.source)).await?=="source:value");
            let review=compare(&a,&b,&m,&name).await?;
            ensure!(review.is_blocked()&&review.plan().is_empty()&&review.blockers().iter().any(|s|s.contains("operators")),"Custom operator accepted: {:?}",review.blockers());
            let label=format!("changed_{language}");
            exec(&a,&format!("CREATE OR REPLACE FUNCTION {}.custom_add(integer,text) RETURNS text LANGUAGE sql IMMUTABLE AS $b$SELECT '{label}:' || $2$b$",m.source)).await?;
            let fresh=definitions_are_current(&a,&b,&review).await?;
            ensure!(!fresh.source&&fresh.destination,"Changed operator implementation was falsely fresh: {fresh:?}");
            ensure!(scalar(&a,&format!("SELECT {}.{name}(1,'value')",m.source)).await?==format!("{label}:value"));
            ensure!(apply_definition_comparison(&a,&b,&review).await?.status==DefinitionApplyStatus::Blocked);
            ensure!(scalar(&b,&format!("SELECT {}.{name}(1,'value')",m.destination)).await?=="value","Blocked comparison mutated destination");
            exec(&a,&format!("CREATE OR REPLACE FUNCTION {}.custom_add(integer,text) RETURNS text LANGUAGE sql IMMUTABLE AS $b$SELECT 'source:' || $2$b$",m.source)).await?;
        }
        // An operator not yet present at review time must invalidate a formerly
        // accepted review; it is not enough to reject only the known bad symbol.
        exec(&a,&format!("DROP OPERATOR {}.+ (integer,text)",m.source)).await?;
        exec(&b,&format!("DROP OPERATOR {}.+ (integer,text)",m.destination)).await?;
        for (schema,n) in [(&m.source,2),(&m.destination,1)]{exec(&a,&format!("CREATE FUNCTION {schema}.safe(x integer) RETURNS integer LANGUAGE sql AS $b$SELECT x+{n}$b$")).await?;}
        let review=compare(&a,&b,&m,"safe").await?;ensure!(!review.is_blocked(),"{:?}",review.blockers());
        exec(&b,&format!("CREATE OPERATOR {}.* (LEFTARG=integer,RIGHTARG=text,FUNCTION={}.custom_add)",m.destination,m.destination)).await?;
        ensure!(apply_definition_comparison(&a,&b,&review).await?.status==DefinitionApplyStatus::StaleDestination);
        let blocked=compare(&a,&b,&m,"safe").await?;ensure!(blocked.is_blocked()&&blocked.plan().is_empty(),"Non-plus custom overload was ignored");
        ensure!(scalar(&b,&format!("SELECT {}.safe(10)",m.destination)).await?=="11");
        Ok(())
    }.await;
    cleanup(&a, &m).await?;
    result
}

#[tokio::test]
#[ignore = "Parent-owned Rowboat PG15435"]
async fn pass6_implicit_casts_outside_search_path_are_not_treated_as_builtin_proof() -> Result<()> {
    let a = connect().await?;
    let b = connect().await?;
    let m = mapping();
    create(&a, &m).await?;
    let result:Result<()>=async {
        // The custom type/cast is unique to this fixture. It does not change
        // built-in casts used by other test sessions, yet proves that the cast
        // guard is database-wide (cast lookup does not follow search_path).
        for (db,schema,n) in [(&a,&m.source,2),(&b,&m.destination,1)] {
            exec(db,&format!("SET search_path=pg_catalog; CREATE FUNCTION {schema}.f(x integer) RETURNS integer LANGUAGE sql AS $b$SELECT x+{n}$b$")).await?;
        }
        let review=compare(&a,&b,&m,"f").await?;ensure!(!review.is_blocked(),"{:?}",review.blockers());
        exec(&a,&format!("CREATE TYPE {}.cast_source AS ENUM ('a'); CREATE FUNCTION {}.cast_text({}.cast_source) RETURNS text LANGUAGE sql IMMUTABLE AS $b$SELECT 'first'$b$; CREATE CAST ({}.cast_source AS text) WITH FUNCTION {}.cast_text({}.cast_source) AS IMPLICIT",m.source,m.source,m.source,m.source,m.source,m.source)).await?;
        ensure!(apply_definition_comparison(&a,&b,&review).await?.status==DefinitionApplyStatus::StaleSource);
        let blocked=compare(&a,&b,&m,"f").await?;
        ensure!(blocked.is_blocked()&&blocked.plan().is_empty()&&blocked.blockers().iter().any(|s|s.contains("casts")),"Unproven implicit casts accepted");
        exec(&a,&format!("CREATE OR REPLACE FUNCTION {}.cast_text({}.cast_source) RETURNS text LANGUAGE sql IMMUTABLE AS $b$SELECT 'changed'$b$",m.source,m.source)).await?;
        let fresh=definitions_are_current(&a,&b,&blocked).await?;ensure!(!fresh.source&&!fresh.destination,"Cast implementation was not retained as evidence");
        ensure!(scalar(&a,&format!("SELECT CAST('a'::{}.cast_source AS text)",m.source)).await?=="changed");
        ensure!(apply_definition_comparison(&a,&b,&blocked).await?.status==DefinitionApplyStatus::Blocked);
        ensure!(scalar(&b,&format!("SELECT {}.f(10)",m.destination)).await?=="11");
        // Removing our custom cast restores the supported built-in matrix.
        exec(&a,&format!("DROP CAST ({}.cast_source AS text)",m.source)).await?;
        let clean=compare(&a,&b,&m,"f").await?;ensure!(!clean.is_blocked(),"{:?}",clean.blockers());
        ensure!(apply_definition_comparison(&a,&b,&clean).await?.status==DefinitionApplyStatus::Applied);
        ensure!(scalar(&b,&format!("SELECT {}.f(10)",m.destination)).await?=="12");
        Ok(())
    }.await;
    cleanup(&a, &m).await?;
    result
}
