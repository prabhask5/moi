#!/usr/bin/env bash
# offsite-backup.sh: encrypted, deduplicated copy of tonight's backup to Cloudflare R2 (restic).
# Called by backup.sh after the local backup:  offsite-backup.sh <tonight's backup dir> <config dir>...
#
# Stays inside R2's free tier (10 GB-month storage, 1M Class A + 10M Class B ops per month):
#   - BEFORE uploading, reads the account's real R2 usage for the last 30 days from Cloudflare's
#     analytics and refuses to run if it is past half the free allowance, or if usage can't be read
#   - uploads raw folders (only changes are sent), in large packs (fewer requests)
#   - at most one upload per 20 hours, whatever calls it
#   - keeps the repository under MAX_REPO_GB
# Exits non-zero on any refusal or failure, so backup.sh skips its "OK" push and Kuma alerts.
set -uo pipefail

ENV_FILE=/root/.config/restic/r2.env   # RESTIC_*, AWS_* keys, CF_ACCOUNT_ID, CF_API_TOKEN
STAMP_FILE=/var/lib/offsite-backup/last-upload
MAX_REPO_GB=8                          # free tier is 10 GB-month
MAX_CLASS_A=500000                     # 50% of the 1M free Class A ops (30 days, whole account)
MAX_CLASS_B=5000000                    # 50% of the 10M free Class B ops
MAX_STORAGE_GB=8                       # whole account, per Cloudflare's metrics
MIN_HOURS_BETWEEN=20
EXTRA_PATHS=(                          # outside the config dirs backup.sh archives
  /etc/dokploy /etc/iptables /etc/cron.d /etc/ssh/sshd_config.d /etc/systemd/system /etc/docker
  /mnt/storage/database/db-config /mnt/storage/database/storage /mnt/storage/outline/data
)
GRAPHQL_URL=${CF_GRAPHQL_URL:-https://api.cloudflare.com/client/v4/graphql}
log() { echo "[$(date '+%F %T')] offsite: $*"; }

OUT=${1:?usage: offsite-backup.sh <backup dir> <config dir>...}; shift
CONFIG_DIRS=("$@")
[[ -f "$ENV_FILE" ]] || { log "ERROR: $ENV_FILE missing"; exit 1; }
set -a; . "$ENV_FILE"; set +a
for v in RESTIC_REPOSITORY RESTIC_PASSWORD_FILE AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY CF_ACCOUNT_ID CF_API_TOKEN; do
  [[ -n "${!v:-}" ]] || { log "ERROR: $v not set in $ENV_FILE"; exit 1; }
done
for c in restic curl jq; do command -v "$c" >/dev/null || { log "ERROR: $c not installed"; exit 1; }; done

# ---- 1. Rate limit: never more than one upload per MIN_HOURS_BETWEEN hours
mkdir -p "$(dirname "$STAMP_FILE")"
if [[ -f "$STAMP_FILE" ]]; then
  age_h=$(( ( $(date +%s) - $(cat "$STAMP_FILE") ) / 3600 ))
  if (( age_h < MIN_HOURS_BETWEEN )); then
    log "skipped: last upload was ${age_h}h ago (minimum ${MIN_HOURS_BETWEEN}h)"; exit 0
  fi
fi

# ---- 2. Free-tier guard: real R2 usage, whole account, last 30 days. Fails closed.
CLASS_A_ACTIONS='["ListBuckets","PutBucket","ListObjects","ListObjectsV2","PutObject","CopyObject","CreateMultipartUpload","UploadPart","UploadPartCopy","CompleteMultipartUpload","ListMultipartUploads","ListParts","PutBucketEncryption","PutBucketCors","PutBucketLifecycleConfiguration","LifecycleStorageTierTransition"]'
FREE_ACTIONS='["DeleteObject","DeleteObjects","DeleteBucket","AbortMultipartUpload"]'
since=$(date -u -d '30 days ago' +%FT%TZ); now=$(date -u +%FT%TZ)
query=$(jq -n --arg a "$CF_ACCOUNT_ID" --arg s "$since" --arg e "$now" '{query:
  "query($a:String!,$s:Time!,$e:Time!){viewer{accounts(filter:{accountTag:$a}){
     ops:r2OperationsAdaptiveGroups(limit:10000,filter:{datetime_geq:$s,datetime_leq:$e}){sum{requests} dimensions{actionType}}
     sto:r2StorageAdaptiveGroups(limit:10000,filter:{datetime_geq:$s,datetime_leq:$e}){max{payloadSize metadataSize} dimensions{bucketName}}
  }}}", variables:{a:$a,s:$s,e:$e}}')
resp=$(curl -fsS -m 30 -X POST "$GRAPHQL_URL" -H "Authorization: Bearer $CF_API_TOKEN" \
  -H 'Content-Type: application/json' --data "$query") \
  || { log "ERROR: cannot read R2 usage from Cloudflare (refusing to upload)"; exit 1; }
if ! jq -e '.errors == null and .data.viewer.accounts[0] != null' >/dev/null <<<"$resp"; then
  log "ERROR: R2 usage query failed (refusing to upload): $(jq -c '.errors' <<<"$resp" | head -c 300)"; exit 1
fi
read -r class_a class_b storage_b < <(jq -r --argjson A "$CLASS_A_ACTIONS" --argjson F "$FREE_ACTIONS" '
  .data.viewer.accounts[0] as $acc
  | ([$acc.ops[] | select(.dimensions.actionType as $t | $A | index($t)) | .sum.requests] | add // 0) as $a
  | ([$acc.ops[] | select(.dimensions.actionType as $t | ($A | index($t) | not) and ($F | index($t) | not)) | .sum.requests] | add // 0) as $b
  | ([$acc.sto | group_by(.dimensions.bucketName)[] | (map(.max.payloadSize + .max.metadataSize) | max)] | add // 0) as $s
  | "\($a) \($b) \($s)"' <<<"$resp")
storage_gb=$(( storage_b / 1073741824 ))
log "R2 usage, last 30 days: Class A ${class_a}/${MAX_CLASS_A}, Class B ${class_b}/${MAX_CLASS_B}, storage ${storage_gb}/${MAX_STORAGE_GB} GB"
if (( class_a >= MAX_CLASS_A || class_b >= MAX_CLASS_B || storage_gb >= MAX_STORAGE_GB )); then
  log "ERROR: R2 usage past the safety limit, refusing to upload (check the R2 dashboard)"; exit 1
fi

# ---- 3. Upload
paths=("$OUT/db")
for p in "${CONFIG_DIRS[@]}" "${EXTRA_PATHS[@]}"; do [[ -e "$p" ]] && paths+=("$p"); done
excl=$(mktemp); trap 'rm -f "$excl"' EXIT
printf '%s\n' '*/cache' '*/Cache' '*/transcodes' '*/logs' '*/log' \
  /etc/dokploy/applications /etc/dokploy/compose /etc/dokploy/logs > "$excl"
[[ -f "$OUT/raw-db-dirs" ]] && cat "$OUT/raw-db-dirs" >> "$excl"

restic unlock --quiet 2>/dev/null   # clears stale locks left by a crash or reboot mid-backup
attempt=1
while :; do
  log "uploading ${#paths[@]} paths (attempt $attempt)"
  restic backup --quiet --tag nightly --pack-size 64 --exclude-file "$excl" --exclude-caches "${paths[@]}"
  rc=$?
  (( rc == 0 || rc == 3 )) && break
  (( attempt >= 2 )) && { log "ERROR: restic backup failed ($rc)"; exit 1; }
  attempt=$((attempt + 1)); log "backup failed ($rc), retrying in 10 minutes"; sleep 600
done
(( rc == 3 )) && log "warning: some files could not be read (snapshot still saved)"
date +%s > "$STAMP_FILE"

# ---- 4. Retention: daily forget is cheap; pruning (which rewrites data) only on Sundays
if [[ $(date +%u) == 7 ]]; then
  log "retention + prune"
  restic forget --quiet --tag nightly --keep-daily 7 --keep-weekly 4 --keep-monthly 6 --prune \
    || { log "ERROR: restic forget/prune failed"; exit 1; }
  log "integrity check (re-reads 2% of the data)"
  restic check --quiet --read-data-subset=2% || { log "ERROR: restic check failed"; exit 1; }
else
  restic forget --quiet --tag nightly --keep-daily 7 --keep-weekly 4 --keep-monthly 6 \
    || { log "ERROR: restic forget failed"; exit 1; }
fi

bytes=$(restic stats --quiet --mode raw-data --json | jq -r '.total_size // 0')
log "repository size: $(( bytes / 1048576 )) MB (limit ${MAX_REPO_GB} GB)"
(( bytes / 1073741824 >= MAX_REPO_GB )) && { log "ERROR: repository near the free tier"; exit 1; }
log "done"
