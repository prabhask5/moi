#!/usr/bin/env bash
# disk-audit.sh — map every running container's mounts to SSD / HDD / RAM
# and flag likely mistakes. Read-only: it changes nothing.
# Run with: sudo bash disk-audit.sh
set -uo pipefail

RED=$'\e[31m'; YEL=$'\e[33m'; GRN=$'\e[32m'; DIM=$'\e[2m'; BLD=$'\e[1m'; RST=$'\e[0m'

# Paths that should live on fast storage (databases, app configs/SQLite)
HOT='postgres|pgdata|postgresql|redis|/config|/db($|/)|database|sqlite|/var/lib/glances|/app/data'
# Paths that hold bulk files and belong on the big HDD
BULK='/data($|/)|media|movies|/tv($|/)|music|books|download|torrent|usenet|complete|library|/var/lib/storage|backup'
# Mounts to ignore
SKIP='docker\.sock|/etc/localtime|/etc/timezone|/dev/|/proc|/sys|/run/'

[[ $EUID -ne 0 ]] && echo "${YEL}Tip: run with sudo so sizes of Docker volumes can be read.${RST}"

disk_type() {               # prints SSD, HDD, RAM or ?
  local src dev rota
  src=$(df --output=source "$1" 2>/dev/null | tail -1)
  case "$src" in tmpfs|shm|overlay) echo RAM; return;; esac
  rota=$(lsblk -no ROTA "$src" 2>/dev/null | head -1 | tr -d ' ')
  case "$rota" in 0) echo SSD;; 1) echo HDD;; *) echo '?';; esac
}

echo "${BLD}== Disks ==${RST}"
lsblk -o NAME,SIZE,ROTA,TRAN,FSTYPE,MOUNTPOINT | sed 's/^/  /'
echo "  ${DIM}ROTA 1 = spinning HDD, 0 = SSD; TRAN usb = connected over USB${RST}"
echo
root=$(docker info -f '{{.DockerRootDir}}')
echo "${BLD}== Docker data root ==${RST}  $root  →  $(disk_type "$root")"
echo

problems=0
ssd_sizes=$(mktemp)

echo "${BLD}== Container mounts ==${RST}"
for c in $(docker ps --format '{{.Names}}' | sort); do
  echo "${BLD}$c${RST}"
  bulk_mounts=0
  while IFS='|' read -r type src dst; do
    [[ -z "$src" ]] && continue
    [[ "$src$dst" =~ $SKIP ]] && continue
    [[ -f "$src" ]] && continue                      # single config files
    t=$(disk_type "$src")
    lc=$(echo "$src $dst" | tr '[:upper:]' '[:lower:]')
    flag=""
    if [[ $lc =~ $BULK ]]; then
      bulk_mounts=$((bulk_mounts+1))
      [[ $t == SSD ]] && { flag="${YEL}⚠ bulk data on SSD${RST}"; problems=$((problems+1)); }
    elif [[ $lc =~ $HOT ]]; then
      [[ $t == HDD ]] && { flag="${RED}✗ database/config on HDD${RST}"; problems=$((problems+1)); }
    fi
    if [[ $t == SSD ]]; then
      sz=$(timeout 30 du -sh "$src" 2>/dev/null | cut -f1)
      echo -e "${sz:-?}\t$c\t$dst" >> "$ssd_sizes"
    fi
    printf '  %-4s %-7s %s  →  %s %s\n' "$t" "$type" "$dst" "${DIM}$src${RST}" "$flag"
  done < <(docker inspect -f '{{range .Mounts}}{{.Type}}|{{.Source}}|{{.Destination}}{{"\n"}}{{end}}' "$c")

  # Hardlinks fail across separate mounts, even on the same disk
  if (( bulk_mounts > 1 )); then
    echo "  ${YEL}⚠ $bulk_mounts separate bulk mounts — hardlinks between them won't work; use one /data mount${RST}"
    problems=$((problems+1))
  fi

  # Data written inside the container (no volume) is lost on recreate and sits on the SSD
  rw=$(docker inspect --size -f '{{.SizeRw}}' "$c" 2>/dev/null || echo 0)
  if (( rw > 1073741824 )); then
    echo "  ${RED}✗ $((rw/1048576)) MB written inside the container itself (not in a volume)${RST}"
    problems=$((problems+1))
  fi
done
echo

echo "${BLD}== Biggest SSD users (container mounts) ==${RST}"
sort -hr "$ssd_sizes" | head -15 | sed 's/^/  /'
rm -f "$ssd_sizes"
echo
echo "${BLD}== Docker images / build cache ==${RST}"
docker system df | sed 's/^/  /'
echo
if (( problems == 0 )); then
  echo "${GRN}No obvious problems found.${RST}"
else
  echo "${YEL}$problems item(s) to review above.${RST} ${DIM}Flags are keyword-based; use judgment.${RST}"
fi
