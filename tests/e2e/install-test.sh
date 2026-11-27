#!/usr/bin/env bash
# The installer against a release made on the spot: it installs what the checksums vouch for and
# nothing else. Linux only (the installer refuses other systems).
#
#   tests/e2e/install-test.sh path/to/moi
set -euo pipefail

moi=$(realpath "$1")
here=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case $(uname -m) in
  x86_64|amd64) target=x86_64-unknown-linux-musl ;;
  aarch64|arm64) target=aarch64-unknown-linux-musl ;;
esac
name=moi-9.9.9-$target
mkdir -p "$work/release/$name" "$work/good" "$work/bad" "$work/unlisted"
cp "$moi" "$work/release/$name/moi"
tar -C "$work/release" -czf "$work/release/$name.tar.gz" "$name"
rm -rf "${work:?}/release/$name"
(cd "$work/release" && sha256sum "$name.tar.gz" > SHA256SUMS)
export MOI_RELEASE_URL="file://$work/release"

echo "--- a release whose checksum matches is installed"
out=$(sh "$here/install.sh" --version 9.9.9 --prefix "$work/good" 2>&1)
echo "$out"
"$work/good/bin/moi" --version
grep -q "next: sudo moi init" <<<"$out"

echo "--- a tarball that is not the one the checksums name is refused"
echo tampered >> "$work/release/$name.tar.gz"
if out=$(sh "$here/install.sh" --version 9.9.9 --prefix "$work/bad" 2>&1); then
  echo "the installer accepted a tampered download: $out" >&2; exit 1
fi
grep -q "checksum mismatch" <<<"$out"
test ! -e "$work/bad/bin/moi"

echo "--- a release whose checksums do not list the file is refused"
echo "0000  something-else.tar.gz" > "$work/release/SHA256SUMS"
if out=$(sh "$here/install.sh" --version 9.9.9 --prefix "$work/unlisted" 2>&1); then
  echo "the installer accepted an unlisted download: $out" >&2; exit 1
fi
grep -q "does not list" <<<"$out"
test ! -e "$work/unlisted/bin/moi"
echo "installer: ok"
