# Rowboat

Native Rust database workbench using GPUI Kit and gpui-omarchy. PostgreSQL, MySQL and SQLite. Runtimes managed exclusively by project mise.toml.

- Preserve independent work. Shared contracts live in crates/rowboat-core/src/model.rs; coordinate changes.
- Keep driver mechanics/catalog SQL in core adapters and optional tool providers. GUI/CLI consume typed metadata and capability contracts; see docs/ENGINE-ADAPTERS.md.
- Use real database operations, never present sample data as a live connection.
- Never persist passwords in connection JSON or logs. Query history is local sensitive data.
- Quote identifiers by dialect and parameterize values. Editing must identify rows by complete primary keys and detect conflicting updates.
- Run format/check/tests. Integration tests use only explicit local disposable databases, never discovered user connections.
- Document verified capabilities and remaining parity gaps honestly in docs/FEATURES.md.
- Do not deploy or change user-wide/system configuration.
