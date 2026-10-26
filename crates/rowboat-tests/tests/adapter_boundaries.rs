//! Consumer architecture contracts: metadata SQL belongs to engine adapters.

#[test]
fn completion_consumers_do_not_embed_engine_catalog_sql() {
    let consumers = [
        (
            "GUI completion",
            include_str!("../../rowboat/src/completion_ui.rs"),
        ),
        (
            "CLI workbench",
            include_str!("../../rowboat-cli/src/workbench.rs"),
        ),
    ];
    for (name, source) in consumers {
        // Tests may deliberately use native SQL for setup. Only the production
        // portion is a consumer of the session-bound metadata API.
        // A cfg(test) import can precede all production code. Split at the test
        // module specifically so that such imports cannot make this check vacuous.
        let production = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
        assert!(
            production.contains("pub"),
            "{name}: empty production boundary"
        );
        for catalog in [
            "pg_catalog.",
            "information_schema.",
            "sqlite_schema",
            "sqlite_master",
            "pragma_table_info",
            "pragma_table_xinfo",
        ] {
            assert!(
                !production.to_ascii_lowercase().contains(catalog),
                "{name} embeds {catalog}; use the bounded engine metadata contract"
            );
        }
    }
}
