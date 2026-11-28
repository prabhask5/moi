#!/usr/bin/env bash
# Builds the server that the end-to-end test backs up and later rebuilds: a compose stack with a
# Postgres database, a swarm service, a custom unit, a user, firewall rules — and moi looking after it.
# Runs as root inside a throwaway VM (tests/e2e/run.sh); never on a machine anyone cares about.
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive

apt-get update -q
apt-get install -y -q docker.io docker-compose-v2 restic iptables curl

# ---- a compose stack: a database with a row that must survive, and a web server
mkdir -p /opt/services/app/html /opt/scripts /srv/backups /srv/repo
echo "moi e2e" > /opt/services/app/html/index.html
cat > /opt/services/app/docker-compose.yml <<'YAML'
services:
  db:
    image: postgres:16
    container_name: app-db
    restart: unless-stopped
    environment:
      POSTGRES_PASSWORD: fake-e2e-password
    volumes:
      - ./pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres"]
      interval: 5s
      retries: 20
  maria:
    image: mariadb:11
    container_name: app-maria
    restart: unless-stopped
    environment:
      MARIADB_ROOT_PASSWORD: fake-e2e-maria-password
    volumes:
      - ./mysqldata:/var/lib/mysql
  web:
    image: nginx:alpine
    container_name: app-web
    restart: unless-stopped
    ports:
      - "8080:80"
    volumes:
      - ./html:/usr/share/nginx/html
YAML
docker compose --project-directory /opt/services/app up -d
for _ in $(seq 60); do docker exec app-db pg_isready -q -U postgres && break; sleep 2; done
docker exec app-db psql -q -U postgres -c "CREATE TABLE e2e (v text); INSERT INTO e2e VALUES ('survives');"
for _ in $(seq 60); do docker exec app-maria mariadb -uroot -pfake-e2e-maria-password -e 'SELECT 1' >/dev/null 2>&1 && break; sleep 2; done
docker exec app-maria mariadb -uroot -pfake-e2e-maria-password -e "CREATE DATABASE e2e; CREATE TABLE e2e.t (v text); INSERT INTO e2e.t VALUES ('survives too');"

# ---- a swarm service
docker swarm init --advertise-addr 127.0.0.1
docker service create --quiet --name whoami --publish published=8081,target=80 traefik/whoami:v1.10

# ---- a custom unit with its script, and a person
cat > /opt/scripts/hello.sh <<'SH'
#!/bin/sh
while :; do sleep 3600; done
SH
chmod 755 /opt/scripts/hello.sh
cat > /etc/systemd/system/hello.service <<'UNIT'
[Unit]
Description=moi e2e custom unit

[Service]
ExecStart=/opt/scripts/hello.sh

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now hello.service
useradd -m -u 1500 -s /bin/bash alice

# ---- moi: a configuration written by hand first; `moi init` then installs the service for it
mkdir -p /etc/moi/firewall
cat > /etc/moi/firewall/rules4.txt <<'RULES'
-A INPUT -i lo -j ACCEPT
-A INPUT -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
-A INPUT -p icmp -j ACCEPT
-A INPUT -p udp --dport 68 -j ACCEPT
-A INPUT -p tcp --dport 22 -j ACCEPT
-A INPUT -p tcp --dport 8080 -j ACCEPT
-A INPUT -p tcp --dport 8081 -j ACCEPT
-A INPUT -m comment --comment "moi e2e" -j DROP
-A DOCKER-USER -j RETURN
RULES
cat > /etc/moi/firewall/rules6.txt <<'RULES'
-A INPUT -i lo -j ACCEPT
RULES
cat > /etc/moi/moi.toml <<'TOML'
[server]
name = "e2e"

[capture]
data_roots = ["/opt"]

[boot]
compose_up = ["/opt/services/**"]
verify_after = "1m"

[firewall]
manage = true
chains = ["INPUT", "DOCKER-USER"]
rules_v4 = "/etc/moi/firewall/rules4.txt"
rules_v6 = "/etc/moi/firewall/rules6.txt"

[backup]
local_dir = "/srv/backups"
paths = ["/opt/services", "/opt/scripts", "/etc/moi"]

[backup.offsite]
repository = "/srv/repo"
password = "secret:restic_password"
keep = { daily = 7, weekly = 4, monthly = 6 }
TOML
install -m 600 /dev/null /etc/moi/secrets.toml
printf '[secrets]\nrestic_password = "fake-e2e-repository-password"\n' > /etc/moi/secrets.toml
RESTIC_PASSWORD=fake-e2e-repository-password restic -r /srv/repo init

moi config check > /dev/null
moi init --non-interactive | tee /root/init.txt
grep -q "moi.service installed and started" /root/init.txt
grep -q 'name = "e2e"' /etc/moi/moi.toml   # the configuration that was there is untouched
systemctl is-active moi.service
systemctl is-enabled moi.service
moi firewall apply
moi snapshot
moi backup
moi push
# the raw files of both databases stay out of the repository: their dumps are what is kept
RESTIC_PASSWORD=fake-e2e-repository-password restic -r /srv/repo ls latest > /root/repo-files.txt
grep -c 'pgdata/\|mysqldata/' /root/repo-files.txt || true
if grep -q '/pgdata/.\|/mysqldata/.' /root/repo-files.txt; then
  grep '/pgdata/.\|/mysqldata/.' /root/repo-files.txt | head -20
  echo "raw database files were backed up" >&2; exit 1
fi
# the server as it is must pass its own verification, or the comparison later means nothing
sleep 20
moi verify
