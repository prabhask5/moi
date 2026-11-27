#!/usr/bin/env bash
# cestmoi resource check -> one Uptime Kuma push monitor ("Server Resources").
# Run every 5 min by cron as root. Reads the kernel directly (works even if Glances is down).
#
# Checks (sustained, so short spikes don't alert):
#   - CPU: average over the last 5 minutes (between runs) above CPU_LIMIT, two runs in a row
#   - RAM: used above MEM_LIMIT, two runs in a row
#   - Swap: used above SWAP_LIMIT
#   - CPU temperature above TEMP_LIMIT, two runs in a row (disk temps: storage-check.sh)
#   - Out-of-memory kills in the kernel log since the last run
#   - Containers that crashed and were auto-restarted since the last run
# Disk space, SMART and disk temperatures live in storage-check.sh.

KUMA_PUSH="http://uptime.home/api/push/FAKEresourcesTok01"
CPU_LIMIT=90      # % average over 5 min
MEM_LIMIT=90      # % of RAM in use (excluding reclaimable cache)
SWAP_LIMIT=80     # % of swap in use
TEMP_LIMIT=85     # °C, hottest CPU sensor
STATE=/var/lib/resource-check
mkdir -p "$STATE"

bad=""; info=""

# ---- CPU: delta of /proc/stat since the previous run = true 5-minute average
read -r _ u n s i w q sq st _ < /proc/stat
total=$((u + n + s + i + w + q + sq + st)); idle=$((i + w))
if [[ -f $STATE/cpu ]]; then
  read -r ptotal pidle < "$STATE/cpu"
  dt=$((total - ptotal)); di=$((idle - pidle))
  if (( dt > 0 )); then
    cpu=$(( (100 * (dt - di)) / dt ))
    info+="CPU ${cpu}%, "
    (( cpu >= CPU_LIMIT )) && hot_cpu=1
  fi
fi
echo "$total $idle" > "$STATE/cpu"

# ---- RAM and swap
mt=$(awk '/^MemTotal:/{print $2}' /proc/meminfo); ma=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo)
stt=$(awk '/^SwapTotal:/{print $2}' /proc/meminfo); sf=$(awk '/^SwapFree:/{print $2}' /proc/meminfo)
mem=$(( 100 * (mt - ma) / mt )); info+="RAM ${mem}%, "
(( mem >= MEM_LIMIT )) && hot_mem=1
if (( stt > 0 )); then
  swap=$(( 100 * (stt - sf) / stt )); info+="swap ${swap}%, "
  (( swap >= SWAP_LIMIT )) && bad+="swap ${swap}% used (memory pressure) "
fi

# ---- CPU temperature: hottest reading from the CPU's hwmon driver
temp=""
for h in /sys/class/hwmon/hwmon*; do
  case "$(cat "$h/name" 2>/dev/null)" in coretemp|k10temp|zenpower|cpu_thermal|acpitz) ;; *) continue ;; esac
  for f in "$h"/temp*_input; do
    [[ -r $f ]] || continue
    t=$(( $(cat "$f") / 1000 ))
    [[ -z $temp || $t -gt $temp ]] && temp=$t
  done
done
if [[ -n $temp ]]; then
  info+="CPU ${temp}C, "
  (( temp >= TEMP_LIMIT )) && hot_temp=1
fi

# ---- Sustained conditions: alert only on the second run in a row (about 10 minutes)
sustain() {  # name flag message
  local f="$STATE/hot-$1"
  if [[ -n $2 ]]; then
    if [[ -f $f ]]; then bad+="$3 "; else touch "$f"; fi
  else
    rm -f "$f"
  fi
}
sustain cpu  "${hot_cpu:-}"  "CPU ${cpu:-?}% for 10+ min"
sustain mem  "${hot_mem:-}"  "RAM ${mem}% for 10+ min"
sustain temp "${hot_temp:-}" "CPU ${temp:-?}C for 10+ min"

# ---- Out-of-memory kills since the last run
oom=$(journalctl -k --since "-6min" --no-pager -q 2>/dev/null | grep -ciE 'out of memory|oom-kill|killed process')
(( ${oom:-0} > 0 )) && bad+="kernel killed processes for lack of memory ($oom events; journalctl -k --since -10min) "

# ---- Containers that crashed and were auto-restarted since the last run
if command -v docker >/dev/null; then
  new=$(mktemp)
  docker inspect -f '{{.Name}} {{.RestartCount}}' $(docker ps -aq) 2>/dev/null | sed 's#^/##' | sort > "$new"
  if [[ -f $STATE/restarts ]]; then
    crashed=$(join "$STATE/restarts" "$new" | awk '$3 > $2 {printf "%s(+%d) ", $1, $3-$2}')
    [[ -n $crashed ]] && bad+="crashed and restarted: $crashed"
  fi
  mv "$new" "$STATE/restarts"
fi

# ---- Report
info=${info%, }
if [[ -n $bad ]]; then status=down; msg="Problem: $bad| $info"; else status=up; msg="OK: $info"; fi
echo "$status: $msg"
curl -fsS -m 15 --retry 2 -G "$KUMA_PUSH" --data-urlencode "status=$status" --data-urlencode "msg=$msg" -o /dev/null
