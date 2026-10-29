#!/bin/sh
# Install ottobot's `otto` CLI (and optionally the macOS tray app) from GitHub Releases.
#   curl -fsSL https://raw.githubusercontent.com/bryan-uipath/ottobot/main/install.sh | sh
# Everything runs from main(), called on the last line, so a truncated download does nothing.
set -eu

REPO=bryan-uipath/ottobot
APP_NAME="ottobot tray.app"
PLIST="$HOME/Library/LaunchAgents/com.ottobot.daemon.plist"
SKILLS="$HOME/.ottobot/skills"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'USAGE'
Usage: install.sh [--tray] [--uninstall]
  curl -fsSL https://raw.githubusercontent.com/bryan-uipath/ottobot/main/install.sh | sh -s -- [flags]

  --tray        also install ~/Applications/ottobot tray.app (macOS)
  --uninstall   remove otto and the tray app; keeps ~/.ottobot

Env: OTTO_VERSION=v0.2.0   release tag (default: latest)
     OTTO_INSTALL_DIR      binary dir (default: ~/.local/bin)
     OTTO_TRAY=1           same as --tray
     OTTO_DRY_RUN=1        print URLs and paths, change nothing
USAGE
}

# fetch URL FILE ("-" = stdout).
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  fetch() { die "need curl or wget"; }
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha_check() { sha256sum -c -; }
else
  sha_check() { shasum -a 256 -c -; }
fi

# Download $1 into $tmp and verify it against SHA256SUMS.
get() {
  say "downloading $base/$1"
  fetch "$base/$1" "$tmp/$1" || die "download failed: $base/$1"
  (cd "$tmp" && awk -v f="$1" '$2 == f' SHA256SUMS | sha_check >/dev/null) || die "checksum mismatch: $1"
}

uninstall() {
  if [ "$DRY" = 1 ]; then
    [ -f "$PLIST" ] && say "would run   $BIN_DIR/otto daemon --uninstall"
    say "would remove $BIN_DIR/otto"
    say "would remove $APP_DIR/$APP_NAME"
    say "would keep   ~/.ottobot"
    exit 0
  fi
  # Stop the LaunchAgent first, else launchd keeps relaunching a missing binary.
  if [ -f "$PLIST" ] && [ -x "$BIN_DIR/otto" ]; then
    "$BIN_DIR/otto" daemon --uninstall || true
  fi
  rm -f "$BIN_DIR/otto" && say "removed $BIN_DIR/otto"
  if [ -d "$APP_DIR/$APP_NAME" ]; then rm -rf "${APP_DIR:?}/$APP_NAME" && say "removed $APP_DIR/$APP_NAME"; fi
  say "kept ~/.ottobot (skills, state, run history); remove it with: rm -rf ~/.ottobot"
  exit 0
}

main() {
  BIN_DIR=${OTTO_INSTALL_DIR:-"$HOME/.local/bin"}
  APP_DIR="$HOME/Applications"
  TRAY=${OTTO_TRAY:-0}
  DRY=${OTTO_DRY_RUN:-0}
  UNINSTALL=0

  for a in "$@"; do
    case "$a" in
      --tray) TRAY=1 ;;
      --uninstall) UNINSTALL=1 ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; die "unknown argument: $a" ;;
    esac
  done

  [ "$UNINSTALL" = 1 ] && uninstall

  os=$(uname -s)
  case "$os" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-musl ;;
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
  [ "$target" = aarch64-unknown-linux-musl ] && die "no prebuilt binary for $target; build from source: cargo install --git https://github.com/$REPO"
  [ "$TRAY" = 1 ] && [ "$os" != apple-darwin ] && die "--tray is macOS-only"

  api="https://api.github.com/repos/$REPO/releases/latest"
  version=${OTTO_VERSION:-}
  if [ -z "$version" ] && [ "$DRY" != 1 ]; then
    version=$(fetch "$api" - | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
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
    [ -z "$(ls -A "$SKILLS" 2>/dev/null)" ] && say "example   $SKILLS/hello.md"
    exit 0
  fi

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  trap 'exit 1' INT TERM

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
    # Not notarized: strip quarantine so Gatekeeper doesn't block first launch.
    xattr -dr com.apple.quarantine "$APP_DIR/$APP_NAME" 2>/dev/null || true
    say "installed $APP_DIR/$APP_NAME (launch: open \"$APP_DIR/$APP_NAME\"; needs the daemon)"
  fi

  # Seed an empty skills dir with the smoke-test example.
  example=""
  if [ -z "$(ls -A "$SKILLS" 2>/dev/null)" ]; then
    mkdir -p "$SKILLS"
    cp "$tmp/otto-$target/examples/skills/hello.md" "$SKILLS/"
    example="$SKILLS/hello.md"
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
  say "  otto --usage                 # check claude/codex are found"
  if [ -n "$example" ]; then
    say "  # $example fires every 2 minutes (haiku); delete it when done"
  fi
  if [ -f "$PLIST" ] && ! grep -qF "$BIN_DIR/otto<" "$PLIST"; then
    say "  otto daemon --install        # LaunchAgent points at another otto; repoint it"
  elif [ -f "$PLIST" ]; then
    say "  launchctl kickstart -k gui/\$(id -u)/com.ottobot.daemon   # restart the daemon on the new binary"
  elif [ "$os" = apple-darwin ]; then
    say "  otto daemon --install        # run the scheduler at login"
  fi
  say "  otto                         # TUI"
}

main "$@"
