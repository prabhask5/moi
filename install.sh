#!/bin/sh
# moi installer: curl -fsSL https://raw.githubusercontent.com/prabhask5/moi/main/install.sh | sh
#   options (after `sh -s --`): --version X.Y.Z   --prefix /usr/local
# Downloads the static binary for this CPU from GitHub Releases, verifies its SHA-256, installs it.
set -eu
REPO=prabhask5/moi
VERSION=latest
PREFIX=/usr/local
while [ $# -gt 0 ]; do
  case $1 in
    --version) VERSION=$2; shift ;;
    --prefix) PREFIX=$2; shift ;;
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
BASE="https://github.com/$REPO/releases/download/v$VERSION"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
echo "downloading moi $VERSION ($TARGET)"
curl -fsSL "$BASE/$NAME.tar.gz" -o "$TMP/$NAME.tar.gz"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS"
( cd "$TMP" && grep " $NAME.tar.gz\$" SHA256SUMS | sha256sum -c - >/dev/null ) || { echo "checksum mismatch — aborting" >&2; exit 1; }
tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"
SUDO=""; [ "$(id -u)" -eq 0 ] || SUDO=sudo
$SUDO install -m 755 "$TMP/$NAME/moi" "$PREFIX/bin/moi"
echo "installed $PREFIX/bin/moi ($("$PREFIX/bin/moi" --version))"
command -v restic >/dev/null 2>&1 || echo "note: restic is not installed; off-site backups need it (apt install restic / dnf install restic)"
echo "next: sudo moi init"
