use anyhow::{Context, Result, ensure};
use rowboat_admin::{
    access::{self, MySqlAccount, Privilege},
    access_matrix::*,
};
use rowboat_core::{ConnectionProfile, Database, Engine, TableRef, sql::quote_ident};
use std::time::{SystemTime, UNIX_EPOCH};

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        database: "rowboat".into(),
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "root"
        }
        .into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}
fn unique() -> String {
    format!(
        "pass12_admin_{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
async fn exec(db: &Database, sql: &str) -> Result<()> {
    db.query(sql, 100).await?;
    Ok(())
}
async fn check_principal_facade_on_lease(db: &Database, principal: &Principal) -> Result<()> {
    let lease = db.administrative_operation(None).await?;
    let info = access::inspect_account(&lease, principal).await?;
    ensure!(
        info.iter().any(|r| !r.rows.is_empty()),
        "Missing principal info on lease"
    );
    let grants = access::inspect_grants(&lease, principal).await?;
    ensure!(
        grants.iter().any(|r| !r.rows.is_empty()),
        "Missing grants on lease"
    );
    lease.finish().await?;
    Ok(())
}
fn row<'a>(
    report: &'a AccessReport,
    object: &AccessObject,
    privilege: &str,
) -> Result<&'a AccessRow> {
    report
        .rows
        .iter()
        .find(|r| &r.object == object && r.privilege == privilege)
        .with_context(|| format!("Missing {privilege}: {:?}", report.warnings))
}
fn check(
    report: &AccessReport,
    object: &AccessObject,
    privilege: &str,
    effective: AccessState,
    grantable: AccessState,
) -> Result<()> {
    let r = row(report, object, privilege)?;
    ensure!(
        r.effective == effective && r.grantable == grantable,
        "{privilege}: expected {effective:?}/{grantable:?}, got {:?}/{:?}; warnings {:?}",
        r.effective,
        r.grantable,
        report.warnings
    );
    Ok(())
}
fn table(schema: &str, name: &str) -> AccessObject {
    AccessObject::Table(TableRef {
        schema: schema.into(),
        name: name.into(),
    })
}
fn column(schema: &str, name: &str, col: &str) -> AccessObject {
    AccessObject::Column {
        table: TableRef {
            schema: schema.into(),
            name: name.into(),
        },
        column: col.into(),
    }
}
async fn apply(db: &Database, request: &AccessRequest, change: AccessChange) -> Result<()> {
    let plan = preview_change(db, request, &change).await?;
    if change.revoke {
        ensure!(
            plan.warnings
                .iter()
                .any(|w| w.contains("may not remove effective access")),
            "Revoke warning absent"
        );
    }
    for statement in plan.statements {
        exec(db, &statement.sql).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Explicit disposable PostgreSQL fixture 127.0.0.1:15435"]
async fn pass5_access_pg_inheritance_public_owner_columns_and_review() -> Result<()> {
    let db = Database::connect(&profile(Engine::Postgres), "rowboat-test").await?;
    let prefix = unique();
    let names = [
        format!("{prefix}u"),
        format!("{prefix}a"),
        format!("{prefix}b"),
        format!("{prefix}c"),
    ];
    let [u, a, b, c] = names.each_ref().map(|s| quote_ident(Engine::Postgres, s));
    let ns = quote_ident(Engine::Postgres, &prefix);
    let result:Result<()>=async {
        exec(&db,&format!("CREATE ROLE {u} NOINHERIT LOGIN PASSWORD 'rowboat-test'; CREATE ROLE {a}; CREATE ROLE {b}; CREATE ROLE {c}; CREATE SCHEMA {ns}; CREATE TABLE {ns}.t(id int, other int); CREATE TABLE {ns}.col(id int,other int); CREATE TABLE {ns}.owned(id int); ALTER TABLE {ns}.owned OWNER TO {u}; REVOKE SELECT ON {ns}.owned FROM {u}; GRANT USAGE ON SCHEMA {ns} TO {u}; GRANT SELECT ON {ns}.t TO {a} WITH GRANT OPTION; GRANT UPDATE ON {ns}.t TO {b}; GRANT DELETE ON {ns}.t TO {c}; GRANT {a} TO {u} WITH INHERIT TRUE, SET FALSE; GRANT {b} TO {u} WITH INHERIT FALSE, SET TRUE; GRANT {c} TO {b} WITH INHERIT TRUE; GRANT INSERT,SELECT ON {ns}.t TO {u}; GRANT REFERENCES ON {ns}.t TO PUBLIC; GRANT SELECT(id) ON {ns}.col TO {a}; GRANT UPDATE ON {ns}.col TO {a}" )).await?;
        let t=table(&prefix,"t");let col=column(&prefix,"col","id");let other=column(&prefix,"col","other");let owned=table(&prefix,"owned");let namespace=AccessObject::Namespace(prefix.clone());
        let mut request=AccessRequest {principal:principal(Engine::Postgres,names[0].clone(),None)?,objects:vec![namespace.clone(),t.clone(),col.clone(),other.clone(),owned.clone()],context:AccessContext::Principal};
        check_principal_facade_on_lease(&db,&request.principal).await?;
        let report=inspect_access(&db,&request).await?;
        check(&report,&t,"SELECT",AccessState::Granted,AccessState::Granted)?;
        check(&report,&t,"INSERT",AccessState::Granted,AccessState::Denied)?;
        check(&report,&t,"UPDATE",AccessState::Denied,AccessState::Denied)?;
        check(&report,&t,"DELETE",AccessState::Denied,AccessState::Denied)?;
        check(&report,&t,"REFERENCES",AccessState::Granted,AccessState::Denied)?;
        check(&report,&col,"SELECT",AccessState::Granted,AccessState::Denied)?;
        check(&report,&other,"SELECT",AccessState::Denied,AccessState::Denied)?;
        check(&report,&other,"UPDATE",AccessState::Granted,AccessState::Denied)?;
        check(&report,&owned,"SELECT",AccessState::Denied,AccessState::Granted)?;
        check(&report,&namespace,"USAGE",AccessState::Granted,AccessState::Denied)?;
        ensure!(row(&report,&t,"SELECT")?.provenance.iter().any(|e|e.kind==GrantSource::Inherited && e.active),"Inherited evidence absent");
        ensure!(row(&report,&t,"SELECT")?.provenance.iter().any(|e|e.kind==GrantSource::Direct && e.active),"Direct evidence absent");
        ensure!(row(&report,&t,"REFERENCES")?.provenance.iter().any(|e|e.kind==GrantSource::Public && e.active),"PUBLIC evidence absent");
        let ar=report.roles.iter().find(|r|r.principal==Principal::Postgres(names[1].clone())).context("Role a absent")?;
        ensure!(ar.inherited==AccessState::Granted && ar.can_set_role==AccessState::Denied,"Per-membership inherit/set options ignored");
        let br=report.roles.iter().find(|r|r.principal==Principal::Postgres(names[2].clone())).context("Role b absent")?;
        ensure!(br.inherited==AccessState::Denied && br.can_set_role==AccessState::Granted,"NOINHERIT role mislabeled effective");
        apply(&db,&request,AccessChange {object:t.clone(),privileges:vec![Privilege::Select],revoke:true,grant_option:false}).await?;
        let after=inspect_access(&db,&request).await?;check(&after,&t,"SELECT",AccessState::Granted,AccessState::Granted)?;
        ensure!(!row(&after,&t,"SELECT")?.provenance.iter().any(|e|e.kind==GrantSource::Direct),"Direct revoke not applied");
        apply(&db,&request,AccessChange {object:other.clone(),privileges:vec![Privilege::Select],revoke:false,grant_option:false}).await?;
        check(&inspect_access(&db,&request).await?,&other,"SELECT",AccessState::Granted,AccessState::Denied)?;
        // Verify actual non-superuser execution, not only metadata self-consistency.
        let mut user_profile=profile(Engine::Postgres);user_profile.username=names[0].clone();let user_db=Database::connect(&user_profile,"rowboat-test").await?;
        exec(&user_db,&format!("SELECT * FROM {ns}.t")).await?;
        ensure!(exec(&user_db,&format!("UPDATE {ns}.t SET id=1")).await.is_err(),"Noninherited UPDATE unexpectedly executed");
        request.context=AccessContext::CurrentSession;
        check(&inspect_access(&user_db,&request).await?,&t,"SELECT",AccessState::Granted,AccessState::Granted)?;
        exec(&user_db,&format!("SET ROLE {b}")).await?;
        exec(&user_db,"RESET ROLE").await?;
        request.principal=Principal::Postgres("postgres".into());request.context=AccessContext::Principal;
        let super_report=inspect_access(&db,&request).await?;check(&super_report,&t,"SELECT",AccessState::Granted,AccessState::Granted)?;
        ensure!(row(&super_report,&t,"SELECT")?.provenance.iter().any(|e|e.kind==GrantSource::Superuser),"Superuser evidence absent");
        let first=list_principals(&db,PageRequest {offset:0,limit:1}).await?;let next=first.next_offset.context("Missing principal continuation")?;
        let second=list_principals(&db,PageRequest {offset:next,limit:1}).await?;ensure!(first.principals!=second.principals,"Principal paging repeats first page");
        request.objects=vec![table(&prefix,"missing")];
        ensure!(inspect_access(&db,&request).await?.rows.iter().all(|r|r.effective==AccessState::Unknown),"Missing object presented as denied");
        ensure!(preview_change(&db,&request,&AccessChange {object:t.clone(),privileges:vec![Privilege::Select],revoke:false,grant_option:false}).await.is_err(),"Unselected preview target accepted");
        request.objects=vec![t;33];ensure!(inspect_access(&db,&request).await.is_err(),"Object bound ignored");
        ensure!(list_principals(&db,PageRequest {offset:0,limit:501}).await.is_err(),"Page bound ignored");
        Ok(())
    }.await;
    let cleanup = exec(
        &db,
        &format!("DROP SCHEMA IF EXISTS {ns} CASCADE; DROP ROLE IF EXISTS {u},{a},{b},{c}"),
    )
    .await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
#[ignore = "Explicit disposable MySQL fixture 127.0.0.1:13309"]
async fn pass5_access_mysql_cached_global_revoke_is_unknown_not_denied() -> Result<()> {
    let db = Database::connect(&profile(Engine::MySql), "rowboat-test").await?;
    let prefix = unique();
    let account = MySqlAccount {
        user: format!("{prefix}u"),
        host: "%".into(),
    };
    let user = account.sql()?;
    let ns = quote_ident(Engine::MySql, &prefix);
    let result:Result<()>=async {
        exec(&db,&format!("CREATE USER {user} IDENTIFIED BY 'rowboat-test'; CREATE SCHEMA {ns}; CREATE TABLE {ns}.t(id int); GRANT SELECT ON *.* TO {user}")).await?;
        let mut p=profile(Engine::MySql);p.username=account.user.clone();p.database=prefix.clone();
        let user_db=Database::connect(&p,"rowboat-test").await?;
        exec(&user_db,&format!("SELECT * FROM {ns}.t")).await?;
        exec(&db,&format!("REVOKE SELECT ON *.* FROM {user}")).await?;
        // Static global privileges are retained by this already-authenticated
        // connection. A catalog-only "denied" would be demonstrably false.
        exec(&user_db,&format!("SELECT * FROM {ns}.t")).await?;
        let t=table(&prefix,"t");
        let mut request=AccessRequest {principal:Principal::MySql(account.clone()),objects:vec![t.clone()],context:AccessContext::CurrentSession};
        check(&inspect_access(&user_db,&request).await?,&t,"SELECT",AccessState::Unknown,AccessState::Unknown)?;
        request.context=AccessContext::DefaultRoles;
        check(&inspect_access(&db,&request).await?,&t,"SELECT",AccessState::Denied,AccessState::Denied)?;
        Ok(())
    }.await;
    let cleanup = exec(
        &db,
        &format!("DROP SCHEMA IF EXISTS {ns}; DROP USER IF EXISTS {user}"),
    )
    .await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
#[ignore = "Explicit disposable MySQL fixture 127.0.0.1:13309"]
async fn pass5_access_mysql_default_available_active_hosts_columns_and_unknown() -> Result<()> {
    let db = Database::connect(&profile(Engine::MySql), "rowboat-test").await?;
    let prefix = unique();
    let names = [
        format!("{prefix}u"),
        format!("{prefix}a"),
        format!("{prefix}b"),
        format!("{prefix}c"),
    ];
    let accounts = names.each_ref().map(|n| MySqlAccount {
        user: n.clone(),
        host: "%".into(),
    });
    let [u, a, b, c] = accounts.each_ref().map(|a| a.sql().unwrap());
    let twin = MySqlAccount {
        user: names[0].clone(),
        host: "localhost".into(),
    };
    let tw = twin.sql()?;
    let ns = quote_ident(Engine::MySql, &prefix);
    let result:Result<()>=async {
        exec(&db,&format!("CREATE USER {u} IDENTIFIED BY 'rowboat-test'; CREATE USER {tw}; CREATE ROLE {a},{b},{c}; CREATE SCHEMA {ns}; CREATE TABLE {ns}.t(id int,other int); CREATE TABLE {ns}.col(id int,other int); GRANT SELECT ON {ns}.t TO {a} WITH GRANT OPTION; GRANT UPDATE ON {ns}.t TO {b}; GRANT DELETE ON {ns}.t TO {c}; GRANT {b} TO {a}; GRANT {a},{c} TO {u}; SET DEFAULT ROLE {a} TO {u}; GRANT INSERT,SELECT ON {ns}.t TO {u}; GRANT SELECT(id) ON {ns}.col TO {b}; GRANT UPDATE ON {ns}.col TO {b}; GRANT REFERENCES ON {ns}.t TO {tw}" )).await?;
        let t=table(&prefix,"t");let col=column(&prefix,"col","id");let other=column(&prefix,"col","other");let namespace=AccessObject::Namespace(prefix.clone());
        let mut request=AccessRequest {principal:principal(Engine::MySql,names[0].clone(),Some("%".into()))?,objects:vec![namespace.clone(),t.clone(),col.clone(),other.clone()],context:AccessContext::DefaultRoles};
        check_principal_facade_on_lease(&db,&request.principal).await?;
        let report=inspect_access(&db,&request).await?;
        check(&report,&t,"SELECT",AccessState::Granted,AccessState::Granted)?;
        check(&report,&t,"UPDATE",AccessState::Granted,AccessState::Granted)?;
        ensure!(row(&report,&t,"UPDATE")?.provenance.iter().any(|e|e.grantable && e.scope.contains("authority only")),"Cross-role grant-option authority missing from provenance");
        check(&report,&t,"DELETE",AccessState::Denied,AccessState::Denied)?;
        check(&report,&t,"INSERT",AccessState::Granted,AccessState::Granted)?;
        check(&report,&t,"REFERENCES",AccessState::Denied,AccessState::Denied)?;
        check(&report,&col,"SELECT",AccessState::Granted,AccessState::Denied)?;
        check(&report,&other,"SELECT",AccessState::Denied,AccessState::Denied)?;
        check(&report,&other,"UPDATE",AccessState::Granted,AccessState::Denied)?;
        let cr=report.roles.iter().find(|r|r.principal==Principal::MySql(accounts[3].clone())).context("Available role absent")?;
        ensure!(cr.active==AccessState::Denied && cr.default_role==AccessState::Denied && cr.can_set_role==AccessState::Granted,"Nondefault role reported active");
        let br=report.roles.iter().find(|r|r.principal==Principal::MySql(accounts[2].clone())).context("Nested role absent")?;
        ensure!(br.active==AccessState::Granted && br.default_role==AccessState::Denied && br.can_set_role==AccessState::Denied,"Nested role was not distinguished from direct/default role");
        request.context=AccessContext::Principal;let direct=inspect_access(&db,&request).await?;
        check(&direct,&t,"UPDATE",AccessState::Denied,AccessState::Denied)?;
        check(&direct,&t,"SELECT",AccessState::Granted,AccessState::Denied)?;
        request.context=AccessContext::AvailableRoles;check(&inspect_access(&db,&request).await?,&t,"DELETE",AccessState::Granted,AccessState::Granted)?;
        request.context=AccessContext::DefaultRoles;
        apply(&db,&request,AccessChange {object:t.clone(),privileges:vec![Privilege::Select],revoke:true,grant_option:false}).await?;
        check(&inspect_access(&db,&request).await?,&t,"SELECT",AccessState::Granted,AccessState::Granted)?;
        // Exact underscore-containing schema preview must escape grant patterns.
        apply(&db,&request,AccessChange {object:namespace.clone(),privileges:vec![Privilege::Create],revoke:false,grant_option:false}).await?;
        check(&inspect_access(&db,&request).await?,&namespace,"CREATE",AccessState::Granted,AccessState::Denied)?;
        apply(&db,&request,AccessChange {object:other.clone(),privileges:vec![Privilege::Select],revoke:false,grant_option:false}).await?;
        check(&inspect_access(&db,&request).await?,&other,"SELECT",AccessState::Granted,AccessState::Denied)?;
        request.principal=Principal::MySql(twin.clone());request.context=AccessContext::Principal;
        let twin_report=inspect_access(&db,&request).await?;check(&twin_report,&t,"SELECT",AccessState::Denied,AccessState::Denied)?;check(&twin_report,&t,"REFERENCES",AccessState::Granted,AccessState::Denied)?;
        request.principal=Principal::MySql(accounts[0].clone());
        let mut user_profile=profile(Engine::MySql);user_profile.username=names[0].clone();user_profile.database=prefix.clone();let user_db=Database::connect(&user_profile,"rowboat-test").await?;
        exec(&user_db,&format!("SELECT * FROM {ns}.t")).await?;
        exec(&user_db,&format!("UPDATE {ns}.t SET id=1")).await?;
        ensure!(exec(&user_db,&format!("DELETE FROM {ns}.t")).await.is_err(),"Inactive DELETE role executed");
        request.context=AccessContext::CurrentSession;
        let hidden=inspect_access(&user_db,&request).await?;
        ensure!(hidden.rows.iter().all(|r|r.effective==AccessState::Unknown),"Hidden mysql catalog reported denied");
        ensure!(hidden.warnings.iter().any(|w|w.contains("could not prove")),"Hidden catalog failure absent");
        // Grant only catalog SELECT needed by this inspector, then verify real
        // ENABLED_ROLES before/after SET ROLE on the selected connection.
        exec(&db,&format!("GRANT SELECT ON mysql.user TO {u}; GRANT SELECT ON mysql.role_edges TO {u}; GRANT SELECT ON mysql.default_roles TO {u}; GRANT SELECT ON mysql.db TO {u}; GRANT SELECT ON mysql.tables_priv TO {u}; GRANT SELECT ON mysql.columns_priv TO {u}")).await?;
        let current=inspect_access(&user_db,&request).await?;check(&current,&t,"DELETE",AccessState::Unknown,AccessState::Unknown)?;
        exec(&user_db,&format!("GRANT UPDATE ON {ns}.t TO {tw}; REVOKE UPDATE ON {ns}.t FROM {tw}")).await?;
        exec(&user_db,"SET ROLE ALL").await?;
        check(&inspect_access(&user_db,&request).await?,&t,"DELETE",AccessState::Granted,AccessState::Granted)?;
        exec(&user_db,&format!("DELETE FROM {ns}.t")).await?;
        exec(&user_db,"SET ROLE NONE").await?;
        check(&inspect_access(&user_db,&request).await?,&t,"SELECT",AccessState::Unknown,AccessState::Unknown)?;
        // Root SUPER does not masquerade as ownership: the global SELECT source
        // is evidenced at GLOBAL scope, and account host stays exact.
        request.principal=Principal::MySql(MySqlAccount {user:"root".into(),host:"%".into()});request.context=AccessContext::Principal;
        let root=inspect_access(&db,&request).await?;
        check(&root,&t,"SELECT",AccessState::Granted,AccessState::Granted)?;
        ensure!(row(&root,&t,"SELECT")?.provenance.iter().any(|e|e.scope=="GLOBAL" && e.kind==GrantSource::Direct),"Global grant provenance absent");
        let first=list_principals(&db,PageRequest {offset:0,limit:1}).await?;ensure!(first.principals.len()==1 && first.next_offset==Some(1),"MySQL principal paging absent");
        Ok(())
    }.await;
    let cleanup = exec(
        &db,
        &format!("DROP SCHEMA IF EXISTS {ns}; DROP USER IF EXISTS {u},{tw},{a},{b},{c}"),
    )
    .await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
#[ignore = "Explicit disposable MySQL fixture 127.0.0.1:13309"]
async fn pass5_access_mysql_unicode_column_grants_use_server_identifier_case() -> Result<()> {
    let db = Database::connect(&profile(Engine::MySql), "rowboat-test").await?;
    let prefix = unique();
    let account = MySqlAccount {
        user: format!("{prefix}u"),
        host: "%".into(),
    };
    let user = account.sql()?;
    let ns = quote_ident(Engine::MySql, &prefix);
    let result: Result<()> = async {
        exec(&db, &format!("CREATE USER {user} IDENTIFIED BY 'rowboat-test'; CREATE SCHEMA {ns}; CREATE TABLE {ns}.t(`Ä` int, `A` int, `Z` int); GRANT SELECT (`ä`) ON {ns}.t TO {user}")).await?;
        let umlaut = column(&prefix, "t", "Ä");
        let ascii = column(&prefix, "t", "A");
        let unrelated = column(&prefix, "t", "Z");
        let whole = table(&prefix, "t");
        let request = AccessRequest {
            principal: Principal::MySql(account.clone()),
            objects: vec![umlaut.clone(), ascii.clone(), unrelated.clone(), whole.clone()],
            context: AccessContext::Principal,
        };
        let report = inspect_access(&db, &request).await?;
        check(&report, &umlaut, "SELECT", AccessState::Granted, AccessState::Denied)?;
        check(&report, &ascii, "SELECT", AccessState::Granted, AccessState::Denied)?;
        check(&report, &unrelated, "SELECT", AccessState::Denied, AccessState::Denied)?;
        check(&report, &umlaut, "UPDATE", AccessState::Denied, AccessState::Denied)?;
        check(&report, &ascii, "UPDATE", AccessState::Denied, AccessState::Denied)?;
        check(&report, &whole, "SELECT", AccessState::Denied, AccessState::Denied)?;
        ensure!(row(&report, &umlaut, "SELECT")?.provenance.iter().any(|e| e.kind == GrantSource::Direct && e.scope == "COLUMN" && e.active), "Unicode column grant lost its direct provenance");
        let mut p = profile(Engine::MySql);
        p.username = account.user.clone();
        p.database = prefix.clone();
        let user_db = Database::connect(&p, "rowboat-test").await?;
        exec(&user_db, &format!("SELECT `Ä` FROM {ns}.t")).await.context("Native Unicode SELECT")?;
        // Native ACL comparison also equates accents; this is not equivalent
        // to lowercase Unicode byte equality or to column-definition identity.
        exec(&user_db, &format!("SELECT `A` FROM {ns}.t")).await.context("Native ACL accent equivalence")?;
        ensure!(exec(&user_db, &format!("SELECT `Z` FROM {ns}.t")).await.is_err(), "Column grant incorrectly covered an unrelated column");
        // Multiple physical grant records can collide in the server's ACL
        // hash. Do not invent an additive union or a cache iteration order.
        exec(&db, &format!("GRANT UPDATE (`a`) ON {ns}.t TO {user}")).await?;
        let overlap = inspect_access(&db, &request).await?;
        check(&overlap, &umlaut, "SELECT", AccessState::Unknown, AccessState::Unknown)?;
        check(&overlap, &ascii, "UPDATE", AccessState::Unknown, AccessState::Unknown)?;
        ensure!(overlap.warnings.iter().any(|w| w.contains("Overlapping MySQL column grant")), "Colliding ACL column keys lacked a limitation warning");
        Ok(())
    }.await;
    let cleanup = exec(
        &db,
        &format!("DROP SCHEMA IF EXISTS {ns}; DROP USER IF EXISTS {user}"),
    )
    .await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
#[ignore = "Explicit disposable PostgreSQL fixture 127.0.0.1:15435"]
async fn pass5_access_pg_current_session_set_role_uses_session_authority() -> Result<()> {
    let db = Database::connect(&profile(Engine::Postgres), "rowboat-test").await?;
    let prefix = unique();
    let names = [
        format!("{prefix}u"),
        format!("{prefix}a"),
        format!("{prefix}b"),
        format!("{prefix}c"),
    ];
    let [u, a, b, c] = names.each_ref().map(|s| quote_ident(Engine::Postgres, s));
    let ns = quote_ident(Engine::Postgres, &prefix);
    let result: Result<()> = async {
        exec(&db, &format!("CREATE ROLE {u} NOINHERIT LOGIN PASSWORD 'rowboat-test'; CREATE ROLE {a}; CREATE ROLE {b}; CREATE ROLE {c}; CREATE SCHEMA {ns}; CREATE TABLE {ns}.t(id int); GRANT USAGE ON SCHEMA {ns} TO {a},{b},{c}; GRANT INSERT ON {ns}.t TO {u}; GRANT UPDATE ON {ns}.t TO {a}; GRANT SELECT ON {ns}.t TO {b}; GRANT DELETE ON {ns}.t TO {c}; GRANT {a},{b},{c} TO {u} WITH SET TRUE, INHERIT FALSE; GRANT {b} TO {a} WITH SET FALSE, INHERIT FALSE")).await?;
        let mut p = profile(Engine::Postgres);
        p.username = names[0].clone();
        let user_db = Database::connect(&p, "rowboat-test").await?;
        exec(&user_db, &format!("SET ROLE {a}")).await?;
        let t = table(&prefix, "t");
        let mut request = AccessRequest { principal: Principal::Postgres(names[1].clone()), objects: vec![t.clone()], context: AccessContext::CurrentSession };
        let report = inspect_access(&user_db, &request).await?;
        for name in [&names[2], &names[3]] {
            let role = report.roles.iter().find(|r| r.principal == Principal::Postgres(name.clone())).with_context(|| format!("Session-available role absent: {name}"))?;
            ensure!(role.can_set_role == AccessState::Granted && role.inherited == AccessState::Denied && role.active == AccessState::Denied, "SET ROLE authority was conflated with current effective inheritance");
        }
        check(&report, &t, "UPDATE", AccessState::Granted, AccessState::Denied)?;
        for privilege in ["INSERT", "SELECT", "DELETE"] {
            check(&report, &t, privilege, AccessState::Denied, AccessState::Denied)?;
        }
        // The selected-principal model must still honor a's SET FALSE edge.
        request.context = AccessContext::Principal;
        let selected = inspect_access(&user_db, &request).await?;
        ensure!(selected.roles.iter().any(|r| r.principal == Principal::Postgres(names[2].clone()) && r.can_set_role == AccessState::Denied), "Principal model incorrectly borrowed session_user authority");
        ensure!(!selected.roles.iter().any(|r| r.principal == Principal::Postgres(names[3].clone())), "Principal model included a role available only to session_user");
        exec(&user_db, &format!("UPDATE {ns}.t SET id=1")).await?;
        ensure!(exec(&user_db, &format!("SELECT * FROM {ns}.t")).await.is_err(), "Session authority incorrectly became current SELECT access");
        exec(&user_db, &format!("SET ROLE {b}; SELECT * FROM {ns}.t; SET ROLE {c}; DELETE FROM {ns}.t")).await?;
        Ok(())
    }.await;
    let cleanup = exec(
        &db,
        &format!("DROP SCHEMA IF EXISTS {ns} CASCADE; DROP ROLE IF EXISTS {u},{a},{b},{c}"),
    )
    .await;
    result?;
    cleanup?;
    Ok(())
}
