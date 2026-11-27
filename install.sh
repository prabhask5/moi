#!/bin/sh
# moi installer: curl -fsSL https://raw.githubusercontent.com/prabhask5/moi/main/install.sh | sh
#   options (after `sh -s --`): --version X.Y.Z   --prefix /usr/local   --yes (install restic without asking)
# Downloads the static binary for this CPU from GitHub Releases, verifies its SHA-256, installs it.
# Nothing is installed unless the checksum of the download is the one the release publishes.
set -eu
REPO=prabhask5/moi
VERSION=latest
PREFIX=/usr/local
YES=0
while [ $# -gt 0 ]; do
  case $1 in
    --version) VERSION=${2#v}; shift ;;
    --prefix) PREFIX=$2; shift ;;
    --yes|-y) YES=1 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
case $(uname -m) in
  x86_64|amd64) TARGET=x86_64-unknown-linux-musl ;;
  aarch64|arm64) TARGET=aarch64-unknown-linux-musl ;;
  *) echo "unsupported CPU: $(uname -m)" >&2; exit 1 ;;
esac
[ "$(uname -s)" = Linux ] || { echo "moi runs on Linux" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "missing $1" >&2; exit 1; }; }
need curl; need tar; need sha256sum
if [ "$VERSION" = latest ]; then
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1)
  [ -n "$VERSION" ] || { echo "could not determine the latest release" >&2; exit 1; }
fi
NAME="moi-$VERSION-$TARGET"
# MOI_RELEASE_URL: where the release files are, for mirrors and for this script's own test
BASE=${MOI_RELEASE_URL:-"https://github.com/$REPO/releases/download/v$VERSION"}
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
echo "downloading moi $VERSION ($TARGET)"
curl -fsSL "$BASE/$NAME.tar.gz" -o "$TMP/$NAME.tar.gz"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS"
WANT=$(awk -v f="$NAME.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$TMP/SHA256SUMS" | head -1)
[ -n "$WANT" ] || { echo "SHA256SUMS of this release does not list $NAME.tar.gz — aborting" >&2; exit 1; }
HAVE=$(sha256sum "$TMP/$NAME.tar.gz" | awk '{ print $1 }')
[ "$WANT" = "$HAVE" ] || { echo "checksum mismatch for $NAME.tar.gz — aborting, nothing was installed" >&2; exit 1; }
tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"
SUDO=""; [ "$(id -u)" -eq 0 ] || SUDO=sudo
$SUDO install -d "$PREFIX/bin"
$SUDO install -m 755 "$TMP/$NAME/moi" "$PREFIX/bin/moi"
echo "installed $PREFIX/bin/moi ($("$PREFIX/bin/moi" --version))"
# off-site backups need restic: offered, never installed unasked
if ! command -v restic >/dev/null 2>&1; then
  if command -v apt-get >/dev/null 2>&1; then GET="apt-get install -y restic"
  elif command -v dnf >/dev/null 2>&1; then GET="dnf install -y restic"
  elif command -v pacman >/dev/null 2>&1; then GET="pacman -S --noconfirm restic"
  else GET=""; fi
  ANSWER=n
  if [ -n "$GET" ] && [ "$YES" = 1 ]; then ANSWER=y
  elif [ -n "$GET" ] && [ -r /dev/tty ] && [ -t 1 ]; then
    printf 'restic is not installed; off-site backups need it. Install it now (%s)? [y/N] ' "$GET"
    read -r ANSWER </dev/tty || ANSWER=n
  fi
  case $ANSWER in
    y|Y|yes) $SUDO $GET ;;
    *) echo "note: restic is not installed; off-site backups need it${GET:+ ($GET)}" ;;
  esac
fi
echo "next: sudo moi init"
