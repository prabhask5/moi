# 04 — Audit

`moi audit` answers "what would I lose or forget?". It writes `state/audit.md`, feeds the ⚠ list in CONTEXT.md, and is run by the daemon daily (after the snapshot) and after `moi backup`. Read-only. Reference: `reference/serverstate/sshelper.py` (`audit`) and `reference/cestmoi/scripts/disk-audit.sh`.

Findings are `✗` (gap), `review` (decide), `✓` (verified OK, shown for reboot items so the owner sees *why* it is fine), or `fungible` (declared, shown once, never a gap).

## Sections

### 1. Would it survive a reboot?
- Running container with restart policy `no`/empty → ✗ (unless brought up by `boot.compose_up` or a boot unit that runs `compose up`; then ✓ with the reason).
- Running service whose unit file state is `disabled` → ✗ **unless** it has `TriggeredBy` (socket/timer/path activation — e.g. `ssh.service` via `ssh.socket` on Ubuntu ≥ 24.04) or is pulled in by an enabled unit's `Wants/Requires`.
- Mounted real filesystem not in fstab → ✗; loop-device mounts owned by an enabled service (incus storage pool via `incus-startup.service`, snaps) → ✓ with the owner; unknown loop mounts → review.
- Live custom iptables rules (INPUT, FORWARD, DOCKER-USER, custom chains; Docker/ufw/fail2ban internals excluded) vs persistence: `/etc/iptables/rules.v4` diff, ufw/nftables/firewalld active, an enabled custom unit whose ExecStart (or the script it runs) calls `iptables`/`ip6tables`/`nft`, or `firewall.manage = true` → ✓ naming the mechanism; otherwise ✗ with the rules that would be lost.
- `docker.service` not enabled → ✗. Docker drop-ins (`/etc/systemd/system/docker.service.d/*`) listed with their purpose.
- Swarm: node will rejoin on boot (single-node manager) → ✓; Dokploy services need Dokploy Postgres first → noted.

### 2. Large data not covered by the backup
`du`-style sizing (low IO priority, `nice 19`, `ionice idle`, one filesystem, depth 2) of `/opt /srv /var/lib /root /home` + data roots. Leaf directories ≥ 200 MB not under backup coverage (07: auto-discovered + `backup.paths`) are listed with a note: `fungible (reason)`, `regenerable (images/packages/cache)` for container runtime dirs, `local backup copies` for the backup dir itself, else `✗ not backed up`.

### 3. Docker data a rebuild could lose
- Bind mounts and named volumes (containers and swarm services) not covered → ✗, except read-only views (`ro` mounts of directories owned elsewhere), host system files (`/etc/machine-id`, `/var/log`, `/etc/localtime`, docker.sock…), and fungible paths.
- Database data dirs → `review — covered by dumps` when `backup.databases` dumps that container, else ✗.
- Container writable layer > 100 MB (`SizeRw`) → ✗ "lost when recreated".
- Images without a registry digest → review; for Dokploy-built images (labels/naming from Dokploy) the note says Dokploy rebuilds them from git.

### 4. Configuration a restore would not put back
- `/etc` files read by custom units/scripts/crontabs → listed as *restored automatically* (09 restores them).
- Custom files (no package owns them; dpkg `.list` files / rpm db) changed after OS install (mtime > install time = min(mtime of `/var/log/installer`, `/etc/machine-id`) + 1 day), excluding generated files (apparmor local stubs, console-setup caches, xml catalogs, lvm archives, pam common-*, multipath bindings, `*.ucf-*`, `*.old`, `*.bak` noted separately as stale).
- Package conffiles whose md5 differs from dpkg's (e.g. `/etc/systemd/resolved.conf` changed to free port 53 for AdGuard). **Policy: restore puts back all modified conffiles by default (showing a diff), unless excluded.**

### 5. Application state outside the usual places
Non-package dirs in `/var/lib`, `/opt`, `/srv` (> 64 KB or captured), with size, whether config is captured, and backup coverage.

### 6. Storage placement (port of `disk-audit.sh`)
For each running container mount, the backing device class (SSD/HDD/RAM via the mount's source device `rotational` flag): flag *database/config on HDD* (paths matching `postgres|pgdata|postgresql|redis|/config|/db(/|$)|database|sqlite|/app/data`), *bulk data on SSD* (`/data(/|$)|media|movies|/tv(/|$)|music|books|download|torrent|usenet|complete|library|backup`), *more than one bulk mount in one container* (hardlinks across mounts fail), *> 1 GB written inside the container*. Plus the Docker data root's device class and the biggest SSD users. Keyword lists are configurable (`[audit.placement] hot = […] bulk = […]`).

### 7. Secrets written into scripts
Assignments of `*KEY*|*TOKEN*|*PASS*|*SECRET*` with literal values ≥ 16 chars in captured scripts → review "move to secrets.toml" (moi itself reads `secret:` refs, so migrated checks no longer need them).

### 8. Programs listening on the host outside Docker
Listener processes other than docker-proxy/sshd/systemd/resolved/chronyd → "make sure the rebuild recreates it" with the package/unit/container that provides it when known (AdGuard Home in a host-network container is attributed to its container).

### 9. Monitoring integrity
Kuma monitors with no feeder, feeders pushing to unknown tokens, stale copies with tokens, malformed last messages, moi checks without a monitor binding.

## Decisions (M3)

- **Coverage is judged against what `moi backup` covers** (the identity set of 07, computed from the snapshot and `[backup]`), not against a hand-kept list. With `volumes = "auto"` and `databases = "auto"` every named volume, read-write bind mount and stack directory is covered, so on cestmoi the Dokploy volumes are *not* gaps; they are gaps when `[backup]` is absent or when explicit `volumes`/`databases` lists leave them out. A database's raw data directory is `review — covered by the dump` when it is dumped and ✗ when it is not.
- Findings are data: `{ id, title, blocks }` per section, a block being a finding (`gap | review | ok | fungible | info`), text, code or a table. `audit.md`, the context file and `--json` render the same structure. Section ids: `reboot`, `large`, `docker`, `config`, `state`, `placement`, `secrets`, `listeners`, `monitoring`.
- `moi audit` prints the ✗ and `review` lines per section (`--quiet`: ✗ only), then `N gap(s), M to review — full report: <path>`; exit 1 when there is at least one gap, 0 otherwise. `--json`: `{ "ok": true, "gaps": N, "file": "…", "audit": {…} }` (same exit codes). It takes a snapshot first if none exists.
- Sizing runs only as `nice -n 19 ionice -c3 du -x -B1 -d 2 <root>`; if that cannot be started the sizes are skipped and the section says so — `du` never runs at normal priority. The roots are `/opt /srv /var/lib /root /home` plus the data roots.
- § 1: a container without restart policy is ✓ when its stack directory matches `boot.compose_up`, or when an enabled unit of the owner runs `compose … up` and names that directory (or loops over its parent, `…/*`). Firewall persistence is ✓ by `firewall.manage`, by a rules file that contains every live custom rule, by an active manager, or by an enabled unit of the owner whose text (or the script its `Exec*` lines run) calls `iptables`/`ip6tables`/`nft`.
- § 2: a fungible root is listed once with its reason; `cache`/`.cache` directories and container runtime directories are "regenerable"; dated directories under `backup(s)/` are "local backup copies". `/var/lib/docker/volumes` (one number from `du`) is judged by the volumes in it.
- § 4: "restored" means under `restore.etc_paths`, handled by another restore phase (cron, units, apt, users, fstab, hostname, netplan, `/etc/moi`), or read by the owner's units/scripts. A modified conffile is ✓ when restored or when `restore_modified_conffiles = true`, else ✗. The OS install time is the older of `/var/log/installer` and `/etc/machine-id`.
- § 6 keywords: plain keywords are case-insensitive substrings of `source destination`; one ending in `$` must be followed by `/` or end that text (the script's `($|/)`). Bulk is tested before hot, as in the script. Swarm services are placed too (their task containers are what the script saw). "Biggest SSD users" is not reproduced: it needs a `du` per mount, and § 2 already sizes the roots.
- § 8: a listener is attributed to a host-network container whose name or image matches the process, else to a unit of the owner that mentions it.
- § 9 uses Kuma's monitor list and the integrations scan; "moi checks without a monitor binding" compares configured monitor names with Kuma's list until bindings exist (06).
- `examples/cestmoi.toml` gained `/opt/scripts` in `backup.paths`: the audit showed it was missing although the legacy `backup.sh` archives it.

## Acceptance

- [x] On the `cestmoi` fixture: no false positives for socket-activated ssh, host-firewall.service, incus loop mount, ro/log binds, fungible media/incus; true positives for Dokploy volumes (when the backup does not cover them, see Decisions), supabase-pooler writable layer, resolved.conf, backup.sh.bak.
- [x] Placement section reproduces `disk-audit.sh` findings on the fixture (derived from the script's rules: the script itself cannot run against a fixture).
- [x] Sizing never runs at normal IO priority; whole audit < 10 s on the fixture excluding sizing (measured in `scripts/budgets.sh`).
