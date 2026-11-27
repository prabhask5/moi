# 02 — Snapshot & history

`moi snapshot` captures the complete state of the server into `/var/lib/moi/state/` and commits the stable part to a local git history. The daemon snapshots after changes (10 § Watcher), daily, and before every backup. The working prototype is `reference/serverstate/` (bash + Python); moi keeps its layout and semantics, implemented natively and in parallel.

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

## History

- The state dir is a git repository (gitoxide; no `git` binary needed). One commit per snapshot that changed `snapshot/` or `NOTES.md`; no-op snapshots create no commit.
- Commit subject: `changed: <areas>, <up to 4 file paths> +N files`; body: docker diff (`+ container x (image)`, `- container y`, `~ container z: env, mounts`), swarm service diff, then `name-status` lines (max 60).
- `moi log [-n N]`, `moi diff [rev]` (default HEAD; secrets in the diff are shown only with `--show-secrets` as root).
- `volatile/changelog.txt` = last 25 commits for the context file.

## Performance

Incremental: keep a cache of `(path, size, mtime, inode) → sha256` in `moi.db`; unchanged files are not re-read or re-copied. Budget: full snapshot of the `cestmoi` fixture < 2 s cold, < 500 ms warm (13).

## Acceptance

- [ ] Fixture host `cestmoi` → snapshot layout identical (modulo ordering) to `tests/fixtures/expected/cestmoi/snapshot/` (generated from the prototype's real output, redacted).
- [ ] Two snapshots with no change → one commit total.
- [ ] Changing a container's env, adding a cron line, editing a file in /etc, creating a swarm service → one commit each, subject/body as specified.
- [ ] Swarm task containers never appear in containers.json; `container:<id>` network modes are rewritten to names.
- [ ] A collector that times out does not delete its previous files and is reported.
- [ ] Running `pipx`/`npm`/etc. is never needed (no side effects on the host: assert no files written outside the state dir).
