# serverstate

Capture **everything** about this server, keep it versioned, keep an always-current
`CONTEXT.md` for Claude, and rebuild the whole machine on new hardware from your R2 bucket.

```
            ┌───────────── serverstate snapshot ─────────────┐
 live box ─►│ /var/lib/serverstate  (git: every change logged)│─► CONTEXT.md (for Claude, auto-updated)
            └───────────────┬────────────────────────────────┘
                            │ push (encrypted, only when changed)
                            ▼
                 r2:bucket/server-state/<host>/latest.tar.gz.enc  + bootstrap.sh
                            │
 new box ◄── bootstrap.sh ──┘  phased restore + your data backup restore
```

## What gets captured

| Area | Captured |
|---|---|
| Docker | every container (inspect, normalised), compose stacks (files, `.env`, config dirs — not data), networks with exact subnets, named volumes, standalone containers → ready-to-run `docker run` scripts, Portainer stacks |
| Files | all of `/etc`, user crontabs, `/usr/local/bin` & other script dirs, **every script cron/systemd calls (and scripts those call)**, dotfiles, `~/.ssh`, rclone/restic/aws configs, `EXTRA_CAPTURE` |
| Storage | disks (SSD/HDD/NVMe, model, UUID, label), fstab, mounts, mdadm/LVM/ZFS/btrfs, usage, SMART health, **directory trees of SSD/HDD roots with owners+perms** |
| Packages | apt manual list + repos + keys, holds, snaps, flatpak, pipx, npm -g, pip, cargo, binaries in /usr/local/bin, /opt |
| Scheduling | all crontabs, /etc/cron.*, systemd timers with schedules |
| Services | enabled units, custom units in /etc/systemd/system, running/failed |
| Firewall | iptables/ip6tables (docker noise stripped, `DOCKER-USER` kept), nftables, ufw, fail2ban, CrowdSec, ipsets, sshd settings |
| Network | interfaces, routes, DNS, listening ports + processes, Tailscale, WireGuard, reverse-proxy domains (Traefik/Caddy labels, Caddyfile, nginx, cloudflared) |
| Users | users/groups with UIDs, password hashes, group memberships, SSH keys |

Not captured: the data itself (your existing R2 backup does that), docker images (re-pulled),
Tailscale identity, Cloudflare/router settings.

## Install

**Look-only first (recommended):** installs the tool, takes one snapshot and runs the audit.
Nothing is scheduled, nothing is uploaded, nothing else on the server is touched:
```bash
tar xzf serverstate.tar.gz && cd serverstate
sudo ./install.sh --observe
less ~/SERVER_CONTEXT.md            # everything moi/Claude will know about this server
sudo less /var/lib/serverstate/audit.md   # gaps: not backed up, not restorable, lost on reboot
```

**Full install** (timer, watcher, encrypted R2 pushes, Claude Code pointer):
```bash
sudo ./install.sh
```

**Remove:** everything the installer created is recorded in `/usr/local/lib/serverstate/install-manifest`.
```bash
sudo serverstate uninstall           # removes tool, units, hooks, ~/.claude blocks, config — keeps history for moi
sudo serverstate uninstall --purge   # removes history and passphrase too
```
The installer asks for your R2 path and backup paths, creates the encryption passphrase
(**save it + your R2 keys in a password manager**), installs a daily timer and a watcher service,
takes the first snapshot, writes `~/SERVER_CONTEXT.md` and adds a pointer to `~/.claude/CLAUDE.md`.

Then:

```bash
sudo nano /etc/serverstate.conf                 # check RCLONE_REMOTE, BACKUP_PATHS, DATA_ROOTS, BACKUP_RESTORE_CMD
sudo serverstate hook-backup /path/to/your-r2-backup.sh   # snapshot + push before every backup
# add /var/lib/serverstate to the paths your backup uploads
sudo serverstate notes                          # describe what things are FOR (merged into CONTEXT.md)
sudo serverstate push --force                   # first upload; check: rclone ls r2:…/server-state
```

## Audit — what would I forget?

`sudo serverstate audit` (also run by the installer) writes `/var/lib/serverstate/audit.md` and adds it to CONTEXT.md:

* **reboot survival** — running containers with restart policy `no`, running-but-disabled services,
  mounts missing from fstab, live iptables rules not saved to `rules.v4`, docker not enabled
* **not backed up** — large directories, docker volumes and bind mounts outside `BACKUP_PATHS`,
  data written inside containers, locally built images that can't be re-pulled
* **not restorable** — custom and modified `/etc` files the restore wouldn't put back
* app state in `/var/lib`, `/opt`, `/srv`; secrets embedded in scripts; programs listening outside docker

## Existing monitoring (read-only)

The snapshot inventories Uptime Kuma (read-only from its SQLite DB, found via the container's
`/app/data` mount) and every script / cron job that pushes to Kuma or healthchecks.io, and matches them
by push token. CONTEXT.md flags monitors nothing feeds and scripts pushing to unknown tokens.

## Day to day — nothing to do

* **Watcher** snapshots ~2 min after docker containers/networks/volumes are created/removed,
  or anything changes in `/etc`, crontabs, script dirs, compose files.
* **Timer** snapshots daily; **backup hook** snapshots + pushes before each backup.
* Pushes only happen when something changed (watcher/timer: at most hourly).

```bash
serverstate status          # last snapshot/push, watcher, warnings
sudo serverstate log        # what changed and when  (sudo serverstate diff HEAD~3)
```

## Using it with Claude

* **Claude Code on the server**: already wired — `~/.claude/CLAUDE.md` points to the context file.
* **claude.ai**: upload `~/SERVER_CONTEXT.md` to your project (re-upload after big changes).
* `CONTEXT.md` contains: a ⚠ attention list (full disks, stopped containers, failed units,
  data not covered by backup), your notes, hardware, disks, directory trees, ports, domains,
  every container with ports/networks/mounts/env (secrets redacted), firewall, cron, timers,
  units, scripts with descriptions + who calls them, packages, users, backup coverage, and
  the recent change log — plus rules telling the AI to keep changes reproducible.

## Moving to new hardware

**Old server** (final, consistent backup):
```bash
sudo serverstate final-backup     # snapshot → stop containers → BACKUP_CMD → push
# then disable the old server's backup cron so it can't overwrite the new one
```

**New server** (fresh Debian/Ubuntu, same release recommended):
```bash
curl https://rclone.org/install.sh | sudo bash
sudo rclone config                                    # recreate the R2 remote, same name
sudo rclone cat r2:BUCKET/server-state/HOST/bootstrap.sh > bootstrap.sh
sudo bash bootstrap.sh r2:BUCKET/server-state/HOST --dry-run   # see everything it would do
sudo bash bootstrap.sh r2:BUCKET/server-state/HOST             # do it (asks per phase)
```

Restore phases (each asks first; `--only`, `--from`, `--skip`, `--dry-run` available):

```
preflight      compare old/new machine, install base tools
packages       apt repos+keys, apt manual packages, snaps, pipx, npm -g
users          users, groups (same UIDs/GIDs), password hashes
home           dotfiles, ~/.ssh, rclone/restic configs, ~/bin
storage        interactive: map old mounts to new disks (moved disks auto-detected), fstab, RAID/ZFS/LVM
dirs           recreate SSD/HDD directory trees with owners & permissions
data           restore data from your backup (BACKUP_RESTORE_CMD) — or pause and resume with --from docker-engine
docker-engine  install docker + compose, restore daemon.json
config         selected /etc configs (ssh, sysctl, fail2ban, ufw, wireguard…), hostname, timezone
scripts        scripts + everything cron/systemd call
docker         networks with original subnets, compose stacks up (proxy first), standalone containers
systemd        custom units, re-enable what was enabled
cron           crontabs + /etc/cron.* (warns about backup jobs first)
firewall       ufw/nftables/iptables/fail2ban — over SSH auto-rolls back unless you confirm
verify         missing containers/ports/mounts, failed units; installs serverstate + continues history
```

Safety: never formats a disk unless you type `FORMAT /dev/xxx`; every overwritten file is kept
as `*.pre-serverstate`; network config (netplan) is shown, not applied (interface names differ);
everything is logged to `/root/serverstate-restore/restore.log`.

## Files

| Path | |
|---|---|
| `/usr/local/lib/serverstate/` | the tool (`serverstate`, `sshelper.py`, `bootstrap.sh`, `install.sh`) |
| `/etc/serverstate.conf` | settings |
| `/var/lib/serverstate/` | state (git), `CONTEXT.md`, `NOTES.md`, `snapshot/`, `volatile/` |
| `/root/.serverstate-passphrase` | bundle encryption key |

The state dir contains secrets (`/etc/shadow`, `.env`, SSH keys) — it's root-only (700) and the
R2 bundle is AES-256 encrypted. If you add it to your own backup, that backup should be encrypted too.
