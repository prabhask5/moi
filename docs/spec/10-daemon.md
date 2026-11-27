# 10 — Daemon, watcher, heal, boot, firewall

`moi daemon` is the only long-running process and the only systemd unit moi installs:

```ini
# /etc/systemd/system/moi.service
[Unit]
Description=moi — server state, health checks, backups, monitors
After=network-online.target docker.service local-fs.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=/usr/bin/moi daemon
Restart=always
RestartSec=5
WatchdogSec=60
# backups and audits lower their own IO priority; checks must run on time

[Install]
WantedBy=multi-user.target
```

## Components

Each is a job or a thread of the one process (see Decisions: no async runtime).

| Task | Behaviour |
|---|---|
| scheduler | interval jobs (checks), calendar jobs (backup at `backup.schedule`, daily snapshot+audit, weekly drill, daily monitor drift report). Calendar jobs are **persistent**: a run missed while the machine was off runs 5 min after start (like systemd `Persistent=true`). Last runs in `moi.db` |
| watcher | Docker events stream (container create/destroy/start/die, network/volume create/destroy, service create/update/remove) + inotify (`/etc` recursive with noise filter, crontab spool, script dirs, compose dirs top-level, `/etc/moi`). Debounce: snapshot when quiet for 120 s, or at most 15 min after the first event. Config change in `/etc/moi` → reload |
| checks | run each check on its interval (05), push results, store history |
| heal | evaluate `[[heal]]` rules on their triggers, run actions, enforce cooldowns, feed reports into the services message |
| boot | at daemon start after boot (not after a restart): wait for `wait_for_mounts` (timeout 5 min → report), apply firewall, `compose up -d` in `compose_up` order (only stacks not already running), schedule post-boot verify |
| firewall | apply/re-apply moi-owned chains (also on Docker `start` events of the daemon itself, since Docker may recreate chains) |
| ipc | `/run/moi.sock` (root only): CLI asks the daemon for state (`moi status`, `moi check` uses the daemon's stored deltas **read-only** so manual runs never disturb sustain counters or CPU deltas), triggers (`snapshot`, `backup`, `push`) |
| notify | `sd_notify(READY=1)`, watchdog pings, `STATUS=` line with the next scheduled jobs |

A slow or hung job never blocks others; each job has a timeout and is cancelled and reported on expiry. Idle budget: < 30 MB RSS, < 0.5 % CPU averaged over an hour on the cestmoi fixture workload (13).

### Decisions (M6, core)

- **Threads, no async runtime.** The loop sleeps on a channel until the next job is due, a job finishes, or the CLI asks for something; each due job runs on its own thread and reports back. Jobs are blocking work that already bounds itself (check timeouts, collector timeouts). This replaces the "tokio tasks" of the first draft: the daemon has nothing to gain from an async runtime, and the binary and its dependency tree stay small (14). A job never runs twice at once; one that is still running when it is next due skips that turn.
- **Interval jobs** (checks) run once at start, then every `interval` counted from when they were *due* — a slow or late run does not push the grid. Turns that passed while nothing could run (a suspended machine) are skipped, not made up for.
- **Calendar jobs** are due at local times of the server's zone, read from its zoneinfo file (`/usr/share/zoneinfo/<name>`, else `/etc/localtime`; daylight saving included; no zone data → UTC). A local time that happens twice (clocks set back) counts once, at its first occurrence; one that never happens (clocks set forward) runs an hour after the hour before it, as systemd does. The syntax is the subset of `OnCalendar` that 01 accepts: optional weekdays (`Mon`, `Mon..Fri`, `Sat,Sun`), optional date (`[year-]month-day`), time (`h:m[:s]`), each field a list of values, ranges (`a..b`) and steps (`/n`), `*` for any; and the shorthands `minutely` … `yearly`.
- **Persistence**: the last run of every job is in `moi.db` (`schedule_runs`). At start a calendar job whose time has passed since its last run is due 5 minutes later; one that has never run waits for its first time.
- **The daily job** runs at 05:00 local time: full snapshot (whatever the watcher saw), audit, context; deletes expired shared links; logs what `moi monitors sync` would change (`monitors: N changes and M differences pending`, never changing anything itself) and the plan's warnings; then pushes `snapshot OK, N changes today` (N = snapshots recorded in the last 24 h; ` (stale: collector, …)` appended when a collector failed) — or `down` with `snapshot failed: …` — to the freshness monitor.
- **A check's scheduled run** is a recorded run (05): result and memory stored, written to the journal under the check's `journal_tag` (info when up, warning when down), pushed to its monitor when one is bound and `monitors.mode` is `live`. A push that fails is logged and does not change the result.
- **IPC**: `/run/moi.sock` (mode 0600), one JSON line each way. `{"cmd":"status"}` → `{running, started, now, version, jobs: [{name, running, next, last: {at, status, message, duration_ms}}]}`; `{"cmd":"run","job":"<name>"}` makes a job due now. Job names: `check:<name>`, `daily`.
- **`moi status`** prints that per job (`up: message` / `running` / `not run yet`, and when it is next due) and exits 1 when a job's last run was `down` or `failed`. Without a daemon it says so, lists the last run of each job from `moi.db`, and exits 1.
- **systemd**: `READY=1` once the socket listens; `WATCHDOG=1` and a `STATUS=` line (`4 jobs, 0 running; next: …`) every time the loop turns, which is at least every half watchdog interval (`WATCHDOG_USEC`) and at least once a minute. Only the loop pings: if it hangs, systemd restarts the service.
- **Stopping**: moi handles no signals. SIGTERM ends the process; every file is written atomically and `moi.db` is transactional, so a job cut short leaves the previous state, and its calendar turn is caught up at the next start.

### Decisions (M6, watcher and heal triggers)

- **What counts as a change.** Engine events: container `create`/`destroy`/`start`/`die`/`rename`/`update`, network and volume `create`/`destroy`/`remove`, service `create`/`update`/`remove`. Not exec, health, attach and the like — moi's own exec calls must not make it snapshot itself. Files: anything in a watched directory except editor and package-tool droppings (`*~`, `.#*`, `.*.swp`, `.*.lock`, `*.dpkg-new|tmp|old`, `*.tmp`, `*.bak`, `4913`, moi's own temp files) and files that churn on their own (`/etc/mtab`, `/etc/adjtime`, `/etc/ld.so.cache`, `/etc/resolv.conf`, `/etc/.pwd.lock`).
- **Watched directories** (inotify, each directory on its own): `/etc` and every directory below it (not through symlinks; `capture.etc_exclude` and `prune_dirs` left out; at most 4096), `/var/spool/cron` and `/var/spool/cron/crontabs`, `capture.script_dirs`, the working directory of every compose project, and the directory of each file a `path_changed` rule names. A directory created below a watched one is watched from then on. The list is made when the daemon starts. Where the kernel has no inotify the daemon says so once and relies on engine events and the daily snapshot.
- **Debounce**: a snapshot is due 120 s after the last change, at the latest 15 min after the first one. Changes that arrive while it is being taken start a new wait. The job is `snapshot` (on demand; `moi status` shows it without a next time until something changes).
- **Container monitors** (06) are brought up to date after such a snapshot: docker monitors for new containers are created, moi's monitors of containers that are gone are paused — nothing else. Only when `monitors.kuma.containers` is on, moi has a login, `monitors.mode` is `live`, and the owner has run `moi monitors sync` at least once (a binding exists): the daemon keeps a set of monitors current, it does not introduce one. Kuma is logged in to only when the local inventory says there is something to do.
- **Configuration**: a change to `moi.toml` or `secrets.toml` is taken at once. Jobs that still exist keep their rhythm and their last result; new ones start; removed ones stop. A configuration that does not load is not taken — the daemon logs why and keeps the one in use.
- **Heal rules on events**: each `docker_event` and `path_changed` rule is an on-demand job `heal:<name>`. It fires on a container event with that name and action, or when exactly that path changes; a rule with a `check` then acts only if the check does not hold (the proxy restarted and is on its network: `nothing to do`). Its report waits in `moi.db` for the next recorded services run, which appends it after the edge entries (where the script's section 4f was) and forgets it; a manual `moi check services` shows waiting reports without taking them. `{"cmd":"run","job":"heal:<name>"}` fires a rule by hand.
- **`compose_up`** runs `docker compose --project-directory <dir> up -d` (2 min): Compose has no API, its CLI plugin is the interface.
- The engine's event stream is read on its own thread and picked up again, from where it stopped, 5 s after it ends.

## Heal rules

```toml
[[heal]]
name = "…"
when = { docker_event = { container = "x", action = "start" } }  # | { path_changed = "/path" } | { failing = ["name", …] } | { interval = "5m" }
check = { container_on_network = { container = "x", network = "n" } }   # optional precondition; action runs only if the check fails
action = { network_connect = {…} } | { restart = [..], wait_healthy = {…}, then_restart = [..] } | { run = "/path" } | { compose_up = "/dir" }
cooldown = "30m"
report = "[…]"                 # appended to the services message when the action ran
report_cooldown = "[…]"        # appended when the action was due but in cooldown
report_failed = "…"            # appended when the action failed
report_stdout = true           # use the action's stdout as the report (e.g. the traefik config guard)
```

Heal rules with a `failing` trigger are evaluated at the end of each `services` run (exactly where service-check.sh section 5 ran); `container_on_network` rules are also evaluated at the start of each services run (section 4a). Every action is logged to the journal with what triggered it (`heal {name}: {why}`, field `MOI_RULE`). The run is recorded in `moi.db` before the action starts, so a slow or failing action still counts for the cooldown. A manual `moi check` never runs an action (05). The rules the services check fires itself are implemented in `heal` since M4 (`network_connect`, `restart`/`wait_healthy`/`then_restart`, `run`); event and path triggers and `compose_up` come with the daemon.

cestmoi's rules (see `examples/cestmoi.toml`) replace: `traefik-homelab.service` (docker event watcher), `traefik-config-guard.path` (path trigger, script kept as the action), and the VPN auto-restart in service-check.sh.

## Firewall

`firewall.manage = true`: moi owns exactly the chains in `firewall.chains` (default `INPUT`, `DOCKER-USER`). Rules files contain chain bodies in `iptables-restore` syntax (`-A INPUT …`); moi flushes and refills only those chains atomically (`iptables-restore --noflush` with a generated table that flushes the owned chains), for v4 and v6. Applied at boot (INPUT immediately, DOCKER-USER once Docker is up) and re-applied when Docker restarts. `moi firewall apply|diff|status`; over SSH, `apply` uses the auto-rollback confirmation. Migration copies `/etc/host-firewall/rules{4,6}.txt` to `/etc/moi/firewall/` unchanged.

## Logging

- Under systemd (`JOURNAL_STREAM` is set, as for `moi.service`) moi writes to the journal with the native protocol (`/run/systemd/journal/socket`): `MESSAGE`, `PRIORITY` (3 error, 4 warning, 6 info, 7 debug), `SYSLOG_IDENTIFIER` (`moi`, or a check's `journal_tag`) and one `MOI_<NAME>` field per structured value (`MOI_CHECK`, `MOI_JOB`, `MOI_RULE`, …), so `journalctl -t moi MOI_JOB=backup` works. If the socket is unavailable the entry goes to stderr instead of being lost.
- Run by hand, moi logs to stderr as `moi: message`, `moi: warning: …`, `moi: error: …` with fields appended as `(key=value, …)`. `--quiet` keeps warnings and errors.
- Every message and field value is masked for known secrets before it leaves the process (14).
- Command results go to stdout; logs never do.

## Acceptance

- [ ] Persistent calendar semantics (missed backup runs after boot), interval jitter-free scheduling, job timeout/cancel.
- [ ] Debounce: a burst of 50 events → one snapshot; continuous events → snapshot at 15 min.
- [ ] Heal: traefik reattach on docker start event; VPN restart sequence with 30-min cooldown and the exact report strings.
- [ ] Firewall: owned chains replaced atomically, other chains untouched (fixture with Docker chains), re-apply on Docker restart.
- [ ] Manual `moi check` while the daemon runs does not change stored deltas.
