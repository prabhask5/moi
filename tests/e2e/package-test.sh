#!/usr/bin/env bash
# Installs the built packages in clean containers of the distributions they are for, and looks at
# what arrived. Needs Docker; run by the release workflow.
#
#   tests/e2e/package-test.sh dist/     (a directory with one amd64 .deb and one x86_64 .rpm)
set -euo pipefail

dist=$(realpath "$1")
deb=$(basename "$(ls "$dist"/*amd64.deb)")
rpm=$(basename "$(ls "$dist"/*x86_64.rpm)")

# inside the container: the files are where the distribution expects them, the unit is not enabled
looks_right='
set -e
moi --version
test -x /usr/bin/moi
test -f /usr/lib/systemd/system/moi.service
grep -q "ExecStart=/usr/bin/moi daemon" /usr/lib/systemd/system/moi.service
test ! -e /etc/systemd/system/multi-user.target.wants/moi.service
test -f /usr/share/bash-completion/completions/moi
test -f /usr/share/fish/vendor_completions.d/moi.fish
ls /usr/share/man/man1/moi.1* >/dev/null
moi restore --list | grep -q "^preflight"
moi completions bash | grep -q "_moi()"
'

for image in ubuntu:24.04 ubuntu:26.04 debian:13; do
  echo "=== $deb on $image"
  docker run --rm -v "$dist:/dist:ro" "$image" sh -c "
    set -e
    dpkg -i /dist/$deb
    $looks_right
    dpkg -r moi
    test ! -e /usr/bin/moi
  "
done
for image in fedora:latest; do
  echo "=== $rpm on $image"
  docker run --rm -v "$dist:/dist:ro" "$image" sh -c "
    set -e
    rpm -i --nodeps /dist/$rpm
    $looks_right
    rpm -e moi
    test ! -e /usr/bin/moi
  "
done
echo "packages: ok"
