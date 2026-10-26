#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
container=rowboat-launch-postgres
volume=rowboat-launch-postgres-data
image=docker.io/library/postgres:18

fail() { echo "launch-demo: $*" >&2; exit 1; }
command -v podman >/dev/null || fail 'Podman is required.'
[[ "$(podman info --format '{{.Host.Security.Rootless}}')" == true ]] || fail 'Use rootless Podman.'

check_volume() {
  [[ "$(podman volume inspect --format '{{index .Labels "io.rowboat.launch"}}' "$volume")" == true ]] || fail "Unowned volume: $volume"
  [[ "$(podman volume inspect --format '{{index .Labels "io.rowboat.launch.project"}}' "$volume")" == "$root" ]] || fail "Volume belongs to another checkout: $volume"
}

check_container() {
  [[ "$(podman inspect --format '{{index .Config.Labels "io.rowboat.launch"}}' "$container")" == true ]] || fail "Unowned container: $container"
  [[ "$(podman inspect --format '{{index .Config.Labels "io.rowboat.launch.project"}}' "$container")" == "$root" ]] || fail "Container belongs to another checkout: $container"
  [[ "$(podman inspect --format '{{.ImageName}}' "$container")" == "$image" ]] || fail 'Unexpected container image.'
  [[ "$(podman inspect --format '{{range (index .HostConfig.PortBindings "5432/tcp")}}{{.HostIp}}:{{.HostPort}}{{end}}' "$container")" == 127.0.0.1:15436 ]] || fail 'Unexpected PostgreSQL port binding.'
  [[ "$(podman inspect --format '{{range .Mounts}}{{if eq .Destination "/var/lib/postgresql"}}{{.Name}}{{end}}{{end}}' "$container")" == "$volume" ]] || fail 'Unexpected PostgreSQL data mount.'
  check_volume
}

psql_demo() {
  podman exec -i -e PGPASSWORD=rowboat-demo "$container" \
    psql -X -v ON_ERROR_STOP=1 -h 127.0.0.1 -U rowboat -d northstar "$@"
}

summary() {
  psql_demo -P pager=off -c "SELECT current_database() AS database,
    (SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE') AS tables,
    count(*) AS orders, min(placed_at)::date AS earliest_order, max(placed_at)::date AS latest_order
    FROM public.orders;"
  echo 'Northstar Supply — fictional launch fixture'
  echo 'Host: 127.0.0.1  Port: 15436  Database: northstar  User: rowboat'
  echo 'Password: rowboat-demo (fixture only)  TLS: disable'
  echo "Persistent volume: $volume"
}

up() {
  podman image exists "$image" || fail "Required cached image missing: $image (no automatic pull)."
  if podman container exists "$container"; then
    check_container
    if [[ "$(podman inspect --format '{{.State.Running}}' "$container")" != true ]]; then
      podman start "$container" >/dev/null
    fi
  else
    if podman volume exists "$volume"; then
      check_volume
    else
      podman volume create --label io.rowboat.launch=true \
        --label "io.rowboat.launch.project=$root" "$volume" >/dev/null
    fi
    # Podman fails rather than taking over an occupied name or listening port.
    podman run -d --pull=never --name "$container" \
      --label io.rowboat.launch=true --label "io.rowboat.launch.project=$root" \
      -p 127.0.0.1:15436:5432 \
      -e POSTGRES_USER=rowboat -e POSTGRES_PASSWORD=rowboat-demo -e POSTGRES_DB=northstar \
      -v "$volume:/var/lib/postgresql:Z" "$image" >/dev/null
  fi
  local ready=false
  for _ in {1..90}; do
    # TCP excludes the image's temporary socket-only initialization server.
    if psql_demo -qAt -c 'SELECT 1' >/dev/null 2>&1; then
      ready=true
      break
    fi
    sleep 1
  done
  "$ready" || fail "PostgreSQL readiness timed out; inspect podman logs $container. Data was preserved."
  psql_demo -q < "$root/demo/launch/seed.sql"
  summary
}

case "${1:-up}" in
  up)
    [[ $# -le 1 ]] || fail "Usage: $0 [up|stop|status]"
    up
    ;;
  stop)
    [[ $# -eq 1 ]] || fail "Usage: $0 [up|stop|status]"
    if podman container exists "$container"; then
      check_container
      podman stop --time 30 "$container"
    else
      echo 'Launch container does not exist.'
    fi
    ;;
  status)
    [[ $# -eq 1 ]] || fail "Usage: $0 [up|stop|status]"
    if podman container exists "$container"; then
      check_container
      podman inspect --format 'Container: {{.Name}}  State: {{.State.Status}}' "$container"
      if [[ "$(podman inspect --format '{{.State.Running}}' "$container")" == true ]]; then
        summary
      fi
    else
      echo 'Launch container does not exist.'
      if podman volume exists "$volume"; then
        check_volume
        echo "Retained data volume: $volume"
      fi
    fi
    ;;
  *) fail "Usage: $0 [up|stop|status]" ;;
esac
