#!/usr/bin/env bash
# `moi ssh up` on a real machine, as far as it can go without a Cloudflare account: the real package
# repository with the key moi pins, the real `cloudflared service install`, moi's drop-in, and
# cloudflared's own metrics. Cloudflare's API is tests/e2e/fake-cloudflare.py; the tunnel is not a
# real one, so the connector never gets an edge connection — and that is where the run must stop,
# having changed nothing at Cloudflare. Runs as root inside the throwaway VM (tests/e2e/run.sh).
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

# for real: it must get as far as a connector that runs and answers on its metrics address
set +e
moi ssh up --hostname ssh.moi-e2e.test --yes > /root/ssh-up.txt 2>&1
code=$?
set -e
cat /root/ssh-up.txt
test "$code" = 2
# the key in Cloudflare's repository is the one moi pins; apt accepted the repository; the binary runs
cloudflared --version
grep -q 'signed-by=/usr/share/keyrings/cloudflare-main.gpg' /etc/apt/sources.list.d/cloudflared.list
# cloudflared wrote its own unit, with the token, and moi's drop-in sits next to it
grep -q -- "--token $token" /etc/systemd/system/cloudflared.service || test "$(cat /etc/cloudflared/token)" = "$token"
# … and moi recognizes that unit as this tunnel's: the plan no longer wants to install it
moi ssh up --hostname ssh.moi-e2e.test --dry-run | tee /root/ssh-plan2.txt
if grep -q 'cloudflared service install' /root/ssh-plan2.txt; then echo "moi does not know its own connector" >&2; exit 1; fi
grep -q 'Environment=TUNNEL_METRICS=127.0.0.1:20241' /etc/systemd/system/cloudflared.service.d/moi.conf
systemctl cat cloudflared.service | grep -q TUNNEL_METRICS
# the drop-in is honoured: cloudflared answers on that address, in the shape moi reads — and what moi
# said is "no edge connections", not "metrics not answering"
curl -s http://127.0.0.1:20241/ready | tee /root/ready.json
grep -q '"readyConnections":0' /root/ready.json
grep -q 'has no connection to Cloudflare after 60 s (no edge connections)' /root/ssh-up.txt
# nothing that worked before was touched: no rule, no record, moi.toml as it was
curl -fsS http://127.0.0.1:8787/client/v4/state | tee /root/cf-state.json
grep -q '"records": \[\]' /root/cf-state.json
if grep -q '"PUT\|"POST\|"DELETE\|ssh.moi-e2e.test' /root/cf-state.json; then echo "something was changed at Cloudflare" >&2; exit 1; fi
test "$(sha256sum /etc/moi/moi.toml)" = "$config_before"

# and back: what was installed for it goes again
moi ssh down | tee /root/ssh-down.txt
test ! -e /etc/systemd/system/cloudflared.service
test ! -e /etc/systemd/system/cloudflared.service.d/moi.conf
grep -q 'the cloudflared package and Cloudflare.s apt repository stay installed' /root/ssh-down.txt
sed -i '/cloudflare_/d' /etc/moi/secrets.toml
echo "ssh up on a real machine: ok"
