# 06 — Monitors: Uptime Kuma & healthchecks.io

moi keeps Uptime Kuma in sync with what runs on the server. **Pillar 2 governs everything here: adopt, never duplicate, never sever.**

## Kuma access

- **Read-only inventory, no credentials:** `kuma.db` (SQLite) located through the Kuma container's `/app/data` mount, opened `mode=ro` (fallback `immutable=1`). Used by snapshot, audit, context and `monitors sync --dry-run`. Tables used: `monitor`, `notification`, `monitor_notification`, `tag`, `monitor_tag`, `docker_host`, `heartbeat` (latest per monitor only). Read columns defensively (`PRAGMA table_info`): schemas differ between 1.23 and 2.x.
- **Changes:** Kuma has no REST API for monitor CRUD; moi uses Kuma's socket.io API (the one the web UI uses) with `monitors.kuma.username/password` (2FA: optional `totp_secret`). Supported versions: 1.23.x (cestmoi runs 1.23.17) and 2.x; detect via the `info` event and adapt payloads. Reference implementations for event names and payload shapes: Kuma's `server/server.js` socket handlers and the MIT-licensed `uptime-kuma-api` Python library; verify every payload against a real Kuma in the CI integration job before relying on it.
  - Connect (socket.io v4, websocket transport) → `login {username, password, token}` → callback `{ok, token}`; then the server emits `monitorList`, `notificationList`, `dockerHostList`, `info`.
  - Used events: `add`, `editMonitor`, `getMonitor`, `pauseMonitor`, `resumeMonitor`, `deleteMonitor` (only for moi-owned monitors with `--prune`), `getTags`/`addTag`/`addMonitorTag`, `addDockerHost`.
- **Pushes:** plain HTTP GET to `/api/push/<token>` (05 § Push) — no login needed.

## Desired monitors

| Source | Monitor | Type | Defaults |
|---|---|---|---|
| each enabled check with `monitor` | the configured name | push | interval = check interval + 20% (5 m → 360 s, as cestmoi), retries 1 |
| backup | `checks.backup.monitor` | push | interval = 25 h (90000 s) |
| freshness (`freshness = true`) | `<server>: snapshot` | push | pushed after each daily snapshot+audit; interval 26 h; message `snapshot OK, N changes today` |
| boot verify (`boot.verify_monitor`) | configured name | push | pushed once per boot; interval 0 (no heartbeat expectation) → use `upside_down=false`, long interval 30 d |
| containers (`containers = true`) | `<server>: <container>` | docker | one per non-swarm container with a restart policy; docker host = `monitors.kuma.docker_host` (created as socket `/var/run/docker.sock` if missing and the Kuma container mounts the socket); interval 60 s |

Created monitors get the existing notifications listed in `monitors.kuma.notifications` (matched by name), are put in `group` if set, and are tagged `moi` (tag created if missing) so ownership is visible in Kuma.

## Adoption algorithm (`moi monitors sync`)

For each desired monitor, in order, the first rule that matches wins:
1. **Binding**: `moi.db` already binds this check to a Kuma monitor id that still exists → keep.
2. **Migration map**: the legacy feeder being replaced (11) pushes to token T and a monitor with push token T exists → adopt it, store `(id, T)`.
3. **Exact name + compatible type** → adopt (push token read from the monitor).
4. **Docker monitor** for the same container name on any docker host → adopt.
5. Otherwise → **create**.

Adopting never changes a monitor's name, interval, notifications, group, history or token. Differences from moi's defaults are reported as `drift` and only applied with `--update` (still never the token). Name match with an incompatible type → `conflict`, nothing done.

**Orphans**: moi-owned monitors (tag `moi` and recorded in `moi.db`) whose target no longer exists are **paused**, never deleted, unless `--prune`. Monitors moi does not own are never paused, edited or deleted.

`--dry-run` (and the daemon's daily drift check) prints a plan:

```
ACTION   MONITOR                      TYPE    DETAIL
adopt    Server Resources             push    token …0Ov3 (fed by /opt/scripts/resource-check.sh — migration map)
adopt    Server Storage               push    token …JQIB
create   cestmoi: dozzle              docker  host local
keep     Server Watchdog: Healthchecks.io  http  not managed by moi (watchdog)
drift    Server Backup                push    interval 90000 (moi default 90000) — ok
```

The daemon runs sync automatically only for **creating container monitors** when containers appear (debounced, `containers = true`), and pauses moi-owned container monitors when containers are removed. Everything else happens only on explicit `moi monitors sync`.

## healthchecks.io

cestmoi's watchdog is a Kuma HTTP monitor that requests `https://hc-ping.com/<key>/…` every 60 s, so healthchecks.io alerts when Kuma or the server stops. moi:
- detects such monitors (URL host `hc-ping.com` or `*healthchecks*`), lists them as the watchdog chain in context, never modifies them, and never prints the key;
- warns if `expect_watchdog = true` and no watchdog exists;
- after a restore, verifies the watchdog monitor is active and its last heartbeat is recent.

## Acceptance

- [ ] Integration test against `louislam/uptime-kuma:1.23.17` in CI: login, list, create push/docker monitors, tag, pause; second sync is a no-op; monitors created by a human are untouched.
- [ ] Adoption by token, by name, by docker container; conflict on type mismatch; orphan pause; `--prune` deletes only moi-owned.
- [ ] `--dry-run` works with only read access to `kuma.db` (no credentials).
- [ ] Push sends exactly one `status` and one `msg` (regression test for the `[object Object]` bug).
