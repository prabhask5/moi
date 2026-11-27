#!/usr/bin/env bash
# Remove serverstate — exactly what install.sh / hook-backup recorded, nothing else.
#
#   sudo serverstate uninstall                  remove tool, units, config, hooks, ~/.claude blocks, context copies.
#                                               KEEPS /var/lib/serverstate (history; moi can import it) and the passphrase.
#   sudo serverstate uninstall --purge          also delete the state dir, passphrase and restore leftovers
#   add --remove-packages                       also apt-remove dependencies that install.sh installed (none it found present)
#   add --yes                                   don't ask
set -uo pipefail
PREFIX=/usr/local/lib/serverstate
MANIFEST=$PREFIX/install-manifest
CONF=/etc/serverstate.conf
PURGE=0 RMPKG=0 YES=0
for a in "$@"; do case $a in --purge) PURGE=1 ;; --remove-packages) RMPKG=1 ;; --yes|-y) YES=1 ;; *) echo "unknown option $a"; exit 1 ;; esac; done
[[ $EUID -eq 0 ]] || { echo "run as root"; exit 1; }
say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
STATE_DIR=/var/lib/serverstate PASSPHRASE_FILE=/root/.serverstate-passphrase RCLONE_REMOTE="" CONTEXT_COPIES=""
# shellcheck source=/dev/null
[[ -r $CONF ]] && . "$CONF"
m() { grep -E "^$1 " "$MANIFEST" 2>/dev/null | cut -d' ' -f2-; }

if ((!YES)); then
  echo "This removes serverstate from $(hostname)."
  ((PURGE)) && echo "  --purge: ALSO deletes $STATE_DIR (all history) and $PASSPHRASE_FILE" \
            || echo "  Keeps: $STATE_DIR (history, for moi to import) and $PASSPHRASE_FILE"
  read -rp "Continue? [y/N] " a </dev/tty; [[ $a =~ ^[Yy] ]] || exit 0
fi

say "Stopping and removing systemd units"
for u in $(m unit) serverstate-watch.service serverstate-snapshot.timer serverstate-snapshot.service; do
  [[ -e /etc/systemd/system/$u ]] || continue
  systemctl disable --now "$u" >/dev/null 2>&1
  rm -f "/etc/systemd/system/$u"; echo "  removed $u"
done
systemctl daemon-reload 2>/dev/null

say "Removing backup-script hooks"
for f in $(m hook | sort -u); do
  [[ -f $f ]] || continue
  if grep -q 'serverstate' "$f"; then
    sed -i -e '/^# serverstate: capture full server config/d' -e '\#^/usr/local/sbin/serverstate snapshot#d' "$f"
    echo "  unhooked $f"
  fi
  rm -f "$f.pre-serverstate"
done
leftover=$(grep -rlsE '^/usr/local/sbin/serverstate snapshot' /opt /usr/local/bin /root /home /etc/cron.d 2>/dev/null | grep -v serverstate | head -5)
[[ -n $leftover ]] && echo "  NOTE: still referenced in: $leftover (edit by hand)"

say "Removing Claude Code pointer blocks"
for f in $(m block); do
  [[ -f $f ]] || continue
  sed -i '/<!-- serverstate:begin -->/,/<!-- serverstate:end -->/d' "$f"
  if ! grep -q '[^[:space:]]' "$f"; then rm -f "$f"; echo "  removed $f (was only serverstate)"; else echo "  cleaned $f"; fi
done
for d in $(m emptydir); do rmdir "$d" 2>/dev/null && echo "  removed empty $d"; done

say "Removing context copies, config, command"
for f in $CONTEXT_COPIES $(m file); do
  [[ $f == "$CONF" ]] && continue
  [[ -e $f || -L $f ]] && { rm -f "$f"; echo "  removed $f"; }
done
if [[ -f $CONF ]]; then
  [[ -d $STATE_DIR ]] && ((!PURGE)) && cp -p "$CONF" "$STATE_DIR/serverstate.conf.saved"
  rm -f "$CONF"; echo "  removed $CONF$([[ -d $STATE_DIR ]] && ((!PURGE)) && echo " (copy kept in $STATE_DIR)")"
fi
rm -f /run/serverstate.lock

if ((PURGE)); then
  say "Purging state"
  rm -rf "$STATE_DIR" && echo "  removed $STATE_DIR"
  [[ -e $PASSPHRASE_FILE ]] && rm -f "$PASSPHRASE_FILE" && echo "  removed $PASSPHRASE_FILE"
  rm -rf /root/serverstate-restore
  [[ -n $RCLONE_REMOTE ]] && echo "  Bundles uploaded to R2 are NOT deleted. To remove them: rclone purge $RCLONE_REMOTE"
else
  echo "  kept $STATE_DIR ($(du -sh "$STATE_DIR" 2>/dev/null | cut -f1)) and $PASSPHRASE_FILE"
fi

if ((RMPKG)); then
  pk=$(m pkg | xargs)
  if [[ -n $pk ]]; then say "Removing packages installed by serverstate: $pk"; DEBIAN_FRONTEND=noninteractive apt-get remove -y -q $pk >/dev/null && echo "  done"; fi
else
  pk=$(m pkg | xargs); [[ -n $pk ]] && echo "  Packages installed by serverstate were kept: $pk  (re-run with --remove-packages to remove)"
fi

rm -rf "$PREFIX"
say "serverstate removed."
