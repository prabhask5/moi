# 03 — Context for AI assistants (`CONTEXT.md`) and owner notes

`CONTEXT.md` is a single Markdown file describing the server as observed, written so an AI assistant (Claude Code on the server, or an upload to a chat) can make correct changes without breaking anything. It is regenerated after every snapshot and audit. The prototype's generator (`reference/serverstate/sshelper.py`, function `context`) is the reference for content and order; moi reproduces it and the improvements listed below.

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
10. **VMs & system containers (Incus)** — instances, type, limits, snapshots, notes, where the preseed lives; fungible reason if declared.
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

## Acceptance

- [ ] Golden test: fixture `cestmoi` → `tests/fixtures/expected/cestmoi/CONTEXT.md`.
- [ ] No fixture secret leaks (env, URLs, push tokens, hc-ping key, API keys in scripts).
- [ ] Notes stubs appended once; owner text preserved byte for byte across 100 regenerations.
- [ ] Generation < 200 ms on the fixture.
- [ ] `context share`: presigned URL verified against a local S3-compatible server in the integration job (MinIO); TTL above `max_ttl` refused; `--revoke` deletes every `ctx/` object; nothing but CONTEXT.md is uploaded; no secret appears in the uploaded file.
