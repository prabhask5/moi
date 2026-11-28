#!/usr/bin/env bash
# On the old server, after its backup is safe: the things that only a real systemd, a real cron and a
# real kernel can show — the firewall's rollback timer firing by itself, and a migration from a legacy
# unit and cron job to moi and back. Runs as root inside the throwaway VM (tests/e2e/run.sh).
set -euxo pipefail

# ------------------------------------------------------------------ the firewall's rollback timer
# A rule is applied "over SSH" and nobody confirms — worse, the session dies with moi in it. The
# timer armed beforehand must put the old rules back on its own.
cp /etc/moi/firewall/rules4.txt /root/rules4.before
sed -i '1i -A INPUT -p tcp --dport 9999 -m comment --comment "moi e2e timer" -j ACCEPT' /etc/moi/firewall/rules4.txt
iptables -S INPUT > /root/input.before
(sleep 900 | SSH_CONNECTION="192.0.2.1 50000 192.0.2.2 22" moi firewall apply > /root/apply.txt 2>&1) &
for _ in $(seq 30); do iptables -S INPUT | grep -q 'moi e2e timer' && break; sleep 1; done
iptables -S INPUT | grep -q 'moi e2e timer'            # applied, and waiting for the confirmation
systemctl is-active moi-firewall-rollback.timer       # the way back is armed, outside the session
pkill -9 -f 'moi firewall apply'                      # the session is cut off: moi dies unconfirmed
pkill -f 'sleep 900' || true
for _ in $(seq 60); do iptables -S INPUT | grep -q 'moi e2e timer' || break; sleep 2; done
if iptables -S INPUT | grep -q 'moi e2e timer'; then echo "the rollback timer did not put the rules back" >&2; exit 1; fi
diff <(iptables -S INPUT) /root/input.before          # exactly the rules from before
# back in step: the file as it was, applied at the console
cp /root/rules4.before /etc/moi/firewall/rules4.txt
moi firewall apply
moi firewall status

# ------------------------------------------------------------------ a migration, on real systemd and cron
# The legacy setup: a unit that starts the compose stacks at boot (what `boot.compose_up` does now),
# and a cron job with its script that the owner names in `migrate.remove`.
mkdir -p /opt/scripts
printf '#!/bin/sh\ndate > /var/tmp/legacy-ran\n' > /opt/scripts/legacy-job.sh
chmod 755 /opt/scripts/legacy-job.sh
echo '* * * * * root /opt/scripts/legacy-job.sh' > /etc/cron.d/legacy-job
cat > /etc/systemd/system/legacy-stacks.service <<'UNIT'
[Unit]
Description=legacy: start the stacks
After=docker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/bin/docker compose --project-directory /opt/services/app up -d

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now legacy-stacks.service
cat >> /etc/moi/moi.toml <<'TOML'

[migrate]
remove = ["/etc/cron.d/legacy-job", "/opt/scripts/legacy-job.sh"]
TOML
moi config check > /dev/null
moi snapshot

moi migrate plan | tee /root/plan.txt
grep -q '^boot  →  boot.compose_up' /root/plan.txt
grep -q 'unit   /etc/systemd/system/legacy-stacks.service' /root/plan.txt
grep -q '^stale  →' /root/plan.txt
grep -q '/etc/cron.d/legacy-job' /root/plan.txt

moi migrate shadow | tee /root/shadow.txt
cp /etc/cron.d/legacy-job /root/cron.before
cp /etc/systemd/system/legacy-stacks.service /root/unit.before

moi migrate switch --force | tee /root/switch.txt
test "$(systemctl is-enabled legacy-stacks.service)" = disabled
test "$(systemctl is-active legacy-stacks.service || true)" = inactive
cmp /root/unit.before /var/lib/moi/archive/legacy/etc/systemd/system/legacy-stacks.service
cmp /root/cron.before /var/lib/moi/archive/legacy/etc/cron.d/legacy-job
docker ps --format '{{.Names}}' | grep -q app-db           # stopping the unit stopped no container

moi migrate rollback | tee /root/rollback.txt
test "$(systemctl is-enabled legacy-stacks.service)" = enabled
test "$(systemctl is-active legacy-stacks.service)" = active
cmp /root/cron.before /etc/cron.d/legacy-job               # byte for byte, mode and owner too
test "$(stat -c '%a %U' /etc/cron.d/legacy-job)" = "644 root"
cmp /root/unit.before /etc/systemd/system/legacy-stacks.service

moi migrate switch --force
moi backup
moi migrate finish --now | tee /root/finish.txt
grep -q 'legacy files, units and directories removed' /root/finish.txt
test ! -e /etc/systemd/system/legacy-stacks.service
test ! -e /etc/cron.d/legacy-job
test ! -e /opt/scripts/legacy-job.sh
test -e /var/lib/moi/archive/legacy/opt/scripts/legacy-job.sh
if systemctl cat legacy-stacks.service >/dev/null 2>&1; then echo "systemd still knows the unit" >&2; exit 1; fi
moi migrate plan | tee /root/plan-after.txt
grep -q '^nothing to migrate' /root/plan-after.txt
docker ps --format '{{.Names}}' | grep -q app-db
systemctl is-active moi.service
echo "old-server extras: ok"
