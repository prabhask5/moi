# cestmoi — server profile (non-secret, from the first serverstate snapshot, 2026-11-26)

**Hardware/OS**: Lenovo ThinkCentre 10RRS76200 (i5-9500T, 6 cores, 15 GB RAM, 4 GB swap), Ubuntu 26.04.1 LTS, kernel 7.0. NVMe 256 GB (LVM root, ext4, 31 % used) + USB Toshiba 4 TB HDD (`/mnt/storage`, ext4, label `storage`, 19 % used, fstab `nofail`).

**Docker** 29.1.3 (distro `docker.io`, `docker-compose-v2`), data root `/var/lib/docker`, drop-ins `docker.service.d/{10-boot-order,storage,wait-for-storage}.conf`.
- 36 containers in 13 compose stacks: `/opt/services/{adguard,cloudflared,ddns,dozzle,homepage,host-logs,outline,seerr,database(supabase),monitoring/{glances,scrutiny,uptime-kuma}}`, `/opt/yams` (jellyfin, bookorbit + bookorbit-db, qbittorrent + sabnzbd sharing gluetun's network, sonarr, radarr, bazarr, prowlarr, gluetun, portainer, watchtower).
- Docker Swarm (single node) for **Dokploy** v0.31: services `dokploy`, `dokploy-postgres`, and side projects (Stellar planner with volume `stellar-data`, Shortless + its Redis volume, compiler UI, dokploy-bridge). Standalone `dokploy-traefik` (ports 80/443) is the edge proxy for everything.
- Networks: `homelab` (shared bridge, 172.19/16, most apps + traefik), `dokploy-network` (overlay 10.0.1/24), per-stack defaults.
- Databases: outline-postgres (pg18), bookorbit-db (pgvector pg18), supabase-db (supabase/postgres 17, dump user `supabase_admin`), dokploy-postgres (pg16, swarm), outline-redis.

**Host services**: AdGuard Home (host network: DNS 53, DHCP 67, UI 8085; `systemd-resolved` reconfigured to free :53), Incus with VM `devbox` (btrfs loop pool, 3 snapshots, `incusbr0`; rebuildable from the owner's dotfiles repo → fungible), cloudflared tunnel (incl. SSH), chrony, smartmontools, sysstat.

**Custom systemd units**: `host-firewall.service` (applies `/etc/host-firewall/rules{4,6}.txt` to INPUT + DOCKER-USER only), `homelab-services.service` + `yams.service` (compose up at boot), `traefik-homelab.service` (re-attach traefik to `homelab` on start events), `traefik-config-guard.path/.service` (re-apply local traefik.yml settings when Dokploy rewrites it), `homelab-selfcheck.timer` (4 min after boot), `server-backup.timer` (03:30, persistent).

**Cron**: `/etc/cron.d/{resource,service,storage}-check` every 5 min, `flock … | systemd-cat -t <name>`.

**Uptime Kuma 1.23.17** (no 2FA) at `http://uptime.home`, 21 monitors, all notifying one SMTP "Email" notification:
- push: Server Resources, Server Internal Services, Server Storage (cron scripts, 360 s), Server Backup (backup.sh, 90000 s — currently shows `[object Object]`, see spec 05 deviations), Cloudflare DDNS (pushed by the ddns container itself, 600 s);
- Kuma-run HTTP/keyword/DNS monitors for the public apps, Gluetun leak, AdGuard DNS, cloudflared readiness, devbox (paused);
- watchdog: an HTTP monitor pinging healthchecks.io every 60 s.

**Backups**: backup.sh (dumps every Postgres/MySQL container, tars `/opt/services /opt/yams /opt/scripts` to `/mnt/storage/backups/<stamp>`, 7-day retention, 20 GB cap) → offsite-backup.sh (restic → Cloudflare R2 with free-tier guard, ≤ 1 upload/20 h, Sunday prune + 2 % check, 8 GB cap). Log `/var/log/server-backup.log` is mounted into the `host-logs` container.

**Fungible (never back up)**: `/mnt/storage/media` (≈ 650 GB, re-downloadable), the Incus devbox VM.
**Gaps the migration closes**: Dokploy swarm volumes (stellar-data, Shortless Redis, Dokploy's /root/.docker) not off-site; modified `/etc/systemd/resolved.conf` not restorable; firewall rule files not restorable; stale `backup.sh.bak` holding a push token.
