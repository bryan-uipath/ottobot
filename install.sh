#!/bin/sh
# Install ottobot's `otto` CLI (and optionally the macOS tray app) from GitHub Releases.
#   curl -fsSL https://raw.githubusercontent.com/bryan-uipath/ottobot/main/install.sh | sh
#   ... | sh -s -- --tray        also install ~/Applications/ottobot tray.app (macOS)
#   ... | sh -s -- --uninstall   remove both; keeps ~/.ottobot
# Env: OTTO_VERSION=v0.2.0 (default latest), OTTO_INSTALL_DIR (default ~/.local/bin),
#      OTTO_TRAY=1 (same as --tray), OTTO_DRY_RUN=1 (print URLs/paths, change nothing).
set -eu

REPO=bryan-uipath/ottobot
APP_NAME="ottobot tray.app"
BIN_DIR=${OTTO_INSTALL_DIR:-"$HOME/.local/bin"}
APP_DIR="$HOME/Applications"
TRAY=${OTTO_TRAY:-0}
DRY=${OTTO_DRY_RUN:-0}
UNINSTALL=0

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

for a in "$@"; do
  case "$a" in
    --tray) TRAY=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h | --help) sed -n '2,7s/^# \{0,1\}//p' "$0" 2>/dev/null || true; exit 0 ;;
    *) die "unknown argument: $a" ;;
  esac
done

os=$(uname -s)
case "$os" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) die "unsupported OS: $os (macOS and Linux only)" ;;
esac
arch=$(uname -m)
case "$arch" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) die "unsupported architecture: $arch" ;;
esac
target="$arch-$os"
# Release assets: aarch64/x86_64 macOS, x86_64 Linux.
[ "$target" = aarch64-unknown-linux-gnu ] && die "no prebuilt binary for $target; build from source: cargo install --git https://github.com/$REPO"
[ "$TRAY" = 1 ] && [ "$os" != apple-darwin ] && die "--tray is macOS-only"

if [ "$UNINSTALL" = 1 ]; then
  # Stop the LaunchAgent first, else launchd keeps relaunching a missing binary.
  if [ "$os" = apple-darwin ] && [ -f "$HOME/Library/LaunchAgents/com.ottobot.daemon.plist" ] && [ -x "$BIN_DIR/otto" ]; then
    "$BIN_DIR/otto" daemon --uninstall || true
  fi
  rm -f "$BIN_DIR/otto" && say "removed $BIN_DIR/otto"
  if [ -d "$APP_DIR/$APP_NAME" ]; then rm -rf "${APP_DIR:?}/$APP_NAME" && say "removed $APP_DIR/$APP_NAME"; fi
  say "kept ~/.ottobot (skills, state, run history); remove it with: rm -rf ~/.ottobot"
  exit 0
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL -o "$2" "$1"; }
  fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
  fetch_stdout() { wget -qO- "$1"; }
else
  die "need curl or wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha_check() { sha256sum -c -; }
elif command -v shasum >/dev/null 2>&1; then
  sha_check() { shasum -a 256 -c -; }
else
  die "need sha256sum or shasum"
fi

api="https://api.github.com/repos/$REPO/releases/latest"
version=${OTTO_VERSION:-}
if [ -z "$version" ] && [ "$DRY" != 1 ]; then
  version=$(fetch_stdout "$api" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$version" ] || die "could not resolve latest release from $api (set OTTO_VERSION=vX.Y.Z)"
fi
base="https://github.com/$REPO/releases/download/${version:-<latest>}"
tarball="otto-$target.tar.gz"
appzip="ottobot-tray-$arch.app.zip"

if [ "$DRY" = 1 ]; then
  say "target    $target"
  say "version   ${version:-latest (resolved via $api)}"
  say "binary    $base/$tarball"
  say "checksums $base/SHA256SUMS"
  [ "$TRAY" = 1 ] && say "tray      $base/$appzip -> $APP_DIR/$APP_NAME"
  say "install   $BIN_DIR/otto"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM

# Download $1 into $tmp and verify it against SHA256SUMS.
get() {
  say "downloading $base/$1"
  fetch "$base/$1" "$tmp/$1" || die "download failed: $base/$1"
  (cd "$tmp" && awk -v f="$1" '$2 == f' SHA256SUMS | sha_check >/dev/null) || die "checksum mismatch: $1"
}

fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS (does $version exist?)"
get "$tarball"
tar -xzf "$tmp/$tarball" -C "$tmp"
mkdir -p "$BIN_DIR"
install -m 755 "$tmp/otto-$target/otto" "$BIN_DIR/otto"
say "installed $BIN_DIR/otto ($version)"

if [ "$TRAY" = 1 ]; then
  get "$appzip"
  mkdir -p "$APP_DIR"
  rm -rf "${APP_DIR:?}/$APP_NAME"
  ditto -x -k "$tmp/$appzip" "$APP_DIR"
  # Unsigned build: strip quarantine so Gatekeeper doesn't block first launch.
  xattr -dr com.apple.quarantine "$APP_DIR/$APP_NAME" 2>/dev/null || true
  say "installed $APP_DIR/$APP_NAME (launch: open \"$APP_DIR/$APP_NAME\"; needs the daemon)"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    rc="$HOME/.bashrc"
    case "${SHELL:-}" in */zsh) rc="$HOME/.zshrc" ;; esac
    say ""
    say "$BIN_DIR is not on PATH. Add it:"
    say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> $rc && . $rc"
    ;;
esac

say ""
say "next:"
say "  otto --usage                                   # check claude/codex are found"
say "  mkdir -p ~/.ottobot/skills"
say "  curl -fsSL https://raw.githubusercontent.com/$REPO/$version/examples/skills/hello.md -o ~/.ottobot/skills/hello.md"
[ "$os" = apple-darwin ] && say "  otto daemon --install                          # run the scheduler at login"
say "  otto                                           # TUI"
