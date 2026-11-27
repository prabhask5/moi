# Coverage & reboot audit — cestmoi
_Generated 2026-11-26 17:00 UTC. Read-only findings: ✗ = gap, review = decide, ✓ = checked and fine, fungible = declared not worth backing up. Close gaps in `/etc/moi/moi.toml` (`[backup]`, `[data] fungible`, `[restore]`) or explain them with `moi notes`._

## Would it survive a reboot?
- ✓ service **ssh.service** is disabled but started on demand by ssh.socket
- ✓ `/var/lib/incus/storage-pools/default` (a loop image) is mounted at boot by incus-startup.service
- ✓ 6 custom firewall rules: chains INPUT, DOCKER-USER are applied at boot by moi (`firewall.manage`)
- ✓ docker.service is enabled
- docker.service drop-in `10-boot-order.conf`: After=host-firewall.service
- docker.service drop-in `storage.conf`: Environment=DOCKER_TMPDIR=/mnt/storage/docker-tmp
- docker.service drop-in `wait-for-storage.conf`: RequiresMountsFor=/mnt/storage
- ✓ single-node swarm: the manager comes back by itself and restarts its services
- Dokploy redeploys its services only once `dokploy-postgres` is up: both are swarm services and start together

## Large data not covered by the backup
- ✗ 2 large directories are outside the backup (see the table)
| Size | Path | Note |
|---|---|---|
| 650 GB | /mnt/storage/media | fungible (media library — temporary, re-downloadable) |
| 38.0 GB | /var/lib/docker/overlay2 | regenerable (images/packages/cache) |
| 21.0 GB | /var/lib/incus | fungible (devbox VM — rebuilt from github.com/prabhask5/dotfiles) |
| 9.0 GB | /mnt/storage/backups/2026-11-26_0330 | local backup copies |
| 1.2 GB | /home/prabhas/projects | ✗ not backed up |
| 700 MB | /home/prabhas/.cache | regenerable (cache) |
| 600 MB | /var/lib/docker/image | regenerable (images/packages/cache) |
| 300 MB | /var/lib/apt/lists | regenerable (images/packages/cache) |
| 250 MB | /var/lib/private-app/data | ✗ not backed up |

## Docker data a rebuild could lose
- review `/mnt/storage/database/db-data` — bind mount → supabase-db:/var/lib/postgresql/data: raw database files, covered by the dump of supabase-db
- fungible `/mnt/storage/media`: media library — temporary, re-downloadable
- review `/opt/services/outline/pgdata` — bind mount → outline-postgres:/var/lib/postgresql: raw database files, covered by the dump of outline-postgres
- review `/opt/yams/config/bookorbit-db` — bind mount → bookorbit-db:/var/lib/postgresql: raw database files, covered by the dump of bookorbit-db
- review `/var/lib/docker/volumes/dokploy-postgres-database/_data` — volume dokploy-postgres-database → swarm service dokploy-postgres: raw database files, covered by the dump of dokploy-postgres
- fungible `/var/lib/incus`: devbox VM — rebuilt from github.com/prabhask5/dotfiles
- ✗ container **supabase-pooler** has 350 MB written inside the container itself (not in a volume): lost when it is recreated
- review image **compiler-ui-a5b6:latest** has no registry digest: built by Dokploy from git, which rebuilds it on redeploy
- review image **dokploy-bridge-c7d8:latest** has no registry digest: built by Dokploy from git, which rebuilds it on redeploy
- review image **shortless-app-p9q1:latest** has no registry digest: built by Dokploy from git, which rebuilds it on redeploy
- review image **stellar-planner-x7k2:latest** has no registry digest: built by Dokploy from git, which rebuilds it on redeploy
- review 7 swarm services: a rebuild needs their definitions back before they can be redeployed (for Dokploy: Dokploy itself and its Postgres data)

## Configuration a restore would not put back
- ✓ read by your units, cron jobs and scripts, so restored with them: `/etc/docker`, `/etc/dokploy`, `/etc/host-firewall`, `/etc/ssh`
- ✗ 1 custom file(s) in /etc that no package owns and no restore phase puts back — add the ones that matter to `restore.etc_paths`:
```
/etc/sysctl.d/99-homelab.conf
```
- ✓ `/etc/systemd/resolved.conf` differs from the version its package ships; it is put back by the restore (in `restore.etc_paths` or another phase)

## Application state outside the usual places
- ✗ not in the backup: `/var/lib/private-app`
| Path | Size | Config captured | In backup |
|---|---|---|---|
| /var/lib/private-app | 251 MB |  | ✗ |
| /opt/scripts | 2.0 MB | yes | ✓ |
| /opt/services | 400 MB | yes | ✓ |
| /opt/yams | 1.1 GB | yes | ✓ |

## Storage placement
- Docker data root `/var/lib/docker` is on SSD
- review **portainer** `/data` ← `/opt/yams/config/portainer`: bulk data on SSD
- review **qbittorrent** `/config` ← `/opt/yams/config/qbittorrent`: bulk data on SSD
- review **qbittorrent** has 2 separate bulk mounts: hardlinks between them cannot work — use one `/data` mount
- ✗ **supabase-db** `/etc/postgresql-custom` ← `/mnt/storage/database/db-config`: database/config on HDD
- review **uptime-kuma** `/app/data` ← `/opt/services/monitoring/uptime-kuma/data`: bulk data on SSD
- review **dokploy-postgres** `/var/lib/postgresql/data` ← `/var/lib/docker/volumes/dokploy-postgres-database/_data`: bulk data on SSD
- review **shortless-redis-m3n4** `/data` ← `/var/lib/docker/volumes/shortless-redis-m3n4-data/_data`: bulk data on SSD
- review **stellar-planner-x7k2** `/app/data` ← `/var/lib/docker/volumes/stellar-data/_data`: bulk data on SSD
| Container | Disk | Type | Mounted at | Source | Flag |
|---|---|---|---|---|---|
| adguard | SSD | bind | /opt/adguardhome/conf | /opt/services/adguard/conf |  |
| adguard | SSD | bind | /opt/adguardhome/work | /opt/services/adguard/work |  |
| bazarr | SSD | bind | /config | /opt/yams/config/bazarr |  |
| bazarr | HDD | bind | /data | /mnt/storage/media |  |
| bookorbit | HDD | bind | /books | /mnt/storage/media/books |  |
| bookorbit-db | SSD | bind | /var/lib/postgresql | /opt/yams/config/bookorbit-db |  |
| dokploy-traefik | SSD | bind | /etc/dokploy/traefik/dynamic | /etc/dokploy/traefik/dynamic |  |
| gluetun | SSD | bind | /gluetun | /opt/yams/config/gluetun |  |
| homepage | SSD | bind | /app/config | /opt/services/homepage/config |  |
| jellyfin | SSD | bind | /config | /opt/yams/config/jellyfin |  |
| jellyfin | HDD | bind | /data | /mnt/storage/media |  |
| outline | HDD | bind | /var/lib/outline/data | /mnt/storage/outline/data |  |
| outline-postgres | SSD | bind | /var/lib/postgresql | /opt/services/outline/pgdata |  |
| portainer | SSD | bind | /data | /opt/yams/config/portainer | bulk data on SSD |
| prowlarr | SSD | bind | /config | /opt/yams/config/prowlarr |  |
| prowlarr | HDD | bind | /data | /mnt/storage/media |  |
| qbittorrent | SSD | bind | /config | /opt/yams/config/qbittorrent | bulk data on SSD |
| qbittorrent | HDD | bind | /downloads | /mnt/storage/media/downloads |  |
| radarr | SSD | bind | /config | /opt/yams/config/radarr |  |
| radarr | HDD | bind | /data | /mnt/storage/media |  |
| sabnzbd | SSD | bind | /config | /opt/yams/config/sabnzbd |  |
| sabnzbd | HDD | bind | /downloads | /mnt/storage/media/downloads |  |
| scrutiny | SSD | bind | /opt/scrutiny/config | /opt/services/monitoring/scrutiny/config |  |
| seerr | SSD | bind | /app/config | /opt/services/seerr/config |  |
| sonarr | SSD | bind | /config | /opt/yams/config/sonarr |  |
| sonarr | HDD | bind | /data | /mnt/storage/media |  |
| supabase-db | HDD | bind | /etc/postgresql-custom | /mnt/storage/database/db-config | database/config on HDD |
| supabase-db | HDD | bind | /var/lib/postgresql/data | /mnt/storage/database/db-data |  |
| supabase-storage | HDD | bind | /var/lib/storage | /mnt/storage/database/storage |  |
| uptime-kuma | SSD | bind | /app/data | /opt/services/monitoring/uptime-kuma/data | bulk data on SSD |
| dokploy | SSD | bind | /etc/dokploy | /etc/dokploy |  |
| dokploy | SSD | volume | /root/.docker | /var/lib/docker/volumes/dokploy-docker-config/_data |  |
| dokploy-postgres | SSD | volume | /var/lib/postgresql/data | /var/lib/docker/volumes/dokploy-postgres-database/_data | bulk data on SSD |
| shortless-redis-m3n4 | SSD | volume | /data | /var/lib/docker/volumes/shortless-redis-m3n4-data/_data | bulk data on SSD |
| stellar-planner-x7k2 | SSD | volume | /app/data | /var/lib/docker/volumes/stellar-data/_data | bulk data on SSD |
_Flags are keyword-based (`[audit.placement]` hot / bulk): use judgment._

## Secrets written into scripts
- review `/opt/scripts/service-check.sh`: SONARR_KEY — captured and restored as it is; move it to `secrets.toml` (a migrated check reads `secret:` references instead)

## Programs listening on the host outside Docker
- ✓ udp 0.0.0.0:53 — AdGuardHome: provided by container adguard (host network)

## Monitoring integrity
- review `/opt/scripts/backup.sh.bak` is a stale copy holding a token no monitor has (…pTok): delete it
- review monitor **Server Backup** last received a malformed message (`[object Object]`): whatever feeds it sends an object, not text
