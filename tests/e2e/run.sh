#!/usr/bin/env bash
# End-to-end test of backup → restore → reboot → verify, on two real virtual machines (Incus + KVM).
#
#   tests/e2e/run.sh path/to/static/moi
#
# Made for a CI runner (the nightly workflow): it installs Incus and creates and deletes VMs named
# moi-e2e-old and moi-e2e-new. Nothing of the machine it runs on is given to moi.
set -euxo pipefail

moi=$(realpath "$1")
here=$(cd "$(dirname "$0")" && pwd)
image=${MOI_E2E_IMAGE:-images:ubuntu/24.04/cloud}
work=$(mktemp -d)

launch() {
    sudo incus launch "$image" "$1" --vm -c limits.cpu=2 -c limits.memory=3GiB -d root,size=12GiB
    # the agent answers once the machine is up
    for _ in $(seq 120); do sudo incus exec "$1" -- true 2>/dev/null && break; sleep 5; done
    sudo incus exec "$1" -- cloud-init status --wait || true
    sudo incus file push "$moi" "$1/usr/local/bin/moi" --mode 0755
}

run_in() {
    local vm=$1 script=$2
    sudo incus file push "$here/$script" "$vm/root/$script" --mode 0755
    sudo incus exec "$vm" -- "/root/$script"
}

if ! command -v incus >/dev/null; then
    sudo apt-get update -q
    sudo apt-get install -y -q incus qemu-system-x86
fi
sudo incus admin init --auto
# Docker on the runner drops forwarded packets that are not its own: let the VMs' bridge through
if sudo iptables -S DOCKER-USER >/dev/null 2>&1; then
    sudo iptables -I DOCKER-USER -i incusbr0 -j ACCEPT
    sudo iptables -I DOCKER-USER -o incusbr0 -j ACCEPT
fi

# ---- the old server: built, backed up, switched off
launch moi-e2e-old
run_in moi-e2e-old old-server.sh
sudo incus exec moi-e2e-old -- tar -C /srv -cf - repo > "$work/repo.tar"
sudo incus delete --force moi-e2e-old

# ---- the new one: only the repository and the binary
launch moi-e2e-new
sudo incus exec moi-e2e-new -- mkdir -p /srv
sudo incus exec moi-e2e-new -- tar -C /srv -xf - < "$work/repo.tar"
run_in moi-e2e-new new-server.sh

# ---- the reboot that proves it
sudo incus restart moi-e2e-new
for _ in $(seq 120); do sudo incus exec moi-e2e-new -- true 2>/dev/null && break; sleep 5; done
sudo incus exec moi-e2e-new -- systemctl is-system-running --wait || true
# containers start, health checks settle, the daemon's own verification runs (boot.verify_after = 1m)
sleep 120
run_in moi-e2e-new verify-rebuilt.sh

sudo incus delete --force moi-e2e-new
rm -rf "$work"
echo "end-to-end: rebuilt, rebooted, verified"
