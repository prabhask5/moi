#!/usr/bin/env bash
# serverstate bootstrap — rebuild a server on fresh hardware from its state bundle in R2.
#
# On the NEW machine (fresh Debian/Ubuntu install, network up):
#   1. curl https://rclone.org/install.sh | sudo bash
#   2. sudo rclone config            # recreate your R2 remote (same name as before, e.g. "r2")
#   3. sudo rclone cat r2:BUCKET/server-state/bootstrap.sh > bootstrap.sh
#      sudo bash bootstrap.sh r2:BUCKET/server-state [--dry-run]
#
# It downloads latest bundle, asks for the bundle passphrase, extracts it and starts
# the phased, interactive restore (each phase asks before it runs).
set -euo pipefail
REMOTE=${1:?usage: bootstrap.sh RCLONE_REMOTE [restore options, e.g. --dry-run]}
shift
[[ $EUID -eq 0 ]] || { echo "run as root (sudo)"; exit 1; }
command -v rclone >/dev/null || { echo "install rclone first: curl https://rclone.org/install.sh | sudo bash"; exit 1; }
if [[ -z ${RCLONE_CONFIG:-} && ! -s /root/.config/rclone/rclone.conf && -n ${SUDO_USER:-} ]]; then
  uc=$(getent passwd "$SUDO_USER" | cut -d: -f6)/.config/rclone/rclone.conf
  [[ -s $uc ]] && export RCLONE_CONFIG=$uc && echo "using rclone config $uc"
fi
for t in tar openssl python3; do
  command -v "$t" >/dev/null || { apt-get update -q && apt-get install -y -q tar openssl python3 rsync; break; }
done
dest=/root/serverstate-restore/$(date +%Y%m%d-%H%M%S)
mkdir -p "$dest"; chmod 700 /root/serverstate-restore
if rclone lsf "$REMOTE/latest.tar.gz.enc" 2>/dev/null | grep -q .; then
  rclone copyto "$REMOTE/latest.tar.gz.enc" "$dest/bundle.tar.gz.enc"
  read -rsp "Bundle passphrase (from your password manager): " pass </dev/tty; echo
  SS_PASS=$pass openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 -in "$dest/bundle.tar.gz.enc" -pass env:SS_PASS \
    | tar -xzf - -C "$dest" || { echo "decryption failed — wrong passphrase?"; exit 1; }
  (umask 077; printf '%s' "$pass" >"$dest/.passphrase")
else
  rclone copyto "$REMOTE/latest.tar.gz" "$dest/bundle.tar.gz"
  tar -xzf "$dest/bundle.tar.gz" -C "$dest"
fi
echo
echo "Bundle from: $(cat "$dest/state/snapshot/meta/hostname" 2>/dev/null)  captured $(cat "$dest/state/volatile/captured-at" 2>/dev/null)"
echo "Full description of the old server: $dest/state/CONTEXT.md"
echo
exec "$dest/tool/serverstate" restore --bundle "$dest" "$@" </dev/tty
