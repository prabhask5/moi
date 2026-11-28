#!/usr/bin/env bash
# `moi ssh up` on a real machine, as far as it can go without a Cloudflare account: the real package
# repository with the key moi pins, the real `cloudflared service install`, moi's drop-in, and
# cloudflared's own metrics. Cloudflare's API is tests/e2e/fake-cloudflare.py; the tunnel is not a
# real one, so the connector's service cannot start — and that is where the run must stop, having
# changed nothing at Cloudflare. Runs as root inside the throwaway VM (tests/e2e/run.sh).
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive

account=FAKEaccount0000000000000000000001
tunnel=00000000-0000-4000-8000-00000000e2e0
token=$(printf '{"a":"%s","t":"%s","s":"%s"}' "$account" "$tunnel" "$(head -c 32 /dev/zero | base64 -w0)" | base64 -w0)
python3 /root/fake-cloudflare.py 8787 "$account" "$tunnel" moi-e2e.test > /root/fake-cloudflare.log 2>&1 &
for _ in $(seq 20); do curl -fsS http://127.0.0.1:8787/client/v4/state >/dev/null && break; sleep 1; done

printf 'cloudflare_api_token = "FAKEe2eapitoken"\ncloudflare_tunnel_token = "%s"\n' "$token" >> /etc/moi/secrets.toml
# the daemon and the commands talk to the fake
export MOI_CF_API_URL=http://127.0.0.1:8787/client/v4
config_before=$(sha256sum /etc/moi/moi.toml)

moi ssh up --hostname ssh.moi-e2e.test --dry-run | tee /root/ssh-plan.txt
grep -q 'change  connector    install cloudflared from Cloudflare' /root/ssh-plan.txt
grep -q 'change  ingress      add ssh.moi-e2e.test → ssh://localhost:22' /root/ssh-plan.txt
grep -q 'change  dns          create a proxied CNAME' /root/ssh-plan.txt
if command -v cloudflared; then echo "cloudflared is installed before the run" >&2; exit 1; fi

# for real: the package comes from Cloudflare's repository, with the key moi pins, and cloudflared
# installs its own service. This tunnel does not exist, so that service cannot start (cloudflared
# reports ready to systemd only once it has registered with the edge) — and there the run must stop.
set +e
moi ssh up --hostname ssh.moi-e2e.test --yes > /root/ssh-up.txt 2>&1
code=$?
set -e
cat /root/ssh-up.txt
test "$code" = 2
grep -q '`cloudflared service install \*\*\*` failed' /root/ssh-up.txt     # and the token is not shown
if grep -q "$token" /root/ssh-up.txt; then echo "the tunnel token was printed" >&2; exit 1; fi
grep -q '`moi ssh down` puts back what was changed so far' /root/ssh-up.txt
# the key in Cloudflare's repository is the one moi pins; apt accepted the repository; the binary runs
cloudflared --version
grep -q 'signed-by=/usr/share/keyrings/cloudflare-main.gpg' /etc/apt/sources.list.d/cloudflared.list
ls -la /etc/systemd/system/ | grep -i cloudflared || true
ls -la /etc/cloudflared 2>/dev/null || true

# What moi's drop-in relies on, shown with the real binary: TUNNEL_METRICS sets the metrics address,
# and /ready answers there in the shape moi reads, 503 with no edge connection.
TUNNEL_METRICS=127.0.0.1:20241 cloudflared --no-autoupdate tunnel run --token "$token" > /root/cloudflared.log 2>&1 &
ready=""
for _ in $(seq 150); do
  if curl -s -o /root/ready.json -w '%{http_code}' http://127.0.0.1:20241/ready > /root/ready.code 2>/dev/null && [ -s /root/ready.json ]; then ready=1; break; fi
  sleep 0.2
done
pkill -f 'cloudflared --no-autoupdate tunnel run' || true
if [ -z "$ready" ]; then tail -20 /root/cloudflared.log; echo "cloudflared did not answer on TUNNEL_METRICS" >&2; exit 1; fi
cat /root/ready.json; cat /root/ready.code
grep -q '"readyConnections":0' /root/ready.json
grep -q '"status":503' /root/ready.json
test "$(cat /root/ready.code)" = 503
# nothing that worked before was touched: no rule, no record, moi.toml as it was
curl -fsS http://127.0.0.1:8787/client/v4/state | tee /root/cf-state.json
grep -q '"records": \[\]' /root/cf-state.json
if grep -q '"PUT\|"POST\|"DELETE\|ssh.moi-e2e.test' /root/cf-state.json; then echo "something was changed at Cloudflare" >&2; exit 1; fi
test "$(sha256sum /etc/moi/moi.toml)" = "$config_before"

# and back: what was installed for it goes again
moi ssh down 2>&1 | tee /root/ssh-down.txt || true
test ! -e /etc/systemd/system/cloudflared.service
test ! -e /etc/systemd/system/cloudflared.service.d/moi.conf
if systemctl is-active --quiet cloudflared.service; then echo "a connector is still running" >&2; exit 1; fi
sed -i '/cloudflare_/d' /etc/moi/secrets.toml
echo "ssh up on a real machine: ok"
