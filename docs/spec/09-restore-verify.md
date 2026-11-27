# 09 — Restore & verify

`moi restore --bundle DIR` rebuilds a server on fresh hardware from a state bundle (08) plus the data backup (07). It is phased, interactive, resumable and dry-runnable. The prototype's restore (`reference/serverstate/serverstate`, `restore_*` functions) is the reference; moi keeps its safety rules and adds the items below.

```
moi restore --bundle DIR [--dry-run] [--only a,b] [--from phase] [--skip a,b] [--yes]
moi restore --list
```

Rules carried over: each phase asks before running (`--yes` skips the questions but **never** a disk format or a firewall confirmation); every overwritten file is kept as `*.pre-moi`; non-fatal errors warn and continue; everything is logged to `/var/lib/moi/restore/restore.log`; a disk is formatted only after typing `FORMAT /dev/xxx`; network config (netplan) is shown, never applied (interface names differ); disks that were physically moved are recognized by UUID and their fstab lines kept.

## Phases

| Phase | Does |
|---|---|
| preflight | compare old/new (OS, release, CPU arch, disks, RAM); install base tools |
| packages | apt repos + keys (same release only unless confirmed), manual packages (kernel/boot packages excluded, unavailable ones listed), holds, snaps, pipx, npm -g; list manual binaries that must be reinstalled |
| users | groups and users with the same UIDs/GIDs, password hashes, supplementary groups |
| home | dotfiles, `~/.ssh`, tool configs captured in the bundle |
| storage | interactive mapping of old mounts to new devices; mdadm assemble / zpool import / vgchange; fstab with `nofail` |
| dirs | recreate data-root directory trees with owners and modes |
| docker-engine | install Docker + compose plugin (distro packages if that is what the old server used — cestmoi: `docker.io`, `docker-compose-v2`), restore `/etc/docker` and docker.service drop-ins, enable |
| data | `restic restore` of the identity paths (latest `nightly` snapshot or `--snapshot ID`) — fungible paths are not in the backup and are recreated empty with original owners |
| config | `/etc` paths in `restore.etc_paths` (default list: ssh incl. host keys, sysctl, modules, sudoers.d, environment, logrotate, fail2ban/ufw/nftables, wireguard, cloudflared, samba/exports, docker, dokploy…) **plus** `/etc` files read by custom units/scripts **plus** all modified package conffiles (diff shown, e.g. `resolved.conf`); hostname; timezone; sshd reload after `sshd -t` |
| scripts | script dirs + every script referenced by units/cron |
| incus | if captured: install incus, `incus admin init --preseed` from the captured preseed (pools/networks/profiles); instances: configs restored, disks only if not fungible — otherwise print the declared rebuild note (cestmoi: devbox from the dotfiles repo) |
| docker | networks with original subnets (compose-owned networks pre-created with their compose labels so compose adopts them); **databases first**: for each stack with DB services, `up -d <db services>`, wait healthy, import the dump if the data dir is empty (`psql -f` / `mysql <`), then `up -d` the rest; stacks in `boot.compose_up` order; standalone containers via reviewed scripts; **swarm/Dokploy**: `docker swarm init`, Dokploy installed at the captured version with its official installer, then Dokploy's Postgres restored from its dump and `/etc/dokploy` from the backup, then Dokploy redeploys its apps (research spike in M8 decides the exact sequence and whether services are recreated from captured specs instead) |
| moi | install `moi.service` with the restored `/etc/moi`; the daemon takes over boot tasks, firewall, checks, heal, backups |
| systemd | remaining custom units not replaced by moi; re-enable units that were enabled |
| cron | crontabs + /etc/cron.* (warns about backup jobs first; moi's own schedule replaces legacy ones) |
| firewall | `moi firewall apply` with auto-rollback over SSH (type `ok` within 90 s from a new session) |
| monitors | Kuma data came back with the data restore; `monitors sync --dry-run` must show only `keep/adopt`; pushes resume with the same tokens |
| verify | expectations check (§ Verify), then the **guided reboot** |

## Verify (`moi verify`)

Expectations are written to `/var/lib/moi/expect.json` from the bundle (what ran on the old server) and refreshed by every snapshot on the new one:
- containers that were running are running; swarm services at desired replicas;
- non-ephemeral listening ports (proto + port, excluding ≥ 32768 unless pinned) present;
- mounts present and writable where required;
- enabled units active (or triggered-by-able), no failed units;
- cron entries and moi schedules present;
- firewall: moi-managed chains contain the configured rules; required DOCKER-USER texts present;
- all moi checks `up`, Kuma pushes accepted (HTTP 200), watchdog monitor active;
- `moi backup --dry-run` succeeds (repository reachable, guard readable).

Output: a checklist with ✓/✗ and the fix hint for each ✗; exit 1 if any ✗.

### Guided reboot

The final phase records "verify after next boot", asks the owner to reboot (`moi restore` prints the exact command and does not reboot by itself), and on the next boot the daemon runs `moi verify` after `boot.verify_after`, writes the result to the journal and `/run/motd.d/50-moi`, and pushes `boot.verify_monitor`. The same post-boot verify runs on **every** boot afterwards (replacing cestmoi's `homelab-selfcheck`), so "survives a reboot" is continuously proven, not assumed.

## Acceptance

- [ ] Full dry run against the cestmoi fixture bundle lists every action without touching the host.
- [ ] VM-based end-to-end test (nightly CI, not per-PR): build a server with compose + a Postgres stack + a swarm service + custom unit + firewall rules, back up to a local restic repo, restore onto a fresh VM, reboot, `moi verify` all ✓.
- [ ] Moved-disk recognition, FORMAT confirmation, firewall rollback, DB-first import each have tests.
