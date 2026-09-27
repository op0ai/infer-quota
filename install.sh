#!/usr/bin/env sh
# install.sh — put quotad / quota / quota-ctl on PATH.
#
# Smallest honest path:
#   ./install.sh                 # cargo build --release, copy into ~/.local/bin
#   ./install.sh --minimal       # quotad + quota only; removes leftover quota-ctl
#   ./install.sh --prefix DIR    # install into DIR/bin
#   ./install.sh --from-release  # fetch a GitHub release if one exists; else build
#
# Socket resolution (same as the binaries):
#   --socket  →  config.json "socket"  →  QUOTA_SOCKET  →
#   $XDG_RUNTIME_DIR/quota/quota.sock  →  ~/.local/share/quota/quota.sock
# See docs/AGENT.md.

set -eu

REPO="op0ai/infer-quota"
PREFIX=""
MINIMAL=0
FROM_RELEASE=0
BINS="quotad quota quota-ctl"

usage() {
  sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-2}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)
      PREFIX="${2:-}"
      [ -n "$PREFIX" ] || usage 2
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
      usage 0
      ;;
    *)
      echo "install.sh: unknown arg $1" >&2
      usage 2
      ;;
  esac
done

if [ -z "$PREFIX" ]; then
  if [ -z "${HOME:-}" ]; then
    echo "install.sh: HOME is unset. Pass --prefix DIR." >&2
    exit 1
  fi
  PREFIX="${HOME}/.local"
fi

BINDIR="${PREFIX}/bin"
mkdir -p "${BINDIR}"

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$root"

if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  RELEASE_DIR="${CARGO_TARGET_DIR}/release"
else
  RELEASE_DIR="${root}/target/release"
fi

copy_bins() {
  for b in $BINS; do
    src="${RELEASE_DIR}/$b"
    if [ ! -x "$src" ]; then
      echo "install.sh: missing $src" >&2
      exit 1
    fi
    tmp="${BINDIR}/.${b}.new.$$"
    cp -f "$src" "$tmp"
    chmod 755 "$tmp"
    mv -f "$tmp" "${BINDIR}/$b"
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
  mkdir -p "${RELEASE_DIR}"
  for b in $BINS; do
    found=$(find "$tmp" -type f -name "$b" | head -n 1)
    if [ -z "$found" ]; then
      rm -rf "$tmp"
      return 1
    fi
    cp -f "$found" "${RELEASE_DIR}/$b"
    chmod 755 "${RELEASE_DIR}/$b"
  done
  rm -rf "$tmp"
  return 0
}

from_release=0
if [ "$FROM_RELEASE" -eq 1 ]; then
  if fetch_release; then
    echo "install.sh: using GitHub release artifacts"
    from_release=1
  else
    echo "install.sh: no matching release asset; building from source"
  fi
fi

if [ "$from_release" -eq 0 ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    echo "install.sh: cargo not found. Install Rust 1.83+ or pass --from-release." >&2
    exit 1
  fi
  # Always rebuild the current tree. Stale target/release bins must not win.
  if [ "$MINIMAL" -eq 1 ]; then
    cargo build --release -p quotad -p quota
  else
    cargo build --release -p quotad -p quota -p quota-ctl
  fi
fi

copy_bins

if [ "$MINIMAL" -eq 1 ] && [ -e "${BINDIR}/quota-ctl" ]; then
  rm -f "${BINDIR}/quota-ctl"
  echo "install.sh: removed leftover ${BINDIR}/quota-ctl (--minimal)"
fi

echo
echo "Next (agent or human):"
echo "  export PATH=\"${BINDIR}:\$PATH\""
if [ -n "${HOME:-}" ]; then
  echo "  export QUOTA_SOCKET=\"\${XDG_RUNTIME_DIR:-$HOME/.local/share}/quota/quota.sock\""
else
  echo "  export QUOTA_SOCKET=\"\${XDG_RUNTIME_DIR:-${PREFIX}/share}/quota/quota.sock\""
fi
echo "  quotad run --socket \"\$QUOTA_SOCKET\" &"
echo "  quota --socket \"\$QUOTA_SOCKET\" status --json"
echo
echo "Do not scrape ~/.codex/auth.json, ~/.claude/.credentials.json, or"
echo "cursor-session.json. The daemon reuses sessions; the CLI only reads the socket."
echo "Details: docs/AGENT.md"
