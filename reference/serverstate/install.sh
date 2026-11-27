#!/usr/bin/env bash
# Install serverstate.   sudo ./install.sh [--observe] [--noninteractive]
#
#   --observe   look-only install: tool + one snapshot + audit, nothing scheduled, no R2 upload,
#               no passphrase, no ~/.claude edits. Ideal for a first look; remove with
#               `sudo serverstate uninstall`.
#   (default)   full install: daily timer, change watcher, encrypted R2 pushes, Claude Code pointer.
#
# Everything this script creates or installs is recorded in /usr/local/lib/serverstate/install-manifest
# so `serverstate uninstall` can remove exactly that and nothing else. Re-run any time; config is kept.
set -euo pipefail
SRC=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
PREFIX=/usr/local/lib/serverstate
CONF=/etc/serverstate.conf
MANIFEST=$PREFIX/install-manifest
NONINT=0 OBSERVE=0
for a in "$@"; do case $a in --noninteractive) NONINT=1 ;; --observe) OBSERVE=1 ;; *) echo "unknown option $a"; exit 1 ;; esac; done
[[ $EUID -eq 0 ]] || { echo "run as root: sudo $0 $*"; exit 1; }
say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
install -d -m 755 "$PREFIX"
touch "$MANIFEST"
rec() { grep -qxF "$*" "$MANIFEST" 2>/dev/null || echo "$*" >>"$MANIFEST"; }   # rec KIND VALUE

say "Dependencies"
DEPS=(git rsync python3 openssl inotify-tools file)
if command -v dpkg-query >/dev/null && command -v apt-get >/dev/null; then
  missing=(); for p in "${DEPS[@]}"; do dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'install ok installed' || missing+=("$p"); done
  if ((${#missing[@]})); then
    echo "  installing: ${missing[*]}"
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q "${missing[@]}" >/dev/null || echo "  (some packages failed — continuing)"
    for p in "${missing[@]}"; do dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'install ok installed' && rec "pkg $p"; done
  else echo "  all present"; fi
elif command -v dnf >/dev/null; then
  for p in "${DEPS[@]}"; do rpm -q "$p" >/dev/null 2>&1 || { dnf install -y -q "$p" && rec "pkg $p"; } || true; done
fi

say "Installing to $PREFIX"
if [[ $SRC != "$PREFIX" ]]; then
  install -m 755 "$SRC/serverstate" "$SRC/sshelper.py" "$SRC/bootstrap.sh" "$SRC/install.sh" "$SRC/uninstall.sh" "$PREFIX/"
  install -m 644 "$SRC/serverstate.conf.example" "$PREFIX/"
  [[ -f $SRC/README.md ]] && install -m 644 "$SRC/README.md" "$PREFIX/"
fi
rec "dir $PREFIX"
ln -sf "$PREFIX/serverstate" /usr/local/sbin/serverstate; rec "file /usr/local/sbin/serverstate"

if [[ ! -f $CONF ]]; then
  say "Creating $CONF"
  install -m 600 "$PREFIX/serverstate.conf.example" "$CONF"; rec "file $CONF"
  if ((!NONINT)); then
    if ((!OBSERVE)) && command -v rclone >/dev/null; then
      echo "  rclone remotes available to root: $(rclone listremotes 2>/dev/null | xargs || true)"
      read -rp "  R2 path for state bundles (e.g. r2:my-bucket/server-state/$(hostname -s)), Enter to skip: " r </dev/tty || r=""
      [[ -n $r ]] && sed -i "s|^RCLONE_REMOTE=\"\"|RCLONE_REMOTE=\"$r\"|" "$CONF"
    fi
    read -rp "  Paths your existing backup uploads (space separated, for coverage checks), Enter to skip: " b </dev/tty || b=""
    [[ -n $b ]] && sed -i "s|^BACKUP_PATHS=\"\"|BACKUP_PATHS=\"$b\"|" "$CONF"
    read -rp "  Roots of your SSD/HDD data whose directory layout to record (Enter = auto-detect): " d </dev/tty || d=""
    [[ -n $d ]] && sed -i "s|^DATA_ROOTS=auto|DATA_ROOTS=\"$d\"|" "$CONF"
  fi
fi
((OBSERVE)) && sed -i 's/^AUTO_PUSH=yes/AUTO_PUSH=no/' "$CONF"
# shellcheck source=/dev/null
. "$CONF"
STATE_DIR=${STATE_DIR:-/var/lib/serverstate}
[[ -d $STATE_DIR ]] || rec "state $STATE_DIR"

ADMIN=${SUDO_USER:-}
H=""; [[ -n $ADMIN && $ADMIN != root ]] && H=$(getent passwd "$ADMIN" | cut -d: -f6)
if [[ -n $H && -z ${CONTEXT_COPIES:-} ]]; then
  sed -i "s|^CONTEXT_COPIES=\"\"|CONTEXT_COPIES=\"$H/SERVER_CONTEXT.md\"|" "$CONF"
  # shellcheck source=/dev/null
  . "$CONF"
fi
for c in ${CONTEXT_COPIES:-}; do rec "file $c"; done

if ((!OBSERVE)); then
  PF=${PASSPHRASE_FILE:-/root/.serverstate-passphrase}
  if [[ ! -s $PF ]]; then
    (umask 077; openssl rand -base64 33 | tr -d '\n' >"$PF"); rec "secret $PF"
    echo
    echo "  ┌────────────────────────────────────────────────────────────────────────┐"
    echo "  │ Bundle encryption passphrase created: $PF"
    echo "  │   $(cat "$PF")"
    echo "  │ SAVE IT IN YOUR PASSWORD MANAGER NOW. Without it the R2 bundle can't   │"
    echo "  │ be decrypted on new hardware. (Also save your R2 access keys.)         │"
    echo "  └────────────────────────────────────────────────────────────────────────┘"
    echo
  fi
  if [[ -n $H ]]; then
    for d in "$H" /root; do
      f=$d/.claude/CLAUDE.md
      target=$([[ $d == /root ]] && echo "$STATE_DIR/CONTEXT.md" || echo "$H/SERVER_CONTEXT.md")
      [[ -e $f ]] || rec "file $f"
      [[ -d $d/.claude ]] || rec "emptydir $d/.claude"
      mkdir -p "$d/.claude"
      if ! grep -q 'serverstate:begin' "$f" 2>/dev/null; then
        cat >>"$f" <<EOF

<!-- serverstate:begin -->
## This server
A complete, auto-updated description of this machine (hardware, disks, docker stacks/networks/volumes,
ports, domains, firewall, cron, systemd, scripts, monitoring, backups, recent changes) is at: $target
Read it before proposing or making any change here. Keep changes reproducible (compose files in
existing stack dirs, scripts in /usr/local/bin, units in /etc/systemd/system, crontab). After changing
anything run \`sudo serverstate snapshot\`, and record the purpose with \`sudo serverstate notes\`.
<!-- serverstate:end -->
EOF
        rec "block $f"
        [[ $d == "$H" ]] && chown -R "$ADMIN:" "$d/.claude"
      fi
    done
  fi

  say "Installing systemd units"
  cat >/etc/systemd/system/serverstate-snapshot.service <<'EOF'
[Unit]
Description=serverstate: capture server state, regenerate CONTEXT.md, push bundle
After=docker.service network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/serverstate snapshot --quiet --auto-push
Nice=10
IOSchedulingClass=idle
EOF
  cat >/etc/systemd/system/serverstate-snapshot.timer <<'EOF'
[Unit]
Description=serverstate daily snapshot

[Timer]
OnCalendar=*-*-* 03:41
RandomizedDelaySec=20m
Persistent=true

[Install]
WantedBy=timers.target
EOF
  cat >/etc/systemd/system/serverstate-watch.service <<'EOF'
[Unit]
Description=serverstate: auto-snapshot shortly after docker/config/cron/script changes
After=docker.service local-fs.target

[Service]
ExecStart=/usr/local/sbin/serverstate watch
Restart=always
RestartSec=30
Nice=10
IOSchedulingClass=idle

[Install]
WantedBy=multi-user.target
EOF
  for u in serverstate-snapshot.service serverstate-snapshot.timer serverstate-watch.service; do rec "unit $u"; done
  systemctl daemon-reload
  systemctl enable --now serverstate-snapshot.timer >/dev/null
  systemctl enable serverstate-watch.service >/dev/null
fi

say "First snapshot (can take a minute)"
/usr/local/sbin/serverstate snapshot || echo "  snapshot reported problems — see output above"
say "Coverage & reboot audit (sizes directories at low IO priority — can take a few minutes)"
/usr/local/sbin/serverstate audit || echo "  audit reported problems — see output above"
((OBSERVE)) || systemctl restart serverstate-watch.service

echo
if ((OBSERVE)); then
  say "Observe-only install done. Nothing is scheduled; nothing was uploaded."
  echo "  Read / share:  ${CONTEXT_COPIES:-$STATE_DIR/CONTEXT.md}"
  echo "  Gaps found:    sudo less $STATE_DIR/audit.md"
  echo "  Refresh:       sudo serverstate snapshot && sudo serverstate audit"
  echo "  Remove:        sudo serverstate uninstall            (keeps $STATE_DIR so moi can import it)"
  echo "                 sudo serverstate uninstall --purge    (removes everything)"
else
  say "Backup hook"
  cands=$(grep -hvE '^\s*#' /var/spool/cron/crontabs/* /etc/crontab /etc/cron.d/* 2>/dev/null | grep -iE 'backup|restic|rclone|borg|kopia|r2' || true)
  if [[ -n $cands ]]; then echo "  Backup-looking cron jobs:"; echo "$cands" | sed 's/^/    /'; fi
  echo "  Hook it: sudo serverstate hook-backup /path/to/backup.sh   — and add $STATE_DIR to what the backup uploads."
  echo
  serverstate status || true
  say "Done. Context for Claude: $STATE_DIR/CONTEXT.md ${CONTEXT_COPIES:+(copy: $CONTEXT_COPIES)}"
  echo "  Remove later: sudo serverstate uninstall [--purge]"
fi
