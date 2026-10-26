# Disposable TLS integration fixtures

Requires existing host OpenSSL, rootless Podman, and cached images
`docker.io/library/postgres:18` and `docker.io/library/mysql:8.4`. No downloads or
installations are performed.

```sh
bash scripts/test-tls-databases.sh up
mise exec -- cargo test -p rowboat-tests --test tls -- --ignored --nocapture --test-threads=1
bash scripts/test-tls-databases.sh down
```

The ignored tests assume ready fixtures. The workspace `test:integration` task
depends on `db:tls:up`; `db:down` also removes these TLS fixtures. Both script
actions are repeatable. They only use `rowboat-test-postgres-tls` and
`rowboat-test-mysql-tls`, requiring labels `io.rowboat.test=true` and
`io.rowboat.fixture=tls` before reusing/removing existing containers. Host bindings
are exclusively `127.0.0.1:15433` and `127.0.0.1:13307`. Database storage is tmpfs.
Avoid concurrent fixture lifecycle commands against these fixed names.

Certificates, private keys, and generation logs stay under ignored `.local/rowboat-tls`
with private permissions. Only server material is mounted into the containers;
CA signing keys and client keys stay on the host. The server SAN is
`DNS:localhost`, deliberately excluding the IP address. Certificates last seven
days. To regenerate, run `down`, remove this project's `.local/rowboat-tls` directory,
then run `up`. `down` removes containers but retains the private certificates.

## Checks through `rowboat-core::Database`

For **both engines**:

- `require` without a configured CA, using `127.0.0.1`.
- `verify-ca` and `verify-full` with the trusted CA and `localhost`.
- A live `SELECT 42` plus server-side evidence of encrypted transport (PostgreSQL
  `pg_stat_ssl`, MySQL session `Ssl_cipher`).
- `verify-full` rejects the hostname mismatch at `127.0.0.1`.
- Both verification modes reject a separate, untrusted server CA.
- Server policy rejects plaintext connections.
- A certificate-required account rejects a missing or untrusted client
  certificate, and accepts the trusted certificate/key. PostgreSQL uses `cert`
  authentication without a password and exposes the authenticated client DN;
  MySQL uses `REQUIRE X509` together with its disposable account password.
- A valid verified connection still succeeds after negative handshakes.

Two additional regression tests assert that `verify-ca` accepts the trusted
certificate at the nonmatching IP hostname. Initial verification exposed a
SQLx 0.8.6/rustls compatibility issue: SQLx's `NoHostnameTlsVerifier` handles
`NotValidForName` but not `NotValidForNameContext`. Rowboat now uses SQLx's native
TLS backend with the existing system OpenSSL. All four TLS tests pass, including
CA-only hostname independence and strict `verify-full` rejection. No certificate
verification mode was weakened and no host package was installed.

This fixture does not cover SSH, GUI profile editing, revocation, expired
certificates, or every supported TLS protocol/cipher combination.
