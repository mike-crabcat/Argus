#!/usr/bin/env bash
# Installs the auto-deploy on this machine (no sudo):
#   ~/argus-deploy/{compose.yaml,argus-update.sh,.env}
#   ~/.config/systemd/user/argus-update.{service,timer}
# Needs `loginctl enable-linger $USER` once so the timer runs while logged
# out. Edit ~/argus-deploy/.env for the image, config and data paths.
set -euo pipefail
src=$(cd "$(dirname "$0")" && pwd)
dir=${ARGUS_DEPLOY_DIR:-$HOME/argus-deploy}
units=$HOME/.config/systemd/user
mkdir -p "$dir" "$units"
install -m 644 "$src/compose.yaml" "$dir/compose.yaml"
install -m 755 "$src/argus-update.sh" "$dir/argus-update.sh"
if [ ! -f "$dir/.env" ]; then
  cat > "$dir/.env" <<ENV
ARGUS_IMAGE=ghcr.io/mike-crabcat/argus:main
ARGUS_CONFIG=$HOME/.config/argus/argus.toml
ARGUS_DATA=/mnt/data3/argus-data
ENV
fi
install -m 644 "$src/argus-update.service" "$units/argus-update.service"
install -m 644 "$src/argus-update.timer" "$units/argus-update.timer"
systemctl --user daemon-reload
systemctl --user enable --now argus-update.timer
echo "installed to $dir; timer: systemctl --user list-timers argus-update.timer"
