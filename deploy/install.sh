#!/usr/bin/env bash
# Build, test and (re)install the sheprd server as a systemd user service.
#
# Always builds committed HEAD in a throwaway git worktree, so uncommitted work
# in the repo (yours or another agent's) can never ship, and installs exactly
# the binary that build produced.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
target="${SHEPRD_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/sheprd/target}"
wt="$(mktemp -d "${TMPDIR:-/tmp}/sheprd-build.XXXXXX")"
cleanup() { git -C "$repo" worktree remove --force "$wt" >/dev/null 2>&1 || rm -rf "$wt"; }
trap cleanup EXIT

git -C "$repo" worktree add --quiet --detach "$wt" HEAD
echo "building $(git -C "$repo" log -1 --format='%h %s')"
cd "$wt/server"
CARGO_TARGET_DIR="$target" cargo build --release
CARGO_TARGET_DIR="$target" cargo test --release --quiet

install -Dm755 "$target/release/sheprd" ~/.local/bin/sheprd
install -Dm644 "$repo/deploy/sheprd.service" ~/.config/systemd/user/sheprd.service
systemctl --user daemon-reload
systemctl --user enable sheprd.service >/dev/null 2>&1
systemctl --user restart sheprd.service
sleep 0.5
systemctl --user --no-pager status sheprd.service | head -5
