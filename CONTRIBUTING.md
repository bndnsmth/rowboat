# Contributing

Use the runtimes and development tools declared in `mise.toml`. Start with the
[architecture](docs/ARCHITECTURE.md), [engine adapter boundary](docs/ENGINE-ADAPTERS.md)
and [testing guide](docs/TESTING.md).

```sh
mise trust
mise install
mise run dev
mise run fmt
mise run check
mise run lint
mise run lint:distribution
mise run test
```

Use the separate headless GUI suite when changing input, rendering, close/recovery
or workspace behavior. Native GUI acceptance is still necessary: compilation and
headless tests cannot establish platform integration or usability.

## Database and data safety

- Integration tests use explicitly provisioned disposable databases, never saved
  profiles or discovered user connections.
- Keep driver mechanics/catalog SQL in adapters and typed optional providers.
- Quote identifiers by dialect, parameterize values, require complete primary
  keys for edits, and retain optimistic conflict detection.
- Separate review from mutation. Never substitute sample data for a live result.
- Do not commit real profiles, credentials, query history, database files,
  private keys, raw logs or unreviewed screenshots.

Generated builds, reports and fixture material belong under ignored `.local/` or
`target/`. Keep documentation focused on current behavior and reproducible
instructions rather than local run journals. Intentional launch screenshots use
only the documented fictional demo.

Before committing, stage the intended files, inspect the diff and run
`mise run audit:secrets`. Secret scanning is a safeguard, not proof that every
form of private data has been removed. See [SECURITY.md](SECURITY.md).
