#!/usr/bin/env bash
# `moi ssh connect` end to end, with nothing faked on the client side: the command moi prints is run
# by another user of this machine (a clean home), it downloads devbox's released client and setup
# script, and the real OpenSSH client then logs in through a WebSocket origin to a real sshd.
# What stands in for Cloudflare is tests/e2e/ws-bridge.py: TLS, a WebSocket, bytes to sshd.
#
#   tests/e2e/ssh-client-test.sh path/to/moi        (a CI runner: it uses sudo and creates a user)
set -euxo pipefail

moi=$(realpath "$1")
here=$(cd "$(dirname "$0")" && pwd)
name=ssh.moi-e2e.test
work=$(mktemp -d)
me=$(id -un)

sudo apt-get update -q
sudo apt-get install -y -q openssh-server openssh-client python3-pip curl
pip install --quiet --break-system-packages websockets || pip install --quiet websockets

# ---- the server: sshd on the loopback, with the system's host keys
sudo ssh-keygen -A
sudo mkdir -p /run/sshd
sudo /usr/sbin/sshd -p 2222 -o ListenAddress=127.0.0.1 -o PasswordAuthentication=no -o PidFile="$work/sshd.pid"

# ---- the way in: a name with a certificate this machine trusts, answering WebSocket on 443
# (a certificate authority and a leaf it signs: a self-signed leaf is refused by rustls, rightly)
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 2 \
  -keyout "$work/ca-key.pem" -out "$work/ca.pem" -subj "/CN=moi e2e test authority" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$work/key.pem" -out "$work/leaf.csr" -subj "/CN=$name"
printf 'subjectAltName=DNS:%s\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n' "$name" > "$work/leaf.ext"
openssl x509 -req -in "$work/leaf.csr" -CA "$work/ca.pem" -CAkey "$work/ca-key.pem" -CAcreateserial \
  -days 2 -extfile "$work/leaf.ext" -out "$work/cert.pem"
sudo cp "$work/ca.pem" /usr/local/share/ca-certificates/moi-e2e.crt
sudo update-ca-certificates
echo "127.0.0.1 $name" | sudo tee -a /etc/hosts
sudo sysctl -q net.ipv4.ip_unprivileged_port_start=443
python3 "$here/ws-bridge.py" "$work/cert.pem" "$work/key.pem" 443 2222 > "$work/bridge.log" 2>&1 &
up=""
for _ in $(seq 30); do
  # any HTTP answer will do: a plain GET is not a WebSocket handshake
  if [ "$(curl -s -o /dev/null -w '%{http_code}' "https://$name/")" != 000 ]; then up=1; break; fi
  sleep 1
done
if [ -z "$up" ]; then cat "$work/bridge.log"; echo "the WebSocket origin did not come up" >&2; exit 1; fi

# ---- moi on the server
sudo install -m 755 "$moi" /usr/local/bin/moi
sudo mkdir -p /etc/moi
printf '[server]\nname = "e2e"\n\n[ssh]\nprovider = "none"\nhostname = "%s"\nuser = "%s"\n' "$name" "$me" | sudo tee /etc/moi/moi.toml
sudo moi config check > /dev/null

# moi's own look from outside, through a real TLS handshake and a WebSocket server it has never met
MOI_IT=1 MOI_IT_SSH_HOST=$name cargo test --test ssh_it -- --ignored --nocapture

# ---- a computer: another user with a home of its own and a key, as the script would make one
sudo useradd -m -s /bin/bash laptop
sudo -u laptop -H ssh-keygen -q -t ed25519 -N '' -f /home/laptop/.ssh/id_ed25519

sudo ls -la /home/laptop/.ssh
sudo moi ssh connect | tee "$work/connect.txt"
# the command is everything up to the first empty line
sed '/^$/q' "$work/connect.txt" > "$work/command.sh"
grep -q "releases/download/ssh-v0.1.0/devbox-ssh-setup.sh" "$work/command.sh"
grep -q -- "--alias e2e --url https://$name --user $me" "$work/command.sh"
chmod 644 "$work/command.sh"; chmod 755 "$work"

# the key is not allowed yet: the script says so, shows moi's hint and the key, and exits 1
if sudo -u laptop -H sh "$work/command.sh" > "$work/first.txt" 2>&1; then
  cat "$work/first.txt"; echo "the setup succeeded before the key was authorized" >&2; exit 1
fi
cat "$work/first.txt"
sudo ls -la /home/laptop/.ssh
grep -q "not authorized on $name yet" "$work/first.txt"
grep -q "On the server, run: sudo moi ssh authorize" "$work/first.txt"
grep -q "$(sudo cut -d' ' -f2 /home/laptop/.ssh/id_ed25519.pub)" "$work/first.txt"

# on the server: allow it, with the line the script showed
sudo SUDO_USER="$me" moi ssh authorize "$(sudo cat /home/laptop/.ssh/id_ed25519.pub)" --name e2e-laptop
grep -q "e2e-laptop" "$HOME/.ssh/authorized_keys"
test "$(stat -c '%a %U' "$HOME/.ssh/authorized_keys")" = "600 $me"

sudo ls -la /home/laptop/.ssh
# again: everything is in place, and the test connection of the script goes through
sudo -u laptop -H sh "$work/command.sh" | tee "$work/second.txt"
grep -q "Done. Connect with: ssh e2e" "$work/second.txt"

# the real client, through devbox-ssh, the WebSocket and sshd — with the host key moi printed pinned
test "$(sudo -u laptop -H ssh -o BatchMode=yes e2e 'echo through-the-tunnel; id -un')" = "through-the-tunnel
$me"
sudo -u laptop -H grep -q "StrictHostKeyChecking yes" /home/laptop/.ssh/config.d/e2e
# a file both ways
head -c 3000000 /dev/urandom > "$work/blob"
sudo -u laptop -H scp -q -o BatchMode=yes "e2e:$work/blob" /home/laptop/blob
sudo cmp "$work/blob" /home/laptop/blob

# moi's view of it
sudo SUDO_USER="$me" moi ssh status | tee "$work/status.txt"
grep -q "keys       1 allowed to log in" "$work/status.txt"

# and gone again, by the same script
sudo -u laptop -H sh -c "curl -fsSL https://github.com/prabhask5/devbox/releases/download/ssh-v0.1.0/devbox-ssh-setup.sh | sh -s -- remove --alias e2e"
if sudo -u laptop -H ssh -o BatchMode=yes -o ConnectTimeout=5 e2e true 2>/dev/null; then echo "the alias still works" >&2; exit 1; fi

sudo kill "$(sudo cat "$work/sshd.pid")" || true
echo "ssh client end to end: ok"
