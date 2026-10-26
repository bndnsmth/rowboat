# Security and sensitive data

Do not put credentials, private connection details, query history, production
SQL or unreviewed screenshots in public issues or pull requests. Use a minimal
reproduction with an explicitly disposable database and fictional data.

For vulnerabilities, use GitHub private vulnerability reporting if it is enabled,
or contact a maintainer privately before disclosing sensitive reproduction details.
Do not post a working credential or private key to demonstrate a problem.

## Application boundaries

- Saved connection JSON has no password field. Remembered credentials use the
  system keyring; session passwords remain in memory.
- SQL, snippets, history and recovery can contain sensitive literals even when
  profile credentials are omitted. Treat local data and exports accordingly.
- Read-only mode is a workbench policy, not a database privilege boundary. Use
  restricted database roles; trusted server functions can have external effects.
- Review generated mutation SQL before Apply. Direct SQL and EXPLAIN ANALYZE can
  execute changes; cancellation does not guarantee rollback of earlier statements.
- Vendored dependency security fixes require rebuilt releases. Platform/signing
  status is documented in [DISTRIBUTION.md](docs/DISTRIBUTION.md).

## Repository hygiene

`mise run audit:secrets` scans staged changes and committed Git history with
redacted output. CI runs the same check. No historical findings are excluded.
Real secrets must not be allowlisted.

Removing a file in a new commit does not erase old revisions. Before publishing
or sharing a repository, review its full history as well as the current tree.
If a real credential was committed, revoke/rotate it and arrange a deliberate
history cleanup. Never assume a scanner or a later deletion makes it safe.
