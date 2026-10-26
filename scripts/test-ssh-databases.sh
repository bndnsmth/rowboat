#!/usr/bin/env bash
set -euo pipefail
umask 077
# Task-owned SSH + TLS fixtures only. All server/package installation is in Podman.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$root/.local/rowboat-ssh"
names=(rowboat-test-postgres-ssh rowboat-test-mysql-ssh rowboat-test-sshd)
network=rowboat-test-ssh
image=localhost/rowboat-ssh-test:1
owned() {
  [[ "$(podman inspect --format '{{index .Config.Labels "io.rowboat.fixture"}}' "$1")" == ssh ]] || {
    echo "Refusing unrelated container $1" >&2; return 1;
  }
}
command -v podman >/dev/null
[[ "$(podman info --format '{{.Host.Security.Rootless}}')" == true ]] || { echo 'Rootless Podman required.' >&2; exit 1; }
for name in "${names[@]}"; do
  if podman container exists "$name"; then owned "$name"; fi
done
case "${1:-up}" in
  down)
    for name in "${names[@]}"; do
      if podman container exists "$name"; then podman rm -f "$name" >/dev/null; fi
    done
    if podman network exists "$network"; then
      [[ "$(podman network inspect --format '{{index .Labels "io.rowboat.fixture"}}' "$network")" == ssh ]] || { echo 'Refusing unrelated network.' >&2; exit 1; }
      podman network rm "$network" >/dev/null
    fi
    echo 'SSH fixture containers/network removed; private .local/rowboat-ssh fixtures and cached test image retained.'
    exit 0;;
  up) ;;
  *) echo "Usage: $0 [up|down]" >&2; exit 2;;
esac
for tool in openssl ssh-keygen ssh; do command -v "$tool" >/dev/null; done
for base in docker.io/library/postgres:18 docker.io/library/mysql:8.4; do
  podman image exists "$base" || { echo "Cached image required: $base" >&2; exit 1; }
done
mkdir -p "$fixture"; chmod 700 "$fixture"
if [[ ! -f "$fixture/ready" ]]; then
  for name in "${names[@]}"; do
    if podman container exists "$name"; then echo 'Run down before regenerating fixtures.' >&2; exit 1; fi
  done
  mkdir -p "$fixture/server" "$fixture/client" "$fixture/authority" "$fixture/sshd"
  chmod 700 "$fixture"/{server,client,authority,sshd}
  generate() {
    openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 7 -subj '/CN=Rowboat SSH disposable CA' \
      -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' \
      -keyout "$fixture/authority/ca.key" -out "$fixture/server/ca.crt"
    openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 7 -subj '/CN=Rowboat SSH untrusted CA' \
      -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' \
      -keyout "$fixture/authority/untrusted.key" -out "$fixture/client/untrusted-ca.crt"
    cat > "$fixture/server/extensions.cnf" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:postgres.rowboat.test,DNS:mysql.rowboat.test
EOF
    openssl req -new -newkey rsa:2048 -nodes -subj '/CN=postgres.rowboat.test' \
      -keyout "$fixture/server/server.key" -out "$fixture/server/server.csr"
    openssl x509 -req -sha256 -days 7 -in "$fixture/server/server.csr" -CA "$fixture/server/ca.crt" \
      -CAkey "$fixture/authority/ca.key" -set_serial 2 -extfile "$fixture/server/extensions.cnf" -out "$fixture/server/server.crt"
    cat > "$fixture/client/extensions.cnf" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth
EOF
    openssl req -new -newkey rsa:2048 -nodes -subj '/CN=rowboat_cert' -keyout "$fixture/client/client.key" -out "$fixture/client/client.csr"
    openssl x509 -req -sha256 -days 7 -in "$fixture/client/client.csr" -CA "$fixture/server/ca.crt" \
      -CAkey "$fixture/authority/ca.key" -set_serial 3 -extfile "$fixture/client/extensions.cnf" -out "$fixture/client/client.crt"
    ssh-keygen -q -t ed25519 -N '' -f "$fixture/sshd/host_key"
    ssh-keygen -q -t ed25519 -N '' -f "$fixture/client/identity"
    ssh-keygen -q -t ed25519 -N '' -f "$fixture/client/unauthorized"
    cp "$fixture/client/identity.pub" "$fixture/sshd/authorized_keys"
    printf '[127.0.0.1]:12222 %s\n' "$(cat "$fixture/sshd/host_key.pub")" > "$fixture/client/known_hosts"
    printf '[127.0.0.1]:12222 %s\n' "$(cat "$fixture/client/unauthorized.pub")" > "$fixture/client/wrong_known_hosts"
    : > "$fixture/client/empty_known_hosts"
  }
  generate > "$fixture/generation.log" 2>&1
  cat > "$fixture/server/pg_hba.conf" <<'EOF'
local all all trust
hostnossl all all 0.0.0.0/0 reject
hostssl all rowboat_cert 0.0.0.0/0 cert
hostssl all all 0.0.0.0/0 scram-sha-256
EOF
  cat > "$fixture/sshd/sshd_config" <<'EOF'
Port 22
ListenAddress 0.0.0.0
HostKey /fixture/host_key
AuthorizedKeysFile /fixture/authorized_keys
PermitRootLogin prohibit-password
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
AllowTcpForwarding yes
AllowStreamLocalForwarding yes
PermitTTY no
X11Forwarding no
PrintMotd no
PidFile /run/sshd.pid
EOF
  touch "$fixture/ready"
fi
openssl x509 -checkend 300 -noout -in "$fixture/server/server.crt" >/dev/null || {
  echo 'Fixture certificates expired; down then remove .local/rowboat-ssh before regeneration.' >&2; exit 1;
}
if ! podman image exists "$image"; then
  mkdir -p "$fixture/build"
  cat > "$fixture/build/Containerfile" <<'EOF'
FROM docker.io/library/postgres:18
RUN apt-get update && apt-get install -y --no-install-recommends openssh-server && rm -rf /var/lib/apt/lists/* && mkdir -p /run/sshd
ENTRYPOINT ["/usr/sbin/sshd", "-D", "-e", "-f", "/fixture/sshd_config"]
EOF
  podman build --pull=never --label io.rowboat.fixture=ssh -t "$image" "$fixture/build"
fi
[[ "$(podman image inspect --format '{{index .Labels "io.rowboat.fixture"}}' "$image")" == ssh ]] || { echo 'Refusing unrelated SSH image.' >&2; exit 1; }
if ! podman network exists "$network"; then podman network create --label io.rowboat.fixture=ssh "$network" >/dev/null; fi
[[ "$(podman network inspect --format '{{index .Labels "io.rowboat.fixture"}}' "$network")" == ssh ]] || { echo 'Refusing unrelated network.' >&2; exit 1; }
for engine in postgres mysql; do
  name="rowboat-test-$engine-ssh"
  if podman container exists "$name"; then
    podman start "$name" >/dev/null
  elif [[ "$engine" == postgres ]]; then
    podman run -d --pull=never --name "$name" --label io.rowboat.fixture=ssh --network "$network" \
      --network-alias postgres.rowboat.test --network-alias wrong-postgres.rowboat.test \
      -p 127.0.0.1:15434:5432 -e POSTGRES_PASSWORD=rowboat-test -e POSTGRES_DB=rowboat \
      --tmpfs /var/lib/postgresql --tmpfs /run/rowboat-tls -v "$fixture/server:/fixture:ro,z" \
      --entrypoint bash docker.io/library/postgres:18 -ec '
        cp /fixture/{ca.crt,server.crt,server.key,pg_hba.conf} /run/rowboat-tls/
        chown -R postgres:postgres /run/rowboat-tls; chmod 700 /run/rowboat-tls; chmod 600 /run/rowboat-tls/*
        exec docker-entrypoint.sh postgres -c ssl=on -c ssl_ca_file=/run/rowboat-tls/ca.crt \
          -c ssl_cert_file=/run/rowboat-tls/server.crt -c ssl_key_file=/run/rowboat-tls/server.key -c hba_file=/run/rowboat-tls/pg_hba.conf
      ' >/dev/null
  else
    podman run -d --pull=never --name "$name" --label io.rowboat.fixture=ssh --network "$network" \
      --network-alias mysql.rowboat.test --network-alias wrong-mysql.rowboat.test \
      -p 127.0.0.1:13308:3306 -e MYSQL_ROOT_PASSWORD=rowboat-test -e MYSQL_DATABASE=rowboat -e MYSQL_USER=rowboat -e MYSQL_PASSWORD=rowboat-test \
      --tmpfs /var/lib/mysql --tmpfs /run/rowboat-tls -v "$fixture/server:/fixture:ro,z" \
      --entrypoint bash docker.io/library/mysql:8.4 -ec '
        cp /fixture/{ca.crt,server.crt,server.key} /run/rowboat-tls/
        chown -R mysql:mysql /run/rowboat-tls; chmod 700 /run/rowboat-tls; chmod 600 /run/rowboat-tls/*
        exec docker-entrypoint.sh mysqld --require-secure-transport=ON \
          --ssl-ca=/run/rowboat-tls/ca.crt --ssl-cert=/run/rowboat-tls/server.crt --ssl-key=/run/rowboat-tls/server.key
      ' >/dev/null
  fi
done
if podman container exists rowboat-test-sshd; then podman start rowboat-test-sshd >/dev/null; else
  podman run -d --pull=never --name rowboat-test-sshd --label io.rowboat.fixture=ssh --network "$network" \
    --stop-signal SIGTERM -p 127.0.0.1:12222:22 -v "$fixture/sshd:/fixture:ro,z" "$image" >/dev/null
fi
ready=false
for attempt in {1..90}; do
  if timeout 5 podman exec rowboat-test-postgres-ssh pg_isready -U postgres -d rowboat >/dev/null 2>&1 &&
     timeout 5 podman exec -e MYSQL_PWD=rowboat-test rowboat-test-mysql-ssh mysql --connect-timeout=3 --ssl-mode=REQUIRED -h 127.0.0.1 -u rowboat -D rowboat -e 'SELECT 1' >/dev/null 2>&1 &&
     timeout 5 ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UpdateHostKeys=no \
       -o "UserKnownHostsFile=$fixture/client/known_hosts" -o GlobalKnownHostsFile=/dev/null -i "$fixture/client/identity" -p 12222 root@127.0.0.1 true >/dev/null 2>&1; then
    ready=true; break
  fi
  sleep 1
done
$ready || { echo 'SSH/TLS readiness timed out; inspect task-owned container logs.' >&2; exit 1; }
podman exec -i rowboat-test-postgres-ssh psql -v ON_ERROR_STOP=1 -U postgres -d rowboat >/dev/null <<'SQL'
DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname='rowboat_cert') THEN CREATE ROLE rowboat_cert LOGIN; END IF; END $$;
GRANT CONNECT ON DATABASE rowboat TO rowboat_cert;
SQL
podman exec -i -e MYSQL_PWD=rowboat-test rowboat-test-mysql-ssh mysql -u root >/dev/null <<'SQL'
CREATE USER IF NOT EXISTS 'rowboat_cert'@'%' IDENTIFIED BY 'rowboat-test' REQUIRE X509;
GRANT SELECT ON rowboat.* TO 'rowboat_cert'@'%';
SQL
echo 'SSH/TLS ready: SSH 12222, PostgreSQL 15434, MySQL 13308; remote-only DNS identities, no host trust/DNS changes.'
