#!/usr/bin/env bash
# After the reboot of the rebuilt machine: everything must have come up on its own.
# Runs as root inside the VM (tests/e2e/run.sh).
set -euxo pipefail

moi verify

# the row came back through the dump
test "$(docker exec app-db psql -At -U postgres -c 'SELECT v FROM e2e')" = "survives"
# the stack, the swarm service, the unit, the person, the firewall
test "$(curl -fsS http://127.0.0.1:8080/)" = "moi e2e"
curl -fsS http://127.0.0.1:8081/ | grep -q Hostname
systemctl is-active hello.service
test "$(id -u alice)" = 1500
iptables -S INPUT | grep -q 'moi e2e'
systemctl is-active moi.service
# the daemon verified this boot by itself and told the login message
for _ in $(seq 30); do test -s /run/motd.d/50-moi && break; sleep 10; done
cat /run/motd.d/50-moi
