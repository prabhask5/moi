# 03 — Context for AI assistants (`CONTEXT.md`) and owner notes

`CONTEXT.md` is a single Markdown file describing the server as observed, written so an AI assistant (Claude Code on the server, or an upload to a chat) can make correct changes without breaking anything. It is regenerated after every snapshot and audit. The serverstate prototype's generator was the reference for content and order; moi reproduces it and the improvements listed below.

## Sections, in order

1. Title, generation timestamp, "do not edit — use `moi notes`".
2. **How to use this document (for AI assistants)** — ground truth as of timestamp; check ports, networks/subnets, bind paths, volume names, schedules, firewall, disk space before proposing changes; keep changes reproducible (compose in existing stack dirs, scripts in captured dirs, units in /etc/systemd/system, cron via crontab, **moi.toml for checks/backups/monitors**); after changes run `moi snapshot` and `moi notes`; secrets are redacted.
3. **⚠ Attention** — deduplicated warnings: disks ≥ 85% full, stopped containers that should run, failed units, swarm services below replicas, audit ✗ items, malformed Kuma messages (`[object Object]`), stale copies holding push tokens, push tokens matching no monitor, monitors nothing feeds, SSH password auth enabled, backup/snapshot older than expected, data not covered by backup (excluding fungible paths).
4. **Owner notes** — NOTES.md general sections (Overview, Gotchas, Planned) and any custom headings.
5. **System** — hostname, OS, kernel, hardware, virtualization, CPU, RAM, swap, timezone, uptime, Docker version/root.
6. **Storage** — disks table (device, size, type, SSD/HDD/NVMe, fs, label, mount, model, uuid), fstab, usage, RAID/LVM/ZFS/btrfs, directory structure of data roots (top 2 levels, owners when not root:root, capped), fungible paths with reasons.
7. **Network** — interfaces, routes, DNS, listening ports table (proto, address, port, process).
8. **Domains / reverse-proxy routes** — Traefik labels (`Host()` rules), caddy-docker-proxy labels, Caddyfile sites, nginx `server_name`, cloudflared ingress (hostname → service), Traefik dynamic config files.
9. **Docker** — counts; networks table (name, driver, subnet/gateway, internal, created by compose project / swarm stack / manual, attached containers); named volumes (driver, size, used by containers **and swarm services**); compose stacks (dir, files, env files, services defined vs created, owner notes inline, then each container: image, state, restart policy, ports, networks with static IPs/aliases, mounts, env with secrets redacted, interesting labels, privileged/caps/devices/GPU/healthcheck/user); swarm services (table + notes); standalone containers (+ recreate script path).
10. **VMs & system containers (Incus)** — instances, type, limits, snapshots, notes, where the preseed lives; fungible reason if declared; a devbox block when devbox is present (15 § Context).
11. **Monitoring & alerting** — Kuma monitors table (name, type, target — push tokens masked `…last4`, DNS monitors show hostname, healthchecks.io URLs show host only —, interval, active, group, notifications, fed by, last result), watchdog chain, scripts/containers that report out, and **which moi check owns which monitor** (from `moi.db` bindings).
12. **moi** — version, enabled features, checks with schedules and their monitors, backup schedule/last run/last result/size, bundle last push, heal actions, boot tasks, firewall management.
13. **Firewall & access control** — managers, ufw status, filtered iptables INPUT/DOCKER-USER/custom chains, nftables.conf, fail2ban jails, CrowdSec, ipsets, sshd settings; "not captured: router, Cloudflare, provider firewalls".
14. **Scheduled jobs** — cron table (source, schedule, user, command redacted), run-parts dirs, enabled timers with schedules and what they run.
15. **Services (systemd)** — custom units (state, description, ExecStart/User/WorkingDirectory/OnCalendar, notes), other enabled services, failed units.
16. **Scripts** — path, description (header comment), called by, tags (backup/sync, docker), lines; notes override description.
17. **Installed software** — apt manual, snaps, flatpaks, pipx, npm -g, manual binaries in /usr/local, /opt.
18. **Users & access** — users, uid, home, shell, groups, SSH key comments.
19. **Backups & rebuild** — backup jobs, what is backed up (auto-discovered DB dumps, volumes, paths), fungible paths, coverage table (path, what, ✓ / ✗ / read-only view / fungible), rebuild instructions.
20. **Coverage & reboot audit** — the audit (04), demoted one heading level.
21. **Recent changes** — last 25 commits.
22. **Notes about things no longer present** — owner notes whose headings match nothing current.

Content caps keep the file usable: tables ≤ 200 rows, code blocks ≤ 150 lines, env vars ≤ 40 per container (with "+N"). Target size for cestmoi: < 120 KB.

### Decisions (M3)

- The file is rendered only from what is on disk (snapshot, volatile files, `NOTES.md`, `audit.md`, the configuration): nothing is probed live. It is rewritten after every `moi snapshot`, `moi audit`, `moi notes` run through the daemon, and by `moi context`, which prints `<path> written (N KB, M items need attention)` and the copies (`--json`: `{ "ok": true, "context": { path, bytes, copies, failed_copies, warnings } }`). Without a snapshot, `moi context` takes one first.
- Sections that have nothing to show are left out (no Docker → no Docker section; no routes → no routes section). On cestmoi the order is exactly the list above, with **moi** (12) placed right after Monitoring.
- Section 9 does not list volume sizes (not measured during snapshots, see 02); section 6 lists the fungible paths with their reasons after the directory structure.
- "Swarm services below replicas" counts running task containers (`<service>.<slot>.<id>`) against the replicas the service asks for.
- "Which moi check owns which monitor" is taken from the configured `monitor` names until bindings exist in `moi.db` (06); a push monitor counts as fed when a captured script, a container, or an enabled moi check reports to it.
- "Backup/snapshot older than expected": a snapshot older than two days is flagged; backup age is added with the backup itself (07).
- Dates are UTC. The version shown in section 12 is the running binary's.
- Copies: a copy is written only when its directory exists (a missing directory is reported, never created); it is owned by that directory's owner.
- Claude Code pointer: the block is created at the end of an existing `~/.claude/CLAUDE.md` (or as a new file owned by the user), and replaced in place afterwards; text outside the markers is never changed. `context::remove_pointer` (used by `moi uninstall`) removes exactly the block and deletes the file if nothing else is left.
- Secrets: values are masked where they are printed (rules below) and the finished document is masked once more by value against everything in `secrets.toml`. The fixture file `tests/fixtures/hosts/cestmoi/secrets` lists every secret planted in the fixture; the no-leak test checks the document and its copies against it.

## Redaction (`redact` module)

- Env values whose key matches `PASS|PWD|SECRET|TOKEN|KEY|PRIVATE|CRED|AUTH|SALT|COOKIE|SESSION|SIGNATURE|DSN|WEBHOOK|LICENSE|ACCESS` → `***`.
- In any value or command: `scheme://user:pass@` → `scheme://user:***@`; `password=…`, `token: …`, `api_key=…`, `Bearer …`, `Authorization: …` → `***`; standalone tokens ≥ 40 chars of `[A-Za-z0-9_+=-]` → `***` (never paths: `/` is not in the class).
- Push tokens → `…last4`; healthchecks.io ping URLs → host only.
- A golden test asserts that no secret from the fixture's `secrets` list appears anywhere in CONTEXT.md.

## Owner notes (`NOTES.md`, `moi notes`)

- `## ` headings are stable keys: `Overview`, `Gotchas / do not touch`, `Planned changes / ideas`, plus generated stubs `Stack: <project>`, `Container: <standalone>`, `Service: <swarm service>`, `Instance: <incus>`, `Unit: <custom unit>`, `Script: <path called by cron/systemd>`.
- Stubs are appended for new things with the placeholder `_Not described yet — <hint>_`; nothing the owner wrote is ever modified or deleted. A note whose text is still only the placeholder is ignored. (The placeholder deliberately avoids `TODO`, which the release hygiene check forbids in the codebase.)
- Notes render inline next to the thing they describe; leftovers go to section 22.
- `moi notes` opens `$EDITOR`, then commits and regenerates.

Decisions (M3): `NOTES.md` is created by the first snapshot (so it is part of the first commit) and stubs are appended by every snapshot and by `moi notes`. Headings are matched exactly; a heading the owner wrote twice is theirs and both are kept. Drop-in `.conf` files get no `Unit:` stub (they belong to their unit); `Script:` stubs are only for scripts a cron job or one of the owner's units runs. `moi notes` uses `$VISUAL`, then `$EDITOR` (arguments allowed), then `nano`, then `vi`; with none available it says which file to edit. An editor that exits non-zero records nothing. Before the first snapshot it refuses and says to run `moi snapshot`.

## Copies and Claude Code pointer

`context.copies` are written 0644 owned by the target dir's owner. With `claude_pointer = true`, a block between `<!-- moi:begin -->` and `<!-- moi:end -->` is maintained in `~/.claude/CLAUDE.md` of each copy owner and root, pointing to the copy (root: to the state dir file) and stating the change rules. Uninstall removes exactly that block (and the file if it becomes empty).

## Sharing (`moi context share`)

For pasting into AI chats (claude.ai, etc.) without making anything public:

```
moi context share [--ttl 1h]   # upload CONTEXT.md, print an expiring link
moi context share --revoke     # delete every shared copy now (links stop working immediately)
moi context share --list       # active shares and their expiry
```

- The current `CONTEXT.md` (already redacted) is uploaded to the **private** bucket in `context.share` under a random 128-bit object key (`ctx/<random>.md`), with `Content-Type: text/markdown; charset=utf-8`, `Cache-Control: no-store`.
- moi prints an S3 **presigned GET URL** (SigV4, computed locally — no API call) valid for `--ttl` (default `default_ttl`, capped at `max_ttl`). R2 presigned URLs only work on the account's S3 API domain, not custom domains.
- Expired or revoked objects are deleted by moi (on the next daemon tick after expiry, and by `--revoke`); a bucket lifecycle rule deleting `ctx/` objects after 1 day is created on first use as a safety net. Deleting the object kills the link even before it expires.
- Only `CONTEXT.md` is ever uploaded — never the snapshot, audit internals beyond what CONTEXT.md contains, or secrets.
- Cost: one PutObject per share (+ reads when opened); free-tier R2 handles thousands of shares a month. These operations are counted by the backup R2 guard like any other.
- `notify = true`: a share pushes an informational message (`context shared, expires HH:MM`) so an unexpected share is noticed.

### Decisions (M3)

- Requests are path-style (`<endpoint>/<bucket>/<key>`) and signed with SigV4; the signing region is `context.share.region` (default `auto`, what R2 expects). The signer is checked against the examples in the AWS documentation.
- The object key is `ctx/<32 hex digits>.md` from 16 bytes of the system's random source.
- What moi shared is recorded in `moi.db` (`context_shares`). Expired copies are deleted at the start of every `share` and `--list` (and by the daemon once it exists, M6). `--revoke` does not trust the record: it lists `ctx/` in the bucket and deletes every object there, so links made from a lost state dir die too; nothing outside `ctx/` is touched.
- The lifecycle rule (`moi-ctx-expire`: delete `ctx/` after 1 day) is added on the first share and remembered. A bucket that already has lifecycle rules keeps them untouched (setting a lifecycle configuration replaces the whole set) and moi says so; failing to set the rule is a warning, not an error — moi deletes expired copies itself.
- `--ttl` is a duration (`30m`, `2h`); above `max_ttl` it is refused, and S3's own limit of 7 days applies. `--revoke` and `--list` take no `--ttl`.
- Output: the URL on the first line, then `expires <time> UTC (in <ttl>)`. `--json`: `{ "ok": true, "share": { url, key, expires, ttl, lifecycle } }`, `{ "ok": true, "shares": [{key, created, expires}] }`, `{ "ok": true, "revoked": N }`.
- `notify`: every share and revoke is written to the journal at notice level (`MOI_KEY=<object key>`). The push to the freshness monitor happens when that monitor is bound (06); R2 operation counting with the R2 guard (M7).
- HTTPS uses the system's CA bundle read through the host (`/etc/ssl/certs/ca-certificates.crt` and the usual alternatives); no certificates are compiled in. Redirects are not followed.

## Acceptance

- [x] Golden test: fixture `cestmoi` → `tests/fixtures/expected/cestmoi/CONTEXT.md`.
- [x] No fixture secret leaks (env, URLs, push tokens, hc-ping key, API keys in scripts).
- [x] Notes stubs appended once; owner text preserved byte for byte across 100 regenerations.
- [x] Generation < 200 ms on the fixture (`context_ms` in `scripts/budgets.sh`).
- [x] `context share`: presigned URL verified against a local S3-compatible server in the integration job (Scality CloudServer; MinIO no longer publishes images); TTL above `max_ttl` refused; `--revoke` deletes every `ctx/` object; nothing but CONTEXT.md is uploaded; no secret appears in the uploaded file.
