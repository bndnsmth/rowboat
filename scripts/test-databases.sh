#!/usr/bin/env bash
set -euo pipefail

# Only the new Rowboat cohort is owned here. Legacy base fixtures keep their
# original names/ports and are never lifecycle targets of this script.
owned() {
  test "$(podman inspect --format '{{index .Config.Labels "io.rowboat.test"}}' "$1")" = true
}

case "${1:-up}" in
  up)
    [[ "$(podman info --format '{{.Host.Security.Rootless}}')" == true ]] || {
      echo 'Base fixtures require rootless Podman.' >&2; exit 1;
    }
    for image in docker.io/library/postgres:18 docker.io/library/mysql:8.4; do
      podman image exists "$image" || { echo "Required cached image missing: $image" >&2; exit 1; }
    done
    for name in rowboat-test-postgres rowboat-test-mysql; do
      if podman container exists "$name"; then
        owned "$name" || { echo "Refusing to use unrelated container $name" >&2; exit 1; }
      fi
    done
    for engine in postgres mysql; do
      name="rowboat-test-$engine"
      if podman container exists "$name"; then
        owned "$name" || { echo "Refusing to use unrelated container $name" >&2; exit 1; }
        podman start "$name" >/dev/null
      elif [[ "$engine" == postgres ]]; then
        podman run -d --name "$name" --label io.rowboat.test=true \
          -p 127.0.0.1:15435:5432 -e POSTGRES_PASSWORD=rowboat-test -e POSTGRES_DB=rowboat \
          --tmpfs /var/lib/postgresql docker.io/library/postgres:18 >/dev/null
      else
        podman run -d --name "$name" --label io.rowboat.test=true \
          -p 127.0.0.1:13309:3306 -e MYSQL_ROOT_PASSWORD=rowboat-test \
          -e MYSQL_DATABASE=rowboat -e MYSQL_USER=rowboat -e MYSQL_PASSWORD=rowboat-test \
          --tmpfs /var/lib/mysql docker.io/library/mysql:8.4 >/dev/null
      fi
    done
    ready=false
    for attempt in {1..90}; do
      if podman exec rowboat-test-postgres pg_isready -U postgres -d rowboat >/dev/null 2>&1 && \
         podman exec -e MYSQL_PWD=rowboat-test rowboat-test-mysql mysql -h 127.0.0.1 -u rowboat -D rowboat -e 'SELECT 1' >/dev/null 2>&1; then
        ready=true
        break
      fi
      sleep 1
    done
    $ready || { echo 'Database readiness timed out; inspect podman logs.' >&2; exit 1; }
    echo 'PostgreSQL 127.0.0.1:15435 database=rowboat user=postgres password=rowboat-test'
    echo 'MySQL      127.0.0.1:13309 database=rowboat user=rowboat     password=rowboat-test'
    ;;
  down)
    for name in rowboat-test-postgres rowboat-test-mysql; do
      if podman container exists "$name"; then
        owned "$name" || { echo "Refusing to remove unrelated container $name" >&2; exit 1; }
        podman rm -f "$name"
      fi
    done
    ;;
  *) echo "Usage: $0 [up|down]" >&2; exit 2 ;;
esac
