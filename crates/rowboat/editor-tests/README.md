# Headless GUI regressions

Uses GPUI's in-memory TestPlatform and the application's actual source modules.
It creates no native windows and reads no application data directory. Workbench
fixtures use in-memory channels; recovery fixtures use private temporary folders.

From the project root:

```sh
cargo test --locked --manifest-path crates/rowboat/editor-tests/Cargo.toml --target-dir target -- --test-threads=1
cargo clippy --locked --manifest-path crates/rowboat/editor-tests/Cargo.toml --target-dir target --all-targets -- -D warnings
cargo fmt --manifest-path crates/rowboat/editor-tests/Cargo.toml -- --check
```

The isolated manifest/lock enables `gpui-pre/test-support` without changing the
application's lockfile or enabling GPUI Kit's optional alternate platform graph.
Runtimes come from the parent project's mise configuration. Source modules are
referenced directly, not copied or reimplemented.

The suite covers editor replacement/undo, linked typing, multi-window ownership,
close/recovery behavior, connections, retained results, diagrams and inspection.
It also discovers application module tests; do not double-count those as separate
coverage. Live server tests remain ignored unless explicitly requested against
the documented disposable fixtures. See [testing](../../../docs/TESTING.md).
