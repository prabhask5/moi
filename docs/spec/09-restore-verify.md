# 09 — Restore & verify

`moi restore --bundle DIR` rebuilds a server on fresh hardware from a state bundle (08) plus the data backup (07). It is phased, interactive, resumable and dry-runnable. The serverstate prototype's restore (its `restore_*` functions) was the reference; moi keeps its safety rules and adds the items below.

```
moi restore --bundle DIR [--dry-run] [--only a,b] [--from phase] [--skip a,b] [--yes] [--snapshot ID]
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
| incus | if captured: install incus, `incus admin init --preseed` from the captured preseed (pools/networks/profiles); instances: configs restored, disks only if not fungible — otherwise print the declared rebuild note; a devbox VM is rebuilt by the `devbox` phase |
| docker | networks with original subnets (compose-owned networks pre-created with their compose labels so compose adopts them); **databases first**: for each stack with DB services, `up -d <db services>`, wait healthy, import the dump if the data dir is empty (`psql -f` / `mysql <`), then `up -d` the rest; stacks in `boot.compose_up` order; standalone containers via reviewed scripts; **swarm/Dokploy**: `docker swarm init`, overlay networks, services recreated from their captured descriptions with databases loaded from their dumps (see Decisions: Dokploy) |
| devbox | when `/etc/devbox` was restored: rebuild (instance missing) or reconcile (instance present) the devbox VM non-interactively through `devbox_core::up`, then check its health endpoint (15 § Restore) |
| moi | install `moi.service` with the restored `/etc/moi`; the daemon takes over boot tasks, firewall, checks, heal, backups |
| systemd | remaining custom units not replaced by moi; re-enable units that were enabled |
| cron | crontabs + /etc/cron.* (warns about backup jobs first; moi's own schedule replaces legacy ones) |
| firewall | asks for a typed `yes` (never taken by `--yes`), then applies exactly as `moi firewall apply` does: the rules in place saved first, and over SSH the rollback timer armed before anything changes and the new rules kept only when confirmed within 60 s |
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

### Decisions (M8, verify)

- **Expectations** (`<state root>/expect.json`, 0600) are derived from a state directory: containers that were running (swarm task containers left out — a service's tasks come back under other names), swarm services with their wanted replicas, listening ports as `protocol port` below 32768 (no pinning of higher ports yet), mount points of real filesystems, systemd services that were running, timers that were enabled, users with a crontab and files in `/etc/cron.d`.
- **When they change**: every snapshot rewrites them — except between a boot and its verification. The daemon notes at its first start after a boot that this boot is still to be verified, and until that has happened the expectations stay what they were *before* the boot. Otherwise a snapshot taken two minutes after a reboot would quietly drop the container that did not come back. After the verification they are refreshed, so something the owner stopped on purpose is reported once, not forever.
- **"Enabled units active (or triggered-by-able)"** is checked as: every *enabled* service that was running is running (one that the bus or a socket starts on demand — packagekit, ssh behind `ssh.socket` — stops when idle and is not held to running; found by the end-to-end test), every timer that was enabled is enabled, and nothing is in the failed state. An enabled unit that was not running before (a one-shot, a socket-activated service) is not expected to run.
- **The checklist**, in order: containers, swarm services, ports, mounts (and each `checks.storage.writable` mount written to), services, timers, failed units, cron, the daemon (answers on its socket), firewall (each family `in sync` with what moi applied, when managed; `checks.edge.firewall_required` texts present), every enabled check (a manual-style run), Kuma accepting each bound check's result (the results are really pushed — they are true), the watchdog monitor when `expect_watchdog`, and backups (`require_mount` mounted; `restic cat config` answers; R2 usage readable). A group with nothing missing is one `✓ … : all N` line; each missing thing is its own `✗` line with a `→` hint. Exit 1 if any `✗`. `--json`: `{ failed, checks: [{ok, what, hint}] }`.
- **After a boot** the daemon's `verify` job runs `boot.verify_after` after its start: failures as warnings and a summary in the journal (`MOI_JOB=verify`), the summary and the failed items in `/run/motd.d/50-moi`, `up`/`down` with the summary to `boot.verify_monitor`. A restart of the service is not a boot and verifies nothing.

### Decisions (M8, restore)

- **Dry run** prints every action as a `[dry-run] …` line and runs only commands that ask (`uname`, `lsblk`, `blkid`, `apt-cache`, `restic snapshots`); it asks nobody anything, writes no log, and reads the *bundle's* `moi.toml` where a real run reads the restored one. Its full transcript on the cestmoi bundle is a golden file.
- **`--json`** returns `{ ok, dry_run, warnings, log: [lines] }`; `--list --json` returns `{ phases: [{name, what}] }`.
- **The log** (`/var/lib/moi/restore/restore.log`, 0600) gets every line the terminal gets, plus the error output of commands that failed.
- **config** restores everything the bundle holds under `/etc`: the snapshot keeps only what is the server's own (files the owner wrote, changed package configuration, what units and scripts read), so `restore.etc_paths` is not consulted as a filter. Never copied between machines: `fstab`, `passwd`/`group`/`shadow`/`gshadow`, `hostname`, `machine-id`, `os-release`, `localtime`/`timezone` (set with `timedatectl`), `resolv.conf`, `mtab`, and what other phases own (`apt`, `cron*`, `systemd/system`, `docker`, `moi`). netplan files are copied to `/var/lib/moi/restore/netplan-old` to adapt by hand.
- **users**: password hashes go in through `chpasswd -e` on standard input (never on a command line). A system group that has members but whose package comes later (`docker`, `incus-admin`) is created with `groupadd -r` so the membership is not lost; the package adopts it.
- **storage**: a moved disk is mounted by the name the old fstab used when that was a `UUID=`/`LABEL=`, else by `UUID=`; a new one always by `UUID=` with `nofail` added. With nobody at the terminal no device is chosen and nothing is formatted. The previous fstab is kept as `/etc/fstab.pre-moi.<unix time>`.
- **data**: the repository, its password and keys come from the bundle's `secrets.toml` and reach restic through the environment only. Fungible paths that do not exist are created empty.
- **docker**: a stack that was not running on the old server gets its files back and is not started. A database's dump is the newest `<backup.local_dir>/<run>/db/<name>.sql.gz` that the data phase brought back, streamed into `psql -f -` / `mysql` over the Engine API; it is loaded only when the data directory (or volume) holds nothing. Swarm: a one-node swarm, overlay networks, and services recreated from their captured description (image, environment, mounts, networks, published ports, constraints, replicas); a service whose image was built on the old server (image repository = service name, as Dokploy builds them) cannot be pulled and is listed for redeploying. Standalone containers: their generated script is shown (environment values hidden) and run after a yes.
- **dirs** leaves the inside of database data directories alone: a database initializes only an empty directory, and one that is not empty gets no dump (found by the end-to-end test, where the recreated subdirectories kept Postgres from starting).
- **incus**: the preseed is applied; instance disks are not restored by moi — a fungible `/var/lib/incus` prints the owner's rebuild note, otherwise a warning says the disk must come from the owner's own pool backup.
- **verify** holds the machine to the *bundle's* state (what ran on the old server), not to a snapshot of the half-built new one.

### Decisions (M8, swarm and Dokploy)

The research spike read Dokploy's installer (`dokploy.com/install.sh`) and its backup documentation. Findings:

- The installer is not a restore tool. It runs `docker swarm leave --force`, removes and recreates `dokploy-network`, generates new secrets, refuses to run when ports 80/443/3000 are taken, and exits 0 even when a service was not created. Running it on a half-rebuilt machine would sever what the earlier phases built.
- Dokploy's own backup is its Postgres database plus `/etc/dokploy`, restored from its web UI — which needs a running Dokploy first. moi already has both: the dump of `dokploy-postgres` and `/etc/dokploy` in the data backup.
- Everything the installer creates is visible to the Docker API afterwards: the swarm, the attachable overlay network, the services `dokploy-postgres` (and `dokploy-redis` on older versions) and `dokploy`, and the standalone `dokploy-traefik` container.

So **moi does not run the installer; it recreates what the snapshot saw**, which is the same for Dokploy as for any other swarm workload: one-node swarm, overlay networks, services from their captured descriptions (image at the captured tag, environment, command and arguments, mounts, networks, published ports, constraints, replicas), `dokploy-postgres` loaded from its dump while its volume is empty, `dokploy-traefik` from its generated script. Dokploy then starts against its own database and `/etc/dokploy` and shows every application as it was.

Limits, each reported as a warning during the restore, never silently skipped:

- **Application images built by Dokploy** exist only on the old server (image repository = service name). Those services are not recreated; they are listed, and the owner redeploys them from Dokploy (one click each: Dokploy rebuilds from git). Services with registry images are recreated directly.
- **Docker secrets and configs**: Docker never returns their values, so neither the snapshot nor the backup holds them. A service that uses them (current Dokploy installs keep the Postgres password and the auth secret this way) is listed with the names to create first; cestmoi's Dokploy predates this and passes them in the environment.
- **A changed address**: git providers and domains configured by IP need Dokploy's "Update IP" (its documentation says so); moi prints nothing about it because it cannot know.

### Guided reboot

The final phase records "verify after next boot", asks the owner to reboot (`moi restore` prints the exact command and does not reboot by itself), and on the next boot the daemon runs `moi verify` after `boot.verify_after`, writes the result to the journal and `/run/motd.d/50-moi`, and pushes `boot.verify_monitor`. The same post-boot verify runs on **every** boot afterwards (replacing cestmoi's `homelab-selfcheck`), so "survives a reboot" is continuously proven, not assumed.

## Acceptance

- [x] Full dry run against the cestmoi fixture bundle lists every action without touching the host.
- [x] VM-based end-to-end test (nightly CI, not per-PR; `tests/e2e/`, `.github/workflows/nightly.yml` — also on PRs that touch the restore): build a server with compose + a Postgres stack + a swarm service + custom unit + firewall rules, back up to a local restic repo, restore onto a fresh VM, reboot, `moi verify` all ✓.
- [x] Moved-disk recognition, FORMAT confirmation, firewall rollback, DB-first import each have tests.
