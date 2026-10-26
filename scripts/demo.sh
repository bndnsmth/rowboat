#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
bash scripts/test-databases.sh up
podman exec -i -e PGPASSWORD=rowboat-test rowboat-test-postgres psql -U postgres -d rowboat -v ON_ERROR_STOP=1 < fixtures/workbench.sql
podman exec -i -e MYSQL_PWD=rowboat-test rowboat-test-mysql mysql -u rowboat -D rowboat < fixtures/workbench.sql

umask 077
mkdir -p .local/rowboat-demo
if [[ ! -e .local/rowboat-demo/state.json ]]; then
  cat > .local/rowboat-demo/state.json <<'JSON'
{
  "connections": [
    {"id":"rowboat-podman-postgres","name":"Podman PostgreSQL","group":"Local test databases","engine":"Postgres","host":"127.0.0.1","port":15435,"database":"rowboat","username":"postgres","ssl_mode":"disable","ssl_ca":null,"ssl_cert":null,"ssl_key":null,"color":"blue","read_only":false,"ssh":null},
    {"id":"rowboat-podman-mysql","name":"Podman MySQL","group":"Local test databases","engine":"MySql","host":"127.0.0.1","port":13309,"database":"rowboat","username":"rowboat","ssl_mode":"disable","ssl_ca":null,"ssl_cert":null,"ssl_key":null,"color":"orange","read_only":false,"ssh":null}
  ],
  "history": [],
  "snippets": [
    {"id":"orders-overview","name":"Orders overview","sql":"SELECT * FROM order_summary ORDER BY id;"},
    {"id":"inventory","name":"Available inventory","sql":"SELECT sku, name, price, stock FROM products WHERE active = TRUE ORDER BY stock DESC;"}
  ],
  "last_connection_id": "rowboat-podman-postgres"
}
JSON
fi
if [[ "${1:-}" == --seed-only ]]; then
  exit 0
fi
export ROWBOAT_DATA_DIR="$PWD/.local/rowboat-demo"
unset KEEL_DATA_DIR # The demo explicitly selects its own isolated state.
export ROWBOAT_PASSWORD=rowboat-test
exec cargo run -p rowboat -- --connection "${1:-rowboat-podman-postgres}" --query fixtures/welcome.sql
