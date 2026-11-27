#!/usr/bin/env bash
# backup.sh — nightly backup of every database + all service configs to the HDD.
# New services are picked up automatically:
#   - every running Postgres or MySQL/MariaDB container is dumped
#   - everything under CONFIG_DIRS is archived (configs, SQLite DBs, compose files)
# Run as root.
set -uo pipefail

DEST=/mnt/storage/backups
CONFIG_DIRS=(/opt/services /opt/yams /opt/scripts)
KEEP_DAYS=7
MAX_TOTAL_GB=20    # if all backups together exceed this, the run counts as failed (Kuma alerts)
KUMA_PUSH_URL="http://uptime.home/api/push/FAKE_TOKEN?status=up&msg=OK&ping=="   # optional: Uptime Kuma push URL, pinged only if everything succeeded

STAMP=$(date +%F_%H%M)
OUT="$DEST/$STAMP"
fail=0
excludes=()
log() { echo "[$(date '+%F %T')] $*"; }
envval() { docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$1" | sed -n "s/^$2=//p" | head -1; }
has() { docker exec "$1" sh -c "command -v $2" >/dev/null 2>&1; }

# Never write backups to the SSD if the HDD isn't mounted
mountpoint -q /mnt/storage || { log "ERROR: /mnt/storage not mounted, aborting"; exit 1; }
mkdir -p "$OUT/db"

for c in $(docker ps --format '{{.Names}}'); do
  img=$(docker inspect -f '{{.Config.Image}}' "$c")

  # ---- Postgres: any container with PGDATA set and pg_dumpall available
  pgdata=$(envval "$c" PGDATA)
  if [[ -n "$pgdata" ]] && has "$c" pg_dumpall; then
    if [[ "$img" == *supabase/postgres* ]]; then user=supabase_admin
    else user=$(envval "$c" POSTGRES_USER); user=${user:-postgres}; fi
    pw=$(envval "$c" POSTGRES_PASSWORD)
    log "Dumping Postgres: $c (user $user)"
    docker exec -e PGPASSWORD="$pw" "$c" pg_dumpall -h 127.0.0.1 -U "$user" | gzip > "$OUT/db/$c.sql.gz"
    if [[ ${PIPESTATUS[0]} -ne 0 ]]; then log "ERROR: dump failed for $c"; fail=1; fi
    # Skip this DB's raw data folder in the config archive (a live copy is useless)
    while IFS='|' read -r src dst; do
      [[ -n "$src" && "$pgdata" == "$dst"* ]] && excludes+=(--exclude="$src")
    done < <(docker inspect -f '{{range .Mounts}}{{.Source}}|{{.Destination}}{{"\n"}}{{end}}' "$c")
    continue
  fi

  # ---- MySQL / MariaDB
  rootpw=$(envval "$c" MARIADB_ROOT_PASSWORD); rootpw=${rootpw:-$(envval "$c" MYSQL_ROOT_PASSWORD)}
  if [[ -n "$rootpw" ]]; then
    dump=$(has "$c" mariadb-dump && echo mariadb-dump || echo mysqldump)
    log "Dumping MySQL/MariaDB: $c"
    docker exec -e MYSQL_PWD="$rootpw" "$c" $dump -u root --all-databases --single-transaction | gzip > "$OUT/db/$c.sql.gz"
    if [[ ${PIPESTATUS[0]} -ne 0 ]]; then log "ERROR: dump failed for $c"; fail=1; fi
    while IFS='|' read -r src dst; do
      [[ "$dst" == /var/lib/mysql* ]] && excludes+=(--exclude="$src")
    done < <(docker inspect -f '{{range .Mounts}}{{.Source}}|{{.Destination}}{{"\n"}}{{end}}' "$c")
  fi
done

# Raw database folders (already dumped) for the off-site step to skip
printf '%s\n' "${excludes[@]#--exclude=}" > "$OUT/raw-db-dirs"

# ---- Configs (includes every app's SQLite DB and your compose files)
dirs=(); for d in "${CONFIG_DIRS[@]}"; do [[ -d "$d" ]] && dirs+=("$d"); done
log "Archiving configs: ${dirs[*]}"
tar -czf "$OUT/configs.tar.gz" --warning=no-file-changed --warning=no-file-removed \
    "${excludes[@]}" --exclude='*/cache' --exclude='*/Cache' --exclude='*/transcodes' \
    --exclude='*/logs' --exclude='*/log' "${dirs[@]}"
rc=$?; if (( rc > 1 )); then log "ERROR: tar failed ($rc)"; fail=1; fi

# ---- Retention
find "$DEST" -mindepth 1 -maxdepth 1 -type d -mtime +"$KEEP_DAYS" -exec rm -rf {} +

du -sh "$OUT" | sed 's/^/Backup size: /'
total_gb=$(( $(du -sb "$DEST" | cut -f1) / 1073741824 ))
log "All backups: ${total_gb} GB (limit ${MAX_TOTAL_GB} GB)"
if (( total_gb > MAX_TOTAL_GB )); then log "ERROR: backups exceed ${MAX_TOTAL_GB} GB"; fail=1; fi
# ---- Off-site copy to Cloudflare R2 (encrypted, deduplicated)
if (( fail == 0 )); then
  /opt/scripts/offsite-backup.sh "$OUT" "${dirs[@]}" || { log "ERROR: off-site backup failed"; fail=1; }
fi

if (( fail == 0 )); then
  log "Backup OK: $OUT"
  [[ -n "$KUMA_PUSH_URL" ]] && curl -fsS -m 10 "$KUMA_PUSH_URL?status=up&msg=OK" >/dev/null
else
  log "Backup finished WITH ERRORS: $OUT"
  reason=$(grep -h "ERROR" /var/log/server-backup.log | tail -n 1 | cut -c1-180)
  [[ -n "${KUMA_PUSH_URL:-}" ]] && curl -fsS -m 10 -G "$KUMA_PUSH_URL" --data-urlencode "status=down" --data-urlencode "msg=${reason:-backup failed}" >/dev/null
  exit 1
fi
