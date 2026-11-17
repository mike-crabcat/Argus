#!/usr/bin/env bash
# Pulls the Argus image and redeploys when it changed. Run by the
# argus-update.timer every 2 minutes. On a failed health check it goes back
# to the previous image and remembers the bad one so it isn't retried.
#
# Log: ~/argus-deploy/deploy.log (time, old -> new revision, result).
set -euo pipefail

dir=${ARGUS_DEPLOY_DIR:-$HOME/argus-deploy}
cd "$dir"
exec 9>"$dir/.lock"
flock -n 9 || exit 0

log() { printf '%s %s\n' "$(date -Is)" "$*" >> "$dir/deploy.log"; }
compose() { docker compose --project-directory "$dir" -f "$dir/compose.yaml" --env-file "$dir/.env" "$@"; }
rev() {
  docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$1" 2>/dev/null \
    | cut -c1-7
}

image=$(compose config --images | head -n1)
case "$image" in
  */*/*) ;;   # a registry image: keep it up to date
  *) exit 0 ;; # a locally built image: nothing to pull
esac

running=$(docker inspect -f '{{.Image}}' argus 2>/dev/null || true)
if ! compose pull -q argus 2>>"$dir/pull-errors.log"; then
  log "pull failed (see pull-errors.log)"
  exit 1
fi
new=$(docker image inspect -f '{{.Id}}' "$image")
[ "$new" = "$running" ] && exit 0
if grep -qxF "$new" "$dir/bad-images" 2>/dev/null; then
  exit 0
fi

old_rev=$(rev "$running" || true)
new_rev=$(rev "$new" || true)
[ -n "$running" ] && docker tag "$running" argus:previous
compose up -d argus

status=starting
for _ in $(seq 1 40); do
  status=$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' argus 2>/dev/null || echo missing)
  [ "$status" = healthy ] && break
  [ "$status" = unhealthy ] && break
  sleep 3
done

if [ "$status" = healthy ]; then
  log "deployed ${old_rev:-none} -> ${new_rev:-?} ok"
  docker image prune -f >/dev/null || true
  exit 0
fi

log "deploy ${new_rev:-?} failed health check ($status); rolling back to ${old_rev:-none}"
echo "$new" >> "$dir/bad-images"
if [ -n "$running" ]; then
  docker tag argus:previous "$image"
  compose up -d argus
  log "rolled back to ${old_rev:-?}"
fi
exit 1
