# Argus: notes for Claude

## Branches and pushes

- Name branches `feat/…`, `fix/…`, `docs/…` or `chore/…` (e.g. `feat/web-viewer`).
  Never use a `claude/` prefix.
- When a commit is finished and checks pass (`cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test`), push it to its
  branch without asking.
- Never push to `main` and never force-push. Don't open pull requests unless
  asked; Mike reviews and merges them.

## Screenshots and videos

After each visible milestone, run `scripts/capture.sh <short-name>` and
share the results with Mike through the private "Argus progress" page on
claude.ai (not GitHub). Captures land outside the repo, in
`~/argus-captures/YYYY-MM-DD/<name>/`.

**Never commit camera images or video to this repository: it is public.**
Captures show Mike's home, family and car.

## Secrets

`argus.toml` holds camera passwords and is gitignored. Never print passwords,
tokens or full camera URLs; redact them as `***`.
