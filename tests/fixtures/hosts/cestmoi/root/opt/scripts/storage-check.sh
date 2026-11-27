#!/usr/bin/env bash
# cestmoi storage check -> one Uptime Kuma push monitor ("cestmoi: storage").
# Run every 5 min by cron as root (smartctl and the kernel log need root).
#
# Checks:
#   - Disk space and inode usage on each filesystem
#   - Any filesystem remounted read-only (happens after disk/filesystem errors)
#   - USB media drive mounted AND writable
#   - SMART health for the NVMe SSD and the USB HDD (failing, wear, bad sectors, temperature)
#   - Disk/USB errors in the kernel log since the last run
#   - A disk disappearing entirely (USB drive dropped off the bus)

KUMA_PUSH="http://uptime.home/api/push/FAKE_TOKEN"

MOUNTS="/ /mnt/storage"     # filesystems to check for space / read-only
MEDIA_MOUNT="/mnt/storage"  # the USB drive: must be mounted and writable
DISK_LIMIT=85               # alert at this % of space used
INODE_LIMIT=85              # alert at this % of inodes used

# device | smartctl device type | temperature limit in °C
DISKS=(
  "/dev/nvme0n1|nvme|70"
  "/dev/sda|sat|55"
)
NVME_WEAR_LIMIT=90          # alert when the SSD reports this % of its rated life used
ALLOWED_BAD_SECTORS=0       # HDD reallocated + pending + uncorrectable sectors allowed before alerting

bad=""
info=""

# 1. SMART health for each disk
for d in "${DISKS[@]}"; do
  IFS='|' read -r dev type tmax <<< "$d"
  name=$(basename "$dev")
  if [ ! -e "$dev" ]; then bad+="$name (disk missing) "; continue; fi

  out=$(timeout 30 smartctl -H -A -d "$type" "$dev" 2>&1); rc=$?
  if [ "$rc" -eq 124 ]; then bad+="$name (SMART timed out) "; continue; fi
  if (( rc & 2 )); then bad+="$name (SMART unreadable) "; continue; fi
  (( rc & 8 ))  && bad+="$name (SMART says FAILING) "
  (( rc & 16 )) && bad+="$name (SMART attribute past failure threshold) "

  temp=""
  if [ "$type" = "nvme" ]; then
    cw=$(awk -F: '/Critical Warning/ {gsub(/ /,"",$2); print $2; exit}' <<< "$out")
    used=$(awk -F: '/Percentage Used/ {gsub(/[^0-9]/,"",$2); print $2; exit}' <<< "$out")
    merr=$(awk -F: '/Media and Data Integrity Errors/ {gsub(/[^0-9]/,"",$2); print $2; exit}' <<< "$out")
    temp=$(awk -F: '/^Temperature:/ {gsub(/[^0-9]/,"",$2); print $2; exit}' <<< "$out")
    [ -n "$cw" ] && [ "$cw" != "0x00" ] && bad+="$name (critical warning $cw) "
    [[ "$used" =~ ^[0-9]+$ ]] && { info+="$name ${used}% worn, "; [ "$used" -ge "$NVME_WEAR_LIMIT" ] && bad+="$name (${used}% of rated life used) "; }
    [[ "$merr" =~ ^[0-9]+$ ]] && [ "$merr" -gt 0 ] && bad+="$name ($merr media errors) "
  else
    # 5 = Reallocated_Sector_Ct, 197 = Current_Pending_Sector, 198 = Offline_Uncorrectable
    sectors=$(awk '$1==5 || $1==197 || $1==198 {s+=$10} END {print s+0}' <<< "$out")
    temp=$(awk '$1==194 || $1==190 {print $10; exit}' <<< "$out")
    [ "$sectors" -gt "$ALLOWED_BAD_SECTORS" ] && bad+="$name ($sectors bad/pending sectors) "
  fi

  if [[ "$temp" =~ ^[0-9]+$ ]]; then
    info+="$name ${temp}C, "
    [ "$temp" -ge "$tmax" ] && bad+="$name (${temp}C, too hot) "
  fi
done

# 2. Disk / USB errors in the kernel log since the last run
errs=$(journalctl -k --since "-6min" --no-pager -q 2>/dev/null \
  | grep -ciE 'I/O error|EXT4-fs error|XFS .*error|Buffer I/O|blk_update_request|critical medium error|uas_eh|reset SuperSpeed|device offline')
[ "${errs:-0}" -gt 0 ] && bad+="kernel log ($errs disk/USB errors in last 6 min, see: journalctl -k --since -10min) "

# 3. Space, inodes, read-only
for m in $MOUNTS; do
  read -r pct ipct < <(timeout 10 df --output=pcent,ipcent "$m" 2>/dev/null | tail -1 | tr -d '%')
  if ! [[ "$pct" =~ ^[0-9]+$ ]]; then bad+="$m (df failed) "; continue; fi
  info+="$m ${pct}% used, "
  [ "$pct" -ge "$DISK_LIMIT" ] && bad+="$m (${pct}% full) "
  [[ "$ipct" =~ ^[0-9]+$ ]] && [ "$ipct" -ge "$INODE_LIMIT" ] && bad+="$m (${ipct}% of inodes used) "
  findmnt -no OPTIONS --target "$m" | grep -qE '(^|,)ro(,|$)' && bad+="$m (read-only) "
done

# 4. Media drive mounted and writable (last: a hung USB drive can block)
if ! mountpoint -q "$MEDIA_MOUNT"; then
  bad+="$MEDIA_MOUNT (not mounted) "
elif ! timeout 10 touch "$MEDIA_MOUNT/.storage-check" 2>/dev/null; then
  bad+="$MEDIA_MOUNT (not writable) "
else
  rm -f "$MEDIA_MOUNT/.storage-check"
fi

# 5. Report to Kuma
if [ -n "$bad" ]; then status=down; msg="Problem: $bad"
else status=up; msg="OK: ${info%, }"; fi
echo "$status: $msg"
curl -fsS -m 15 --retry 2 -G "$KUMA_PUSH" \
  --data-urlencode "status=$status" --data-urlencode "msg=$msg" -o /dev/null
