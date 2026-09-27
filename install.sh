#!/usr/bin/env sh
# install.sh — put quotad / quota / quota-ctl on PATH.
#
# Smallest honest path:
#   ./install.sh                 # cargo build --release, copy into ~/.local/bin
#   ./install.sh --minimal       # quotad + quota only (no OpenBao / ctl)
#   ./install.sh --prefix DIR    # install into DIR/bin
#   ./install.sh --from-release  # fetch a GitHub release if one exists; else build
#
# No credentials are read. Socket defaults stay in the binaries
# (QUOTA_SOCKET → config.json → $XDG_RUNTIME_DIR/quota/quota.sock →
# ~/.local/share/quota/quota.sock). See docs/AGENT.md.

set -eu

REPO="op0ai/infer-quota"
PREFIX="${HOME:-}/.local"
MINIMAL=0
FROM_RELEASE=0
BINS="quotad quota quota-ctl"

usage() {
  sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)
      PREFIX="${2:-}"
      [ -n "$PREFIX" ] || usage
      shift 2
      ;;
    --minimal)
      MINIMAL=1
      BINS="quotad quota"
      shift
      ;;
    --from-release)
      FROM_RELEASE=1
      shift
      ;;
    -h|--help)
      usage
      ;;
    *)
      echo "install.sh: unknown arg $1" >&2
      usage
      ;;
  esac
done

if [ -z "${PREFIX}" ]; then
  echo "install.sh: PREFIX is empty (HOME unset?). Pass --prefix DIR." >&2
  exit 1
fi

BINDIR="${PREFIX}/bin"
mkdir -p "${BINDIR}"

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$root"

have_bins() {
  for b in $BINS; do
    [ -x "target/release/$b" ] || return 1
  done
  return 0
}

copy_bins() {
  for b in $BINS; do
    src="target/release/$b"
    if [ ! -x "$src" ]; then
      echo "install.sh: missing $src" >&2
      exit 1
    fi
    cp -f "$src" "${BINDIR}/$b"
    chmod 755 "${BINDIR}/$b"
    echo "installed ${BINDIR}/$b"
  done
}

fetch_release() {
  command -v curl >/dev/null 2>&1 || return 1
  uname_s=$(uname -s)
  uname_m=$(uname -m)
  case "${uname_s}-${uname_m}" in
    Linux-x86_64) triple="x86_64-unknown-linux-gnu" ;;
    Linux-aarch64) triple="aarch64-unknown-linux-gnu" ;;
    Darwin-arm64) triple="aarch64-apple-darwin" ;;
    Darwin-x86_64) triple="x86_64-apple-darwin" ;;
    *)
      echo "install.sh: no release triple for ${uname_s}-${uname_m}" >&2
      return 1
      ;;
  esac
  url="https://github.com/${REPO}/releases/latest/download/infer-quota-${triple}.tar.gz"
  tmp=$(mktemp -d)
  if ! curl -fsSL "$url" -o "$tmp/pack.tgz"; then
    rm -rf "$tmp"
    return 1
  fi
  tar -C "$tmp" -xzf "$tmp/pack.tgz"
  mkdir -p target/release
  for b in $BINS; do
    found=$(find "$tmp" -type f -name "$b" | head -n 1)
    if [ -z "$found" ]; then
      rm -rf "$tmp"
      return 1
    fi
    cp -f "$found" "target/release/$b"
    chmod 755 "target/release/$b"
  done
  rm -rf "$tmp"
  return 0
}

if [ "$FROM_RELEASE" -eq 1 ]; then
  if fetch_release; then
    echo "install.sh: using GitHub release artifacts"
  else
    echo "install.sh: no matching release asset; building from source"
  fi
fi

if ! have_bins; then
  if ! command -v cargo >/dev/null 2>&1; then
    echo "install.sh: cargo not found. Install Rust 1.83+ or pass --from-release." >&2
    exit 1
  fi
  if [ "$MINIMAL" -eq 1 ]; then
    cargo build --release -p quotad -p quota
  else
    cargo build --release -p quotad -p quota -p quota-ctl
  fi
fi

copy_bins

echo
echo "Next (agent or human):"
echo "  export PATH=\"${BINDIR}:\$PATH\""
echo "  export QUOTA_SOCKET=\"\${XDG_RUNTIME_DIR:-$HOME/.local/share}/quota/quota.sock\""
echo "  quotad run --socket \"\$QUOTA_SOCKET\" &"
echo "  quota --socket \"\$QUOTA_SOCKET\" status --json"
echo
echo "Do not scrape ~/.codex/auth.json, ~/.claude/.credentials.json, or"
echo "cursor-session.json. The daemon reuses sessions; the CLI only reads the socket."
echo "Details: docs/AGENT.md"
