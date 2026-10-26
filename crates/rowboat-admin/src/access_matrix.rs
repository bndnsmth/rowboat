//! Stable selected-principal ACL API. The optional access provider owns native
//! direct/inherited grant evidence, role closure, version and permission rules.
pub use crate::providers::access::*;

/// Construct the provider's exact principal without consumer engine branches.
pub fn principal(
    engine: rowboat_core::Engine,
    account: String,
    host: Option<String>,
) -> anyhow::Result<Principal> {
    crate::providers::access::build_principal(engine, account, host)
}
