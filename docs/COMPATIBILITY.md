# Data and configuration compatibility

Rowboat was previously named Keel. Legacy names below are intentional data
contracts, not instructions to rename or copy a user's store.

## Store selection

1. `ROWBOAT_DATA_DIR` selects an exact directory. `KEEL_DATA_DIR` is accepted when
   the new name is absent. If both exist, values must match; empty overrides fail.
2. Otherwise, an existing `rowboat` directory under the native data root wins. If
   only `keel` exists, it is reused in place; a fresh install creates `rowboat`.
3. Linux uses the XDG data root, normally `~/.local/share`. macOS uses
   `~/Library/Application Support`.

There is no automatic merge, move, fallback from invalid new state, or ID rewrite.
Unexpected file types and symlinks are rejected. `state.json`, `session.json` and
`session.lock` retain their names; lock files are not replaced to acquire ownership.
GUI and CLI share the selected store and mutation lock.

Profiles, SQL history, saved queries, preferences and supported recovery formats
retain their identity. Recovery never restores a live session, reruns SQL or
reuses persisted query-edit authorization. SQL/history can contain sensitive
user-authored literals even though credential fields are excluded.

## Environment aliases

Legacy `KEEL_` aliases remain for `PASSWORD`, `URL`, `SSH_KNOWN_HOSTS`, `SOURCE_URL`,
`SOURCE_PASSWORD`, `SOURCE_SSH_KNOWN_HOSTS`, `POSTGRES_CLIENT_IMAGE`,
`MYSQL_CLIENT_IMAGE` and `TRACE_KEYS`. New-name presence wins, including an empty
value. Invalid Unicode is rejected rather than falling back. CLI flags retain
precedence. `DATA_DIR` follows the stricter conflict rules above.

## Credentials and portable connections

Linux Secret Service entries retain `application=keel` and `connection-id=<ID>`
for get/set/delete. The display label is Rowboat database connection. No keyring
enumeration, namespace migration or automatic secret copies are performed.
macOS uses Keychain service `dev.bndnsmth.rowboat` and the connection ID as account.

Portable exports use `rowboat-connections`; imports also accept the supported
legacy `keel-connections` format. Network-only v1 and file/mixed v2 profiles are
supported. Unknown formats/versions are rejected. Imports assign fresh IDs by
default, unlike whole-store loading. Passwords are not part of either format.

Rowboat's lossless TSV/CSV conventions (`\N`, `\x...`, backslash escaping) and
JSON `$binary` values remain unchanged. See [CONNECTIONS.md](CONNECTIONS.md).
