# 02 — Snapshot & history

`moi snapshot` captures the complete state of the server into `/var/lib/moi/state/` and commits the stable part to a local git history. The daemon snapshots after changes (10 § Watcher), daily, and before every backup. It grew out of a working prototype, serverstate (bash + Python); moi keeps its layout and semantics, implemented natively and in parallel.

## Layout

```
state/
  snapshot/                 # STABLE: committed; changes here are real configuration changes
    meta/                   hostname, os-release, kernel, cmdline, cpu (no MHz), memory totals, dmi, timezone, virt, locale
    packages/               family, apt-manual.txt, apt-hold.txt, snap/flatpak/pipx/npm-global/cargo lists, usr-local-bin.tsv, opt-listing
    docker/                 version, compose-version, root-dir, containers.json, networks.json, volumes.json,
                            services.json (swarm), stacks.txt, swarm-*.txt, compose-projects.json, standalone/<name>.sh
    systemd/                enabled-units.txt, unit-file-states.txt, custom-units.txt, triggered-by.txt
    cron/                   user-<name>.crontab, enabled-timers.txt
    firewall/               iptables.rules / ip6tables.rules (filtered), managers.txt, ufw-*, fail2ban-jails, crowdsec-*, ipset-names
    network/                addresses, links, routes, dns (no veth/br- noise), tailscale-ip, wireguard-interfaces
    storage/                lsblk.json, mounts.txt, blkid, mdadm, lvm, zfs, btrfs, data-roots.txt
    users/                  users.passwd (uid 0 and 1000–59999), groups.group
    scripts/                index.txt (real scripts only), referenced.txt
    incus/                  preseed.yaml, instances/<name>.yaml (volatile.* stripped), instances/<name>.snapshots
    monitoring/             kuma-monitors.json, integrations.json
    files/                  verbatim copies, absolute paths mirrored (etc/, opt/…, home/…), size-limited
    files.manifest.tsv      mode, owner, group, path, size, sha256 of every captured file (and of skipped large files)
  volatile/                 # NOT committed: changes constantly
    captured-at, uptime, free, df, apt-all.tsv, docker-ps.tsv, docker-images.tsv, volume sizes, service-ls.tsv,
    listening.txt, iptables.raw, nftables.ruleset, smart, mdstat, zpool-status, running/failed units, timers schedule,
    trees/<root>.tsv (mode uid gid path of every dir to tree_depth), changelog.txt
  NOTES.md                  # committed; owner's notes (03)
  CONTEXT.md, audit.md      # generated, git-ignored
```

The split matters: only `snapshot/` and `NOTES.md` are committed, so the history contains real changes and nothing that changes on its own (uptime, counters, container IPs, ban lists, timers' next-run, directory trees of media).

## moi.db

`<state.dir>/moi.db` (0600, next to `state/`, never committed) is SQLite and holds what must survive restarts but is not server state: check results and sustained counters (05), previous-run values for deltas, monitor bindings and push tokens (06), heal cooldowns and schedule last-runs (10), and from M2 the capture cache below.

- The schema version is SQLite's `user_version`. Opening the database applies every migration newer than it, in order, each in its own transaction, and records it in `schema_history (version, name, applied_at)`. A migration that fails leaves nothing behind and names itself in the error.
- Migrations are forward-only and append-only: a shipped migration is never edited; a schema change is a new one.
- A database with a higher version than the binary knows is refused untouched (`upgrade moi`): downgrading moi never rewrites state.
- `state.dir`, `state/` and `archive/` are created 0700; a state root that already exists more open is tightened.

## Collectors

One collector per area, run concurrently, each with its own timeout (default 60 s, `du`-like work never runs here). A failing collector logs a warning and the snapshot continues; its previous output is kept (not deleted) and marked stale in `volatile/collector-errors.json`.

| Area | Source (via `host::Host` / `docker` / `systemd`) | Normalization |
|---|---|---|
| meta | `/etc/os-release`, `/proc/cmdline`, `/proc/cpuinfo`, `/proc/meminfo`, `/sys/class/dmi/id/*` | drop MHz/bogomips |
| packages | dpkg status file parsed directly (`/var/lib/dpkg/status`), `apt-mark showmanual` equivalent from `/var/lib/apt/extended_states`; rpm via `rpm -qa` fallback; snap/flatpak/pipx/npm by reading their state dirs (never run commands that write logs) | sorted |
| docker | Engine API: containers (all), networks, volumes, services, nodes, secrets/configs (names only), compose projects | see § Docker normalization |
| systemd | D-Bus `ListUnitFiles`, `ListUnits`, unit properties incl. `TriggeredBy`; custom units = regular files in `/etc/systemd/system` (+ drop-in dirs) excluding `snap-*`, `snap.*` | |
| cron | `/var/spool/cron/crontabs/*` (+ `/var/spool/cron/*` RHEL), `/etc/crontab`, `/etc/cron.*` | |
| etc | whole `/etc` minus `capture.etc_exclude`, files ≤ `max_file` | |
| firewall | `iptables-save`, `ip6tables-save`, `nft list ruleset`, ufw/fail2ban/crowdsec state | stable copy: drop counters, Docker-generated chains (keep `DOCKER-USER`), `docker0`/`br-*` rules, fail2ban ban entries |
| network | netlink (`rtnetlink`) or `/proc/net/*`, `/sys/class/net` | drop veth/br-*/docker0 |
| storage | `/sys/block`, `/proc/self/mountinfo`, `blkid` cache, LVM/mdadm/zfs/btrfs via their tools if present | |
| trees | walk each data root to `tree_depth`, one filesystem, skip lost+found/.Trash/@eaDir/.snapshots/node_modules/.git | volatile |
| users | `/etc/passwd`, `/etc/group` (shadow lives in files/etc) | |
| scripts | `capture.script_dirs` (light prune), **files referenced by cron/systemd units and, recursively (depth 2), by those scripts**; index = files with `#!` excluding pip/console-script stubs and dpkg-owned files | |
| home | `capture.home_files` for uid 0 and 1000–59999 | |
| incus | `incus admin init --dump`, `incus config show <i>`, snapshot names (if `incus` exists and responds) | strip `volatile.*` |
| monitoring | `kuma.db` read-only (located via the Kuma container's `/app/data` mount; open `mode=ro`, fall back to `immutable=1`), integrations scan (§ below) | push tokens kept in stable files (state dir is root-only) |

### Decisions (M2)

**Engine**
- Collectors only describe what to store (generated files as bytes, host files as paths); the engine does all writing. A file is rewritten only when its content differs, so a no-op snapshot writes nothing.
- Every file under `snapshot/` and `volatile/` has a row in `moi.db` (`snapshot_files`) naming the collector that owns it. After a run, files no collector produced are removed (and empty directories with them) — except those of a collector that failed, panicked or timed out: its rows and files are kept and it is listed in `volatile/collector-errors.json` as `[{"collector": "…", "error": "…", "stale": true}]` (the file is absent when everything worked). A failure is never a change.
- When two collectors name the same file, the first in collector order owns it.
- Collectors run on one thread each and are joined with a shared deadline (default 60 s). A collector still running at the deadline is abandoned, not killed: it only reads, and its late result is dropped.
- Copies under `snapshot/files/` are written 0600 whatever the original's mode; modes, owners and groups live in `files.manifest.tsv` (`mode owner group path size sha256`, tab-separated, sorted by path). Symlinks are copied as symlinks, with `-> target` in the hash column. A file over `capture.max_file` is not copied but listed with its hash if it is at most 64 MB, else with `-`.
- The capture cache is the same table: a host file whose size, mtime, inode, mode and owner are unchanged is neither read nor rewritten.

**Sources** (where this file's table says otherwise, this list wins until the noted milestone)
- `systemd`: read with `systemctl` (read-only verbs, one `show` call for all running services). The D-Bus client replaces these calls in M6, when the daemon needs it anyway; the files written do not change.
- `network`: `ip -br addr|link`, `ip route`, `resolvectl dns` (fallback `/etc/resolv.conf`), `ss -tulpnH`; no netlink dependency.
- `storage`: mounts from `/proc/self/mountinfo`; `lsblk -J`, `blkid`, LVM/mdadm/zfs/btrfs/`smartctl` when installed.
- `meta`: `kernel` is `ostype osrelease arch` from `/proc/sys/kernel`; `cpu` is model, sockets, cores per socket and threads from `/proc/cpuinfo`; `locale` from `/etc/default/locale` or `/etc/locale.conf`; `virt` from `systemd-detect-virt` (no file on bare metal).
- `packages`: dpkg status and apt's `extended_states` parsed directly (`apt-manual.txt` = installed and not auto-installed, `apt-hold.txt` = held); snap = directories in `/snap`; flatpak = `/var/lib/flatpak/app`; pipx = venv directories; npm = global `node_modules` directories; cargo = `.crates.toml`. No `pip3 freeze` (it would mean running pip). `usr-local-bin.tsv` describes each entry as ELF executable, script (with its interpreter), symlink or data.
- `scripts`: the "package-owned" exclusion checks dpkg file lists only for paths under `/opt` and `/usr` (nothing else a script dir holds can be package-owned).
- `users` also captures `capture.home_files` (the table's `home` row). User crontabs are both copied and listed as `cron/user-<name>.crontab`.
- Data roots with `capture.data_roots = "auto"`: every mounted data filesystem that is not `/`, `/boot*`, `/var`, `/var/log`, `/tmp`, `/usr*`, `/run*`, `/snap*` or under `/var/lib/docker`, plus `/srv`, `/opt`, `/data`, `/home`, `/mnt`, `/media` when they exist.
- Volatile additions: `loadavg`; `uptime` and `free` are the raw `/proc` values.

**Docker, Incus, monitoring**
- A host without a Docker socket simply has no `docker/` state (compose files found on disk are still listed and captured). A socket whose engine does not answer is a *failed collector*: the previous Docker state is kept and reported stale, never replaced by "no containers". The same rule applies to an installed `incus` whose daemon does not answer.
- `docker/compose-version` is the highest `com.docker.compose.version` label among the containers (no compose CLI is run). `docker/stacks.txt` lists stack namespaces from service labels with their service count. `volatile/docker-ps.tsv` is `name, state, started-at, image`; `docker-images.tsv` is `tags, id, size`; `service-ls.tsv` is `name, replicas, image, ports`.
- Volume sizes are not measured during a snapshot (that is `du`-like work); the audit (04) measures what it needs.
- Compose projects: container labels cover what `docker compose ls -a` would add (stopped containers carry the same labels). `compose-projects.json` entries have `project, workdir, config_files, env_files, services, defined_services, containers, source` (`containers` or `found on disk (no containers)`). Each project's working directory is captured four levels deep without `capture.prune_dirs` and data files (`*.db`, `*.sqlite`, `*.log`, media, …). A directory that a container mounts as its database data directory (`/var/lib/postgresql`, `/var/lib/mysql`, `/var/lib/mariadb`) is never entered, whatever it is called: its files are the database's, kept by the dump.
- `docker/standalone/<name>.sh` is generated for every container without a compose project label (swarm tasks excluded). It is never executed by moi.
- The monitoring collector runs in a second round, after the others, because it scans the files they captured and the container list. If the Docker collector failed in this run it fails too (previous output kept).
- `monitoring/kuma-monitors.json` is `{ "db": path, "monitors": [...] }` with the columns of Kuma's `monitor` table that describe a monitor, plus `notifications` (names only — never their configuration), `tags` and `group`. The last heartbeat of each monitor changes every minute, so it lives in `volatile/kuma-last.json` (`name → {status, msg, time}`), not in the stable file.
- `monitoring/integrations.json` entries are `{file, kind, base, token, called_by[]}` (+ `"stale_copy": true` for `*.bak`, `*.old`, `*.orig`, `*~`) or `{container, env, kind, base, token}`; `kind` is `uptime-kuma-push` or `healthchecks`. Only files up to 512 KB are scanned.

**`moi snapshot`** prints `snapshot saved to <state>: N files, M files changed` (or `no configuration changes`), warns per failed collector, and with `--json` prints `{ "ok": true, "state": "…", "snapshot": { "collectors": [{name, error, duration_ms}], "changed": […], "stable_files": N, "files_read": N } }`. With `--push` the state bundle is uploaded afterwards if it changed (08): a last line `state bundle saved off-site` or `state bundle: nothing changed since the last push`, and `"pushed": true|false` in the JSON; without an off-site repository configured that is an error.

### Docker client

The Engine API is spoken directly over `/var/run/docker.sock` with unversioned paths (the engine's own current API version), one request per connection. Payloads are kept as JSON values and only the fields listed below are read, so an engine that adds or drops fields breaks nothing. An unreachable engine is "docker not available" (compose files on disk are still captured), never a failed snapshot. Reading state sends only `GET` requests; the only `POST`s moi ever sends while capturing or checking are `exec` create/start.

### Docker normalization (stable fields only)

- Containers: name, image, image id, cmd, entrypoint, env (sorted), labels, user, hostname (dropped if it equals the short id), healthcheck, exposed ports, HostConfig subset (port bindings, restart policy, network mode, privileged, caps, devices, device requests, extra hosts, dns, log config, resources, tmpfs, sysctls, security opts, pid/ipc mode, init, ulimits, runtime), mounts (anonymous volumes reduced to `anonymous: true`), networks (aliases without ids, static IPs). **Swarm task containers are excluded** (their names change every deploy; the service represents them).
- `network_mode: container:<id>` is rewritten to `container:<name>` (the id changes on recreate).
- Networks: name, id, driver, scope, internal, attachable, ipv6, IPAM subnets/gateways/ranges, options, labels.
- Volumes: named only (anonymous excluded), driver, mountpoint, labels, options.
- Services: name, image (digest split off), replicas/global, env, labels, container labels, mounts, published ports, networks (ids → names), constraints, restart condition, secrets/configs names.
- Compose projects: from container labels (`com.docker.compose.project*`), the compose CLI's project list equivalent (labels on stopped containers), and compose files found under `capture.compose_search` (depth 4, light prune). Portainer stacks map `/data/compose/<id>` to the Portainer volume's host path. For each project: workdir, config files, env files, services defined (parsed YAML), containers.
- Standalone containers (no compose project, not swarm): a reviewed-by-human `docker run` script is generated in `docker/standalone/`.

### Integrations scan

Every captured text file (scripts, units, crontabs, compose files and env files) is scanned for Uptime Kuma push URLs (`/api/push/<token>`) and healthchecks.io ping URLs; each hit records file, kind, base URL, token, and the cron line/unit that runs that file. Container env vars are scanned too (e.g. `UPTIMEKUMA=http://…/api/push/<token>`). Files ending `.bak/.old/.orig/~` are marked stale copies.

### devbox

When devbox is present, the `devbox` collector adds its config, routing file, gateway app description and status (secrets never) — 15 § Snapshot.

### ssh

With `[ssh]`, the `ssh` collector records the tunnel connector (unit, version, metrics address), its ingress rules, the DNS record, key `sshd -T` settings, and the authorized keys' fingerprints and comments, never the keys themselves — 16 § Records.

## History

- The state dir is a git repository (gitoxide; no `git` binary needed). One commit per snapshot that changed `snapshot/` or `NOTES.md`; no-op snapshots create no commit.
- Commit subject: `changed: <areas>, <up to 4 file paths> +N files`; body: docker diff (`+ container x (image)`, `- container y`, `~ container z: env, mounts`), swarm service diff, then `name-status` lines (max 60).
- `moi log [-n N]`, `moi diff [rev]` (default HEAD; secrets in the diff are shown only with `--show-secrets` as root).
- `volatile/changelog.txt` = last 25 commits for the context file.

### Decisions (M2)

- Trees are built directly from the files of `snapshot/`, `NOTES.md` and `.gitignore` (`volatile/`, `CONTEXT.md`, `audit.md`); no index is written. `git log`, `git show` and `git fsck` work on the repository (a test checks this when `git` is installed), but `git status` in the state dir is not meaningful and nobody should commit there by hand.
- The repository is opened isolated: no system or user git configuration is read. Branch `main`, author and committer `moi <moi@<hostname>>`, commit time = the host clock.
- First commit: `initial snapshot of <hostname>`. Later subjects list the areas (the directory under `snapshot/`, or the top-level file such as `NOTES.md`) in alphabetical order, then up to four captured files by their host path, then ` +N files`. `files.manifest.tsv` is never named: it changes whenever a captured file does.
- Body: `+ container x (image)`, `- container y (image)`, `~ container z: env, mounts` (the changed top-level fields of the normalized record), sorted by name, then the same for `service`; then `A|M|D<tab>path` lines (captured files by host path), at most 60, then `… and N more`.
- `moi log [-n N]` prints `YYYY-MM-DD HH:MM  subject`, newest first, times in UTC. `--json`: `{ "ok": true, "commits": [{id, time, subject, body}] }`.
- `moi diff [rev]` prints the commit, its `A|M|D` list and a unified diff (3 context lines) of every changed text file except the manifest; binary files are named only. `rev` is `HEAD`, `HEAD~N` or an id prefix. Secrets are masked by shape (assignments to sensitive names, `name: value` lines, URL credentials, push/ping tokens, password hashes) and by value (everything in `secrets.toml`). `--show-secrets` shows them and is refused unless the caller is root. `--json`: `{ "ok": true, "commit": {…}, "files": [{status, path, diff}], "secrets_shown": false }`.
- Before the first snapshot, `log` and `diff` say `no history yet: run \`moi snapshot\`` and exit 0 without creating anything.
- `moi snapshot` now ends with `recorded <short id>` and the subject, or `no configuration changes`; its JSON gains `"commit": {…} | null`.

## Performance

Incremental: keep a cache of `(path, size, mtime, inode) → sha256` in `moi.db`; unchanged files are not re-read or re-copied. Budget: full snapshot of the `cestmoi` fixture < 2 s cold, < 500 ms warm (13).

## Acceptance

- [x] Fixture host `cestmoi` → snapshot identical to `tests/fixtures/expected/cestmoi/snapshot/` (every generated file byte for byte; the verbatim copies under `files/` are pinned by the manifest's hashes). The fixture and its golden tree are built from a redacted profile of the server and its redacted legacy scripts, not from a dump of the machine: the first run on the real cestmoi should be diffed against the prototype's output there.
- [x] Two snapshots with no change → one commit total.
- [x] Changing a container's env, adding a cron line, editing a file in /etc, creating a swarm service → one commit each, subject/body as specified.
- [x] Swarm task containers never appear in containers.json; `container:<id>` network modes are rewritten to names.
- [x] A collector that times out does not delete its previous files and is reported.
- [x] Running `pipx`/`npm`/etc. is never needed (no side effects on the host: assert no files written outside the state dir).
