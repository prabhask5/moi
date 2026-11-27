#!/usr/bin/env bash
# Rebuilds the server on a machine that has nothing but an operating system, the moi binary and the
# backup repository. Runs as root inside a throwaway VM (tests/e2e/run.sh).
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive

apt-get update -q
apt-get install -y -q restic

export RESTIC_PASSWORD=fake-e2e-repository-password
bundle=$(moi pull --repository /srv/repo | sed -n 's/^next: moi restore --bundle \(.*\) --dry-run$/\1/p')
test -d "$bundle"
unset RESTIC_PASSWORD

# the dry run first: it must change nothing
moi restore --bundle "$bundle" --dry-run
test ! -e /opt/services
test ! -e /etc/systemd/system/hello.service
if command -v docker; then exit 1; fi

# the real one. `--yes` takes the phase questions; the firewall takes a typed yes of its own
echo yes | moi restore --bundle "$bundle" --yes
tail -5 /var/lib/moi/restore/restore.log
