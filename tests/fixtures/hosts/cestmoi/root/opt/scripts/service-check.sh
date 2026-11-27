#!/usr/bin/env bash
# cestmoi service check -> one Uptime Kuma push monitor ("cestmoi: services").
# Run every 5 min by cron as root. Storage checks live in storage-check.sh.
#
# Checks:
#   - Every container is running and not unhealthy
#     (Swarm/Dokploy containers are checked by service replica count instead)
#   - Each app answers over HTTP (and the arrs/Gluetun don't report problems)
#   - Each database accepts connections
#   - Edge: Traefik routes LAN + public traffic, has no errors, firewall rules intact
# Auto-fixes:
#   - VPN stack (Gluetun + the apps sharing its network)
#   - Traefik losing the "homelab" network after a Dokploy update

KUMA_PUSH="http://uptime.home/api/push/FAKEservicesTok002"
SONARR_KEY="FAKEsonarrapikey0000000000000000"; RADARR_KEY="FAKEradarrapikey0000000000000000"; PROWLARR_KEY="FAKEprowlarrapikey00000000000000"
IGNORE=""   # container names allowed to be stopped, space-separated

# HTTP checks, reached over the "homelab" Docker network.
# name | container to reach (or localhost) | port/path | API key | text that means trouble
CHECKS=(
  # Media
  "jellyfin|jellyfin|8096/health||"
  "seerr|seerr|5055/api/v1/status||"
  "sonarr|sonarr|8989/api/v3/health|$SONARR_KEY|\"error\""
  "radarr|radarr|7878/api/v3/health|$RADARR_KEY|\"error\""
  "prowlarr|prowlarr|9696/api/v1/health|$PROWLARR_KEY|\"error\""
  "bazarr|bazarr|6767/||"
  # Downloaders (inside Gluetun's network)
  "qbittorrent|gluetun|8081/||"
  "sabnzbd|gluetun|8080/||"
  "gluetun-portforward|gluetun|8000/v1/portforward||\"port\":0"
  # Apps
  "outline|outline|3000/_health||"
  "bookorbit|bookorbit|3000/api/v1/health||"
  "supabase|supabase-envoy|8000/auth/v1/health||"
  # Infrastructure
  "adguard|localhost|8085/||"
  "scrutiny|scrutiny|8080/api/health||"
  "portainer|portainer|9000/||"
  "homepage|homepage|3000/||"
  "glances|glances|61208/||"
)

# Database checks, run inside each container.
# name | container | command | text the output must contain (optional)
EXEC_CHECKS=(
  "outline-postgres|outline-postgres|pg_isready -q|"
  "bookorbit-db|bookorbit-db|pg_isready -q|"
  "supabase-db|supabase-db|pg_isready -q -U postgres|"
  "outline-redis|outline-redis|redis-cli ping|PONG"
)

# Edge checks through Traefik.
EDGE_LAN=( "home" "dokploy.home" "jellyfin.home" )          # Host headers, sent to Traefik on :80
EDGE_PUBLIC=(                                               # full round trip via Cloudflare
  "https://notes.example.com/"
  "https://books.example.com/"
  "https://media.example.com/"
  "https://status.example.com/"
  "https://database.example.com/auth/v1/health"
  "https://shortless.example.com/api/health"
  "https://compiler.example.com/healthz"
)
# Firewall rules that must exist in DOCKER-USER (matched as text).
FW_REQUIRED=(
  "Intra-network container traffic"
  "Swarm outbound Internet"
  "devbox: VM to Internet"
  "-A DOCKER-USER -j DROP"
)

bad=""

# 1. Container state: stopped, exited, restarting, or unhealthy (non-Swarm containers)
total=0
while read -r name state health task; do
  name=${name#/}
  [ -n "$task" ] && continue            # Swarm task container: checked in 1b
  total=$((total+1))
  case " $IGNORE " in *" $name "*) continue;; esac
  [ "$health" = "-" ] && health=""
  if [ "$state" != "running" ] || [ "$health" = "unhealthy" ]; then
    bad+="$name ($state${health:+, $health}) "
  fi
done < <(docker inspect -f '{{.Name}} {{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}-{{end}} {{index .Config.Labels "com.docker.swarm.task.id"}}' $(docker ps -aq))

# 1b. Swarm services (Dokploy + apps it deploys): every replica running
svc_total=0
while read -r svc reps _; do
  svc_total=$((svc_total+1))
  cur=${reps%%/*}; want=${reps#*/}
  [ "$cur" != "$want" ] && bad+="$svc (swarm $reps) "
done < <(docker service ls --format '{{.Name}} {{.Replicas}}' 2>/dev/null)

# 2. HTTP checks: does each app answer, and does it report problems
addr_of() {
  [ "$1" = "localhost" ] && { echo 127.0.0.1; return; }
  docker inspect -f '{{(index .NetworkSettings.Networks "homelab").IPAddress}}' "$1" 2>/dev/null
}
for line in "${CHECKS[@]}"; do
  IFS='|' read -r name ctr path key trouble <<< "$line"
  addr=$(addr_of "$ctr")
  [ -z "$addr" ] && { bad+="$name (no IP) "; continue; }
  hdr=(); [ -n "$key" ] && hdr=(-H "X-Api-Key: $key")
  resp=$(curl -sS -m 10 "${hdr[@]}" -w $'\n%{http_code}' "http://$addr:$path" 2>/dev/null)
  code=${resp##*$'\n'}; body=${resp%$'\n'*}
  if [ -z "$code" ] || [ "$code" = "000" ] || [ "$code" -ge 500 ]; then
    bad+="$name (no response) "; continue
  fi
  if [ -n "$key" ] && [ "$code" != "200" ]; then bad+="$name (HTTP $code, check API key) "; continue; fi
  if [ -n "$trouble" ] && grep -qF -- "$trouble" <<< "$body"; then bad+="$name (reports problem) "; fi
done

# 3. Database checks
for line in "${EXEC_CHECKS[@]}"; do
  IFS='|' read -r name ctr cmd expect <<< "$line"
  out=$(timeout 15 docker exec "$ctr" $cmd 2>&1); rc=$?
  if [ "$rc" -ne 0 ] || { [ -n "$expect" ] && ! grep -qF -- "$expect" <<< "$out"; }; then
    bad+="$name (not accepting connections) "
  fi
done

# 4. Edge
# 4a. Traefik must be on "homelab", or every non-Dokploy service returns 502. Auto-fix.
if ! docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' dokploy-traefik 2>/dev/null | grep -qw homelab; then
  if docker network connect homelab dokploy-traefik 2>/dev/null; then
    bad+="[traefik had lost the homelab network: reconnected; next check confirms the fix] "
  else
    bad+="traefik (not on homelab network, reconnect failed) "
  fi
fi

# 4b. LAN routing through Traefik
for h in "${EDGE_LAN[@]}"; do
  code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' -H "Host: $h" http://127.0.0.1/)
  case "$code" in 2*|3*|401) ;; *) bad+="traefik-lan:$h (HTTP $code) ";; esac
done

# 4c. Public routing end to end (Cloudflare -> router -> Traefik -> app)
# On failure: say which stage broke, retry once, note what DNS says right now, log it.
PROBE_LOG=/var/log/service-check-public.log
probe() {   # sets P_CODE, P_RC, P_TOTAL, P_MSG
  local url=$1 errf out err dns conn tls ttfb ip
  errf=$(mktemp)
  out=$(curl -sS -m 15 -o /dev/null \
    -w '%{http_code} %{time_namelookup} %{time_connect} %{time_appconnect} %{time_starttransfer} %{time_total} %{remote_ip}' \
    "$url" 2>"$errf"); P_RC=$?
  err=$(tail -n1 "$errf" | sed 's/^curl: ([0-9]*) //'); rm -f "$errf"
  read -r P_CODE dns conn tls ttfb P_TOTAL ip <<< "$out"
  z() { awk -v x="$1" 'BEGIN{exit !(x+0==0)}'; }
  case $P_RC in
    0)  P_MSG="HTTP $P_CODE in ${P_TOTAL}s"
        case "$P_CODE" in 52[0-9]|530) P_MSG+=" (Cloudflare couldn't reach the origin)";; esac;;
    6)  P_MSG="DNS lookup failed";;
    7)  P_MSG="connection refused by ${ip:-?}";;
    28) if   z "$dns";  then P_MSG="timed out in DNS lookup"
        elif z "$conn"; then P_MSG="timed out connecting to $ip"
        elif z "$tls";  then P_MSG="timed out in TLS handshake with $ip"
        elif z "$ttfb"; then P_MSG="connected to $ip, no response from origin"
        else P_MSG="response too slow"; fi;;
    35|60) P_MSG="TLS/cert error: $err";;
    52|56) P_MSG="connection dropped by ${ip:-?}: $err";;
    *)  P_MSG="curl error $P_RC: $err";;
  esac
  [ "$P_RC" -ne 0 ] && P_MSG+=" after ${P_TOTAL}s"
}
for u in "${EDGE_PUBLIC[@]}"; do
  probe "$u"
  case "$P_CODE" in 2*|3*|401) continue;; esac
  first=$P_MSG
  sleep 3; probe "$u"
  case "$P_CODE" in 2*|3*|401) again="retry OK in ${P_TOTAL}s";; *) again="retry failed: $P_MSG";; esac
  host=${u#https://}; host=${host%%/*}
  ip_now=$(getent ahostsv4 "$host" | awk '{print $1; exit}')
  entry="public:${u#https://} ($first; $again; DNS now: ${ip_now:-NOT RESOLVING})"
  [[ $again == "retry OK"* ]] || bad+="$entry "
  echo "$(date '+%F %T') $entry" >> "$PROBE_LOG"
done

# 4d. Traefik errors in the last 5 minutes (config problems, Let's Encrypt coming back, etc.)
errs=$(docker logs --since 5m dokploy-traefik 2>&1 | grep -ciE ' ERR |level=error')
[ "${errs:-0}" -gt 0 ] && bad+="traefik ($errs errors in 5m: docker logs dokploy-traefik) "

# 4e. Firewall rules intact
fw=$(iptables -S DOCKER-USER 2>/dev/null)
for want in "${FW_REQUIRED[@]}"; do
  grep -qF -- "$want" <<< "$fw" || bad+="firewall (missing: $want) "
done

# 4f. Self-healing: Traefik config guard + homelab watcher
fixed=$(python3 /opt/scripts/traefik-config-guard.py 2>&1)
[ -n "$fixed" ] && bad+="[$fixed; next check confirms] "
if ! systemctl is-active --quiet traefik-homelab; then
  systemctl restart traefik-homelab
  bad+="[traefik-homelab watcher was down: restarted] "
fi

# 5. Auto-fix the VPN stack: restart Gluetun, then the apps that share its network
VPN_DEPS="qbittorrent sabnzbd"
HEAL_STAMP=/var/tmp/service-check-heal
HEAL_COOLDOWN=1800   # at most one auto-restart every 30 minutes

vpn_broken=0
grep -q 'gluetun' <<< "$bad" && vpn_broken=1
for c in $VPN_DEPS; do grep -qE "(^| )$c \(" <<< "$bad" && vpn_broken=1; done

if [ "$vpn_broken" = 1 ]; then
  last=$(cat "$HEAL_STAMP" 2>/dev/null || echo 0)
  now=$(date +%s)
  if [ $((now - last)) -ge "$HEAL_COOLDOWN" ]; then
    echo "$now" > "$HEAL_STAMP"
    docker restart gluetun >/dev/null
    for i in $(seq 1 24); do   # wait up to 2 minutes for Gluetun to be healthy
      [ "$(docker inspect -f '{{.State.Health.Status}}' gluetun 2>/dev/null)" = "healthy" ] && break
      sleep 5
    done
    docker restart $VPN_DEPS >/dev/null
    bad+="[auto-restarted gluetun + $VPN_DEPS; next check confirms the fix] "
  else
    bad+="[auto-restart already tried in the last 30 min, needs a look] "
  fi
fi

# 6. Report to Kuma
if [ -n "$bad" ]; then status=down; msg="Failing: $bad"
else status=up; msg="All $total containers + $svc_total swarm services running, ${#CHECKS[@]} app, ${#EXEC_CHECKS[@]} database, $(( ${#EDGE_LAN[@]} + ${#EDGE_PUBLIC[@]} )) edge checks OK"; fi
echo "$status: $msg"
curl -fsS -m 15 --retry 2 -G "$KUMA_PUSH" \
  --data-urlencode "status=$status" --data-urlencode "msg=$msg" -o /dev/null
