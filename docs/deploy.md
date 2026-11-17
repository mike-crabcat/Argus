# Deploying Argus on the workstation

Every merge to `main` deploys itself:

1. **CI** (`.github/workflows/ci.yml`) runs fmt, clippy and the tests.
2. If CI passed on a push to `main`, **Docker image**
   (`.github/workflows/docker.yml`) builds the image and pushes it to
   `ghcr.io/mike-crabcat/argus` as `main` and `sha-<short>`. It never runs
   for pull requests.
3. On the workstation, a systemd user timer runs
   `~/argus-deploy/argus-update.sh` every 2 minutes: it pulls the image,
   and if it changed, restarts the container and waits for it to report
   healthy. If it doesn't, it goes back to the previous image and doesn't
   try that one again.

Results go to `~/argus-deploy/deploy.log`, one line per deploy:

```text
2026-11-17T18:40:02+08:00 deployed 4704d7a -> c5dace1 ok
```

## What runs

| Piece | Where |
| --- | --- |
| Container | `argus` (Docker Compose project `argus`), `restart: unless-stopped`, host networking |
| Config | `~/.config/argus/argus.toml`, mounted read-only; never in the image |
| Recordings | `/mnt/data3/argus-data`, capped by `[storage] local_budget_gb` |
| Viewer | `http://127.0.0.1:8080/?token=…` (token in the config and in `~/.config/argus/token`) |
| Deploy files | `~/argus-deploy/` (`compose.yaml`, `.env`, `argus-update.sh`, `deploy.log`) |
| Timer | `systemctl --user status argus-update.timer` |

The image is a slim Debian with the `argus` binary and the browser app
(`/usr/share/argus/web`), running as uid 1000. Its `HEALTHCHECK` runs
`argus healthcheck`, which asks the server's `/healthz`.

## Setup (once)

```sh
loginctl enable-linger $USER     # timers run while logged out
deploy/install.sh                # files to ~/argus-deploy, enables the timer
# write ~/.config/argus/argus.toml (see argus.example.toml), chmod 600
docker compose --project-directory ~/argus-deploy --env-file ~/argus-deploy/.env up -d
```

For HTTPS from other devices (browsers only decode video in a secure
context): `tailscale serve --bg https / http://127.0.0.1:8080`.

The GHCR package must be readable by the workstation: either make the
package public (the repository already is), or `docker login ghcr.io` there
with a read-only token.

## By hand

```sh
cd ~/argus-deploy
docker compose --env-file .env ps                     # state and health
docker compose --env-file .env logs -f --tail 100     # server log
./argus-update.sh                                     # check for a new image now
tail deploy.log
```

Roll back to the image before the last deploy:

```sh
docker tag argus:previous ghcr.io/mike-crabcat/argus:main
docker compose --env-file .env up -d
```

To run an image built here instead (`docker build -t argus:local .`), set
`ARGUS_IMAGE=argus:local` in `.env`; the timer leaves local images alone.
