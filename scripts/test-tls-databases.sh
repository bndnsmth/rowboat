#!/usr/bin/env bash
set -euo pipefail
umask 077

# Disposable, rootless, TLS-only fixtures. No downloads or host configuration.
# Run: bash scripts/test-tls-databases.sh up|down
# Start before the ignored crates/rowboat-tests/tests/tls.rs tests; down afterwards.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tls="$root/.local/rowboat-tls"
names=(rowboat-test-postgres-tls rowboat-test-mysql-tls)

owned() {
  [[ "$(podman inspect --format '{{index .Config.Labels "io.rowboat.test"}}' "$1")" == true &&
     "$(podman inspect --format '{{index .Config.Labels "io.rowboat.fixture"}}' "$1")" == tls ]] || {
    echo "Refusing to use/remove unrelated container $1" >&2
    return 1
  }
}

command -v podman >/dev/null
[[ "$(podman info --format '{{.Host.Security.Rootless}}')" == true ]] || {
  echo 'TLS fixtures require rootless Podman.' >&2; exit 1;
}
case "${1:-up}" in
  down)
    # Preflight both before removing either.
    for name in "${names[@]}"; do
      if podman container exists "$name"; then owned "$name"; fi
    done
    for name in "${names[@]}"; do
      if podman container exists "$name"; then podman rm -f "$name" >/dev/null; fi
    done
    echo 'TLS fixture containers removed; private certificates remain in .local/rowboat-tls.'
    exit 0
    ;;
  up) ;;
  *) echo "Usage: $0 [up|down]" >&2; exit 2 ;;
esac

command -v openssl >/dev/null || { echo 'Existing host OpenSSL is required.' >&2; exit 1; }
for image in docker.io/library/postgres:18 docker.io/library/mysql:8.4; do
  podman image exists "$image" || { echo "Required cached image missing: $image" >&2; exit 1; }
done
for name in "${names[@]}"; do
  if podman container exists "$name"; then owned "$name"; fi
done

mkdir -p "$tls"
chmod 700 "$tls"
if [[ ! -f "$tls/ready" ]]; then
  for name in "${names[@]}"; do
    if podman container exists "$name"; then
      echo 'Incomplete certificates with existing fixtures; run down before up.' >&2; exit 1
    fi
  done
  mkdir -p "$tls/authority" "$tls/server" "$tls/client"
  chmod 700 "$tls/authority" "$tls/server" "$tls/client"
  # Progress/errors go to a private file; never emit key contents.
  generate() {
    openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 7 \
      -subj '/CN=Rowboat disposable TLS CA' \
      -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' \
      -keyout "$tls/authority/ca.key" -out "$tls/server/ca.crt"
    openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 7 \
      -subj '/CN=Rowboat unrelated TLS CA' \
      -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' \
      -keyout "$tls/authority/untrusted-ca.key" -out "$tls/client/untrusted-ca.crt"
    cat > "$tls/server/extensions.cnf" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
EOF
    openssl req -new -newkey rsa:2048 -nodes -subj '/CN=localhost' \
      -keyout "$tls/server/server.key" -out "$tls/server/server.csr"
    openssl x509 -req -sha256 -days 7 -in "$tls/server/server.csr" \
      -CA "$tls/server/ca.crt" -CAkey "$tls/authority/ca.key" -set_serial 2 \
      -extfile "$tls/server/extensions.cnf" -out "$tls/server/server.crt"
    cat > "$tls/client/extensions.cnf" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth
EOF
    for identity in client untrusted-client; do
      ca="$tls/server/ca.crt"; key="$tls/authority/ca.key"
      if [[ "$identity" == untrusted-client ]]; then
        ca="$tls/client/untrusted-ca.crt"; key="$tls/authority/untrusted-ca.key"
      fi
      openssl req -new -newkey rsa:2048 -nodes -subj '/CN=rowboat_cert' \
        -keyout "$tls/client/$identity.key" -out "$tls/client/$identity.csr"
      openssl x509 -req -sha256 -days 7 -in "$tls/client/$identity.csr" \
        -CA "$ca" -CAkey "$key" -set_serial 3 \
        -extfile "$tls/client/extensions.cnf" -out "$tls/client/$identity.crt"
    done
  }
  generate > "$tls/generation.log" 2>&1
  cat > "$tls/server/pg_hba.conf" <<'EOF'
local all all trust
hostnossl all all 0.0.0.0/0 reject
hostnossl all all ::/0 reject
hostssl all rowboat_cert 0.0.0.0/0 cert
hostssl all rowboat_cert ::/0 cert
hostssl all all 0.0.0.0/0 scram-sha-256
hostssl all all ::/0 scram-sha-256
EOF
  touch "$tls/ready"
fi
openssl x509 -checkend 300 -noout -in "$tls/server/server.crt" >/dev/null || {
  echo 'Fixture certificates expired; run down, remove .local/rowboat-tls, then up to regenerate.' >&2; exit 1;
}

# Container root copies only server material from the private read-only mount,
# then changes ownership inside tmpfs (never on the host bind mount).
for engine in postgres mysql; do
  name="rowboat-test-$engine-tls"
  if podman container exists "$name"; then
    podman start "$name" >/dev/null
  elif [[ "$engine" == postgres ]]; then
    podman run -d --pull=never --name "$name" \
      --label io.rowboat.test=true --label io.rowboat.fixture=tls \
      -p 127.0.0.1:15433:5432 -e POSTGRES_PASSWORD=rowboat-test -e POSTGRES_DB=rowboat \
      --tmpfs /var/lib/postgresql --tmpfs /run/rowboat-tls \
      -v "$tls/server:/fixture:ro,z" --entrypoint bash docker.io/library/postgres:18 -ec '
        cp /fixture/{ca.crt,server.crt,server.key,pg_hba.conf} /run/rowboat-tls/
        chown -R postgres:postgres /run/rowboat-tls
        chmod 700 /run/rowboat-tls; chmod 600 /run/rowboat-tls/*
        exec docker-entrypoint.sh postgres -c ssl=on \
          -c ssl_ca_file=/run/rowboat-tls/ca.crt -c ssl_cert_file=/run/rowboat-tls/server.crt \
          -c ssl_key_file=/run/rowboat-tls/server.key -c hba_file=/run/rowboat-tls/pg_hba.conf
      ' >/dev/null
  else
    podman run -d --pull=never --name "$name" \
      --label io.rowboat.test=true --label io.rowboat.fixture=tls \
      -p 127.0.0.1:13307:3306 -e MYSQL_ROOT_PASSWORD=rowboat-test \
      -e MYSQL_DATABASE=rowboat -e MYSQL_USER=rowboat -e MYSQL_PASSWORD=rowboat-test \
      --tmpfs /var/lib/mysql --tmpfs /run/rowboat-tls \
      -v "$tls/server:/fixture:ro,z" --entrypoint bash docker.io/library/mysql:8.4 -ec '
        cp /fixture/{ca.crt,server.crt,server.key} /run/rowboat-tls/
        chown -R mysql:mysql /run/rowboat-tls
        chmod 700 /run/rowboat-tls; chmod 600 /run/rowboat-tls/*
        exec docker-entrypoint.sh mysqld --require-secure-transport=ON \
          --ssl-ca=/run/rowboat-tls/ca.crt --ssl-cert=/run/rowboat-tls/server.crt \
          --ssl-key=/run/rowboat-tls/server.key
      ' >/dev/null
  fi
done

ready=false
for attempt in {1..90}; do
  if timeout 5 podman exec rowboat-test-postgres-tls pg_isready -h 127.0.0.1 -U postgres -d rowboat >/dev/null 2>&1 &&
     timeout 5 podman exec -e MYSQL_PWD=rowboat-test rowboat-test-mysql-tls \
       mysql --connect-timeout=3 --ssl-mode=REQUIRED -h 127.0.0.1 -u rowboat -D rowboat -e 'SELECT 1' >/dev/null 2>&1; then
    ready=true; break
  fi
  sleep 1
done
$ready || { echo 'TLS database readiness timed out; inspect task-owned container logs.' >&2; exit 1; }

podman exec -i rowboat-test-postgres-tls psql -v ON_ERROR_STOP=1 -U postgres -d rowboat >/dev/null <<'SQL'
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rowboat_cert') THEN
    CREATE ROLE rowboat_cert LOGIN;
  END IF;
END $$;
GRANT CONNECT ON DATABASE rowboat TO rowboat_cert;
SQL
podman exec -i -e MYSQL_PWD=rowboat-test rowboat-test-mysql-tls mysql -u root >/dev/null <<'SQL'
CREATE USER IF NOT EXISTS 'rowboat_cert'@'%' IDENTIFIED BY 'rowboat-test' REQUIRE X509;
GRANT SELECT ON rowboat.* TO 'rowboat_cert'@'%';
SQL
echo 'TLS fixtures ready: PostgreSQL localhost:15433, MySQL localhost:13307 (database rowboat).'
