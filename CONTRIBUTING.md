# Contributing to sheprd

Thanks for helping. sheprd is small on purpose: one Rust binary, one no-build web app, one Claude Code plugin.

## Layout

| Path | What |
|---|---|
| `server/` | Rust server (axum + tokio). `src/main.rs` is the HTTP API and state, `src/cli.rs` is setup/doctor, and each feature has its own module. |
| `web/` | The PWA: vanilla JS modules, no bundler, no framework. `build.rs` embeds it into the binary. |
| `mod/sheprd/` | The Claude Code plugin (`hooks/register.ts`), also embedded and installed by `sheprd setup`. |
| `server/data/activities.toml` | The taxonomy behind the "now doing" phrases. |
| `docs/API.md` | The HTTP API contract. Keep it in sync with the code. |

## Develop

```sh
# server + web from disk (edit web/*.js and reload; no rebuild needed)
cd server
SHEPRD_PORT=7899 SHEPRD_NO_PUSH=1 SHEPRD_STATE_DIR=/tmp/sheprd-dev SHEPRD_WEB_DIR=$PWD/../web cargo run

# tests
cargo test                     # server
claude plugin test mod/sheprd  # plugin (from the repo root)
for f in web/*.js; do node --check "$f"; done
```

Use a different port and `SHEPRD_NO_PUSH=1` for dev instances, so you never fight your real install over port
7878 or push to your phone by accident. To test with a throwaway herdr, start one under a temporary `HOME` and
point `HERDR_SOCKET_PATH` at its socket.

## Guidelines

- **Small and quiet.** The server runs all day next to your agents: no polling loops when no one is looking,
  no holding transcripts or images in memory, and no new dependencies without a good reason.
- **Phone first.** Every UI change should work one-handed on a small iPhone, in light and dark mode.
- **No telemetry, no accounts.** Nothing leaves the machine except Web Push.
- **Onboarding is a feature.** If something can fail during install, `sheprd doctor` should detect it and print the fix.
- Commit messages say what changed and why. PRs need `cargo test` passing.

## Releases

Push a `vX.Y.Z` tag. `.github/workflows/release.yml` builds Linux x86_64 and aarch64 binaries (OpenSSL vendored)
and attaches them to a GitHub release, which `install.sh` downloads.
