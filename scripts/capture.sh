#!/usr/bin/env bash
# Screenshots and a screen recording of the Argus web viewer, for sharing
# progress. Starts argus with your argus.toml's cameras against a scratch
# storage root (your real recordings and settings are untouched), opens the
# viewer in Chrome and saves grid.png, focus.png, viewer.webm (+ viewer.mp4
# if ffmpeg is installed) and summary.json.
#
#   scripts/capture.sh [NAME] [-- extra capture.mjs args, e.g. --focus back]
#
# Output: ${ARGUS_CAPTURE_DIR:-$HOME/argus-captures}/YYYY-MM-DD/NAME/
# It is deliberately outside the repository: captures show your cameras,
# and this repository is public. Never commit them.
#
# Needs: a built web app (scripts/build-web.sh), Node with Playwright
# (npm i -g playwright, or NODE_PATH), and argus.toml in the repo root.
#
# To capture a server that is already running (e.g. the deployed one)
# instead, set ARGUS_CAPTURE_URL=http://127.0.0.1:8080 and
# ARGUS_CAPTURE_TOKEN_FILE=~/.config/argus/token.
#
# Chrome opens a window on the desktop when there is a display, so the real
# GPU is used; set ARGUS_CAPTURE_HEADLESS=1 to stay headless.
set -euo pipefail
cd "$(dirname "$0")/.."

name=${1:-capture}
shift || true
[ "${1:-}" = "--" ] && shift
out="${ARGUS_CAPTURE_DIR:-$HOME/argus-captures}/$(date +%F)/$name"
port=${ARGUS_CAPTURE_PORT:-18090}
scratch=$(mktemp -d)
pid=
trap '[ -n "$pid" ] && { kill -INT "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }; rm -rf "$scratch"' EXIT


# Find Playwright: a normal install, NODE_PATH, or npx's cache.
if ! node -e 'require.resolve("playwright")' 2>/dev/null; then
  for d in "$HOME"/.npm/_npx/*/node_modules; do
    if [ -d "$d/playwright" ]; then export NODE_PATH="$d"; break; fi
  done
fi
node -e 'require.resolve("playwright")' 2>/dev/null || {
  echo "Playwright not found: npm i -g playwright (or set NODE_PATH)" >&2; exit 1; }

if [ -n "${ARGUS_CAPTURE_URL:-}" ]; then
  token=$(cat "${ARGUS_CAPTURE_TOKEN_FILE:?set ARGUS_CAPTURE_TOKEN_FILE}")
  mkdir -p "$out"
  node tools/capture/capture.mjs "$ARGUS_CAPTURE_URL/?token=$token" "$out" "$@"
  if command -v ffmpeg >/dev/null && [ -f "$out/viewer.webm" ]; then
    ffmpeg -v error -y -i "$out/viewer.webm" -c:v libx264 -preset veryfast -crf 26 \
      -pix_fmt yuv420p -movflags +faststart "$out/viewer.mp4"
  fi
  ls -l "$out"
  exit 0
fi

[ -f web/dist/index.html ] || { echo "build the web app first: scripts/build-web.sh" >&2; exit 1; }
[ -f argus.toml ] || { echo "argus.toml not found" >&2; exit 1; }

# A throwaway config: your cameras, a scratch root, the viewer on localhost
# with a random token. No [s3]: nothing is uploaded.
token=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
{
  printf '[storage]\nroot = "%s/data"\nchunk_seconds = 60\n\n' "$scratch"
  printf '[http]\nlisten = "127.0.0.1:%s"\ntoken = "%s"\nweb_root = "%s/web/dist"\n\n' "$port" "$token" "$PWD"
  sed -n '/^\[\[camera\]\]/,$p' argus.toml
} > "$scratch/argus.toml"
chmod 600 "$scratch/argus.toml"

cargo build -q --release -p argus-server
target/release/argus -c "$scratch/argus.toml" > "$scratch/argus.log" 2>&1 &
pid=$!
for _ in $(seq 1 50); do
  curl -sf -o /dev/null -H "Authorization: Bearer $token" "http://127.0.0.1:$port/api/cameras" && break
  sleep 0.2
done

mkdir -p "$out"
NODE_PATH=${NODE_PATH:-} node tools/capture/capture.mjs \
  "http://127.0.0.1:$port/?token=$token" "$out" "$@"

if command -v ffmpeg >/dev/null && [ -f "$out/viewer.webm" ]; then
  ffmpeg -v error -y -i "$out/viewer.webm" -c:v libx264 -preset veryfast -crf 26 \
    -pix_fmt yuv420p -movflags +faststart "$out/viewer.mp4"
fi
ls -l "$out"
