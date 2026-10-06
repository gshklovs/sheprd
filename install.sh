#!/bin/sh
# sheprd installer: downloads the prebuilt binary (or builds it with cargo), then runs `sheprd setup`.
#
#   curl -fsSL https://raw.githubusercontent.com/gshklovski/sheprd/main/install.sh | sh
#
# Options (environment):
#   SHEPRD_VERSION=v0.1.0       a specific release (default: latest)
#   SHEPRD_BIN_DIR=~/.local/bin where the binary goes
#   SHEPRD_NO_SETUP=1           only install the binary
#   SHEPRD_FROM_SOURCE=1        build with cargo instead of downloading
# Arguments after `sh -s --` go to `sheprd setup` (e.g. `| sh -s -- --no-tailscale`).
set -eu

REPO="${SHEPRD_REPO:-gshklovski/sheprd}"
VERSION="${SHEPRD_VERSION:-latest}"
BIN_DIR="${SHEPRD_BIN_DIR:-$HOME/.local/bin}"

say() { printf '\033[1msheprd:\033[0m %s\n' "$*"; }
die() { printf '\033[31msheprd: %s\033[0m\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() { # url dest
  if have curl; then curl -fsSL --retry 2 -o "$2" "$1"
  elif have wget; then wget -qO "$2" "$1"
  else die "need curl or wget"; fi
}

case "$(uname -s)" in
  Linux) os=unknown-linux-gnu ;;
  *) os="" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) arch="" ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$BIN_DIR"

from_source() {
  have cargo || die "${1:-no prebuilt binary for $(uname -s)/$(uname -m)}, and no cargo to build one (install Rust: https://rustup.rs)"
  say "building from source with cargo (a few minutes)…"
  if [ -n "${SHEPRD_SRC:-}" ]; then
    cargo install --locked --path "$SHEPRD_SRC/server" --root "$tmp/root"
  else
    cargo install --locked --git "https://github.com/$REPO" sheprd --root "$tmp/root"
  fi
  install -m 755 "$tmp/root/bin/sheprd" "$BIN_DIR/sheprd"
}

if [ -z "${SHEPRD_FROM_SOURCE:-}" ] && [ -n "$os" ] && [ -n "$arch" ]; then
  target="$arch-$os"
  if [ "$VERSION" = latest ]; then base="https://github.com/$REPO/releases/latest/download"
  else base="https://github.com/$REPO/releases/download/$VERSION"; fi
  base="${SHEPRD_DOWNLOAD_BASE:-$base}"
  asset="sheprd-$target.tar.gz"
  say "downloading $asset…"
  if fetch "$base/$asset" "$tmp/$asset"; then
    if fetch "$base/$asset.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
      want="$(cut -d' ' -f1 < "$tmp/$asset.sha256")"
      if have sha256sum; then got="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"; else got="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"; fi
      [ "$want" = "$got" ] || die "checksum mismatch for $asset"
    fi
    tar -xzf "$tmp/$asset" -C "$tmp"
    install -m 755 "$tmp/sheprd" "$BIN_DIR/sheprd"
  else
    say "couldn't download $base/$asset; building from source instead"
    from_source "couldn't download $asset"
  fi
else
  from_source
fi

say "installed $("$BIN_DIR/sheprd" version) to $BIN_DIR/sheprd"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "note: $BIN_DIR is not on your PATH; add this to your shell rc:"
     printf '    export PATH="%s:$PATH"\n' "$BIN_DIR" ;;
esac

[ -n "${SHEPRD_NO_SETUP:-}" ] && exit 0
echo
# piped into sh, stdin is this script: give setup the terminal so it can ask and wait for the phone
if [ -t 1 ] && [ -r /dev/tty ] && (: < /dev/tty) 2>/dev/null; then
  exec "$BIN_DIR/sheprd" setup "$@" < /dev/tty
else
  exec "$BIN_DIR/sheprd" setup --yes --no-wait "$@"
fi
