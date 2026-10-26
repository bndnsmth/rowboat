//! Account/role/privilege facade. Providers compile review-only plans and inspect
//! native ACLs; callers remain responsible for explicit review and execution.
pub use crate::providers::access_sql::*;

/// Exact provider-specific account/role information on the supplied session.
pub async fn inspect_account(
    db: &impl rowboat_core::QuerySession,
    principal: &crate::access_matrix::Principal,
) -> anyhow::Result<Vec<rowboat_core::QueryResult>> {
    use crate::access_matrix::Principal;
    crate::providers::access::validate_principal(db.engine(), principal)?;
    match principal {
        Principal::Postgres(name) => pg_role_info(db, name).await,
        Principal::MySql(account) => mysql_account_info(db, account).await,
    }
}

/// PostgreSQL grant rows followed by role-membership rows; MySQL native grants.
/// All requests use the caller's QuerySession, including an existing lease.
pub async fn inspect_grants(
    db: &impl rowboat_core::QuerySession,
    principal: &crate::access_matrix::Principal,
) -> anyhow::Result<Vec<rowboat_core::QueryResult>> {
    use crate::access_matrix::Principal;
    crate::providers::access::validate_principal(db.engine(), principal)?;
    match principal {
        Principal::Postgres(name) => {
            let mut results = pg_grants(db, name).await?;
            results.extend(pg_role_memberships(db, name).await?);
            Ok(results)
        }
        Principal::MySql(account) => mysql_grants(db, account).await,
    }
}
