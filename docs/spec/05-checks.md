# 05 — Health checks

Checks run inside the daemon on their interval, and on demand with `moi check [name]`. Each check produces a **status** (`up`/`down`) and a **message**, pushed to its Uptime Kuma push monitor (06) and recorded in `moi.db` (last N results). Checks must work on any server; cestmoi's legacy scripts define the exact output format.

## Parity contract

The built-ins replace these scripts on cestmoi, which today feed Kuma monitors with years-old history:

| Built-in | Replaces | Kuma monitor (cestmoi) |
|---|---|---|
| `resources` | `reference/cestmoi/scripts/resource-check.sh` | Server Resources |
| `storage` | `reference/cestmoi/scripts/storage-check.sh` | Server Storage |
| `services` (+ `edge`, heal) | `reference/cestmoi/scripts/service-check.sh` | Server Internal Services |
| `backup` | push at the end of `backup.sh` | Server Backup |

**For the same machine state, the message must be byte-identical to what the script prints** (`echo "$status: $msg"` minus the `status: ` prefix), including separators, trailing spaces and ordering. The scripts are the specification for semantics too (thresholds, sustain logic, retry rules). `tests/parity/<check>/<case>/` holds a fixture host state and the expected `status` + `msg`; add a case for every branch of the script. Deliberate deviations (bugs fixed) are listed in § Deviations and nowhere else.

Push: `GET {push_base}/api/push/{token}?status={up|down}&msg={urlencoded}&ping=` with 15 s timeout and 2 retries, exactly one `status` and one `msg` parameter.

## Engine

- Checks are async tasks; a check never blocks another. Each has `timeout` (default: interval/2, max 4 min).
- **Sustained conditions** (`sustain_runs`): state in `moi.db` keyed by check+condition; condition reported only when true for N consecutive runs; cleared when false. Matches the scripts' `hot-*` files.
- **Deltas** (CPU, restart counts, kernel log windows) use the previous run's stored values; the first run after install reports no delta-based problems (as the scripts do).
- Results: `moi check` prints `status: message` per check (exit 0 if all up, 1 if any down, 2 on error); `--json`; `--push` pushes even when run by hand; without `--push` a manual run never pushes (so testing does not disturb Kuma).
- Every result also goes to the journal (`SYSLOG_IDENTIFIER=moi-check-<name>`), replacing `systemd-cat -t <name>-check`.

### Decisions (M4)

- **Dry and recorded runs.** A recorded run (the daemon's) stores the check's memory (previous counters, restart counts, sustain counts) and its result in `moi.db` (`check_values`, `sustained`, `check_results`, last 500 per check). A manual `moi check` is a dry run: it reads the memory and writes nothing, so it can be repeated without moving deltas or counting a hot run twice.
- **Sustain** generalises the scripts' `hot-*` files to `sustain_runs`: a condition is reported from the N-th consecutive recorded run on, and forgotten as soon as it does not hold. The text `for {M}+ min` uses M = `sustain_runs` × `interval` in whole minutes.
- **A check that dies still reports** (Deviation 2): a panic or a run longer than `timeout` gives `down` with `check crashed: {reason}` and leaves the memory untouched. `timeout` defaults to half the interval, at most 4 minutes (this replaces the 60 s default noted in 01).
- **Kernel log** entries are read with `journalctl -k --since -{window} --no-pager -q` (the scripts' own command; the window is written in minutes when it is whole minutes). No journal → no events.
- **`resources`**: container restart counts come from the Docker API for every container, task containers included, exactly as `docker ps -aq` gave them; names compare in byte order. An unreachable engine means "no containers", as it did for the script.
- **`storage`**: `smartctl --json -H -A -d {type} {dev}`, 30 s. The exit status is the script's bit mask; a missing `smartctl` is `SMART unreadable` (exit 127 has that bit set). NVMe values come from `nvme_smart_health_information_log` (`critical_warning` printed as `0x%02x`, `percentage_used`, `media_errors`, `temperature`); ATA values from `ata_smart_attributes.table`, taking the leading integer of each `raw.string` (what awk's `$10` saw), sectors = attributes 5 + 197 + 198, temperature = the first of 194 / 190 in table order. `disks = "auto"` means every block device in `/sys/block` except loop, ram, zram, dm, sr, md, fd and nbd, typed `nvme` by name and `auto` otherwise; `temp_max` defaults to 70 for NVMe and 55 for everything else.
- Space is `statvfs`: used % = ⌈used / (used + available to users)⌉ like `df`, inodes likewise (no inode count → not judged). A filesystem is read-only when `ro` is among its mount or superblock options. Each filesystem probe and the write test get 10 s; no answer in time is `(df failed)` / `(not writable)`. `writable` may list several mounts; each is tested in order.
- **`custom`**: `run` is split on whitespace (no shell); a script that prints nothing gets `OK` (or `exit status N`); one that cannot be started is `down` with `cannot run …`; stderr goes to the journal under the check's tag.
- **`moi check [name] [--push] [--json]`** runs the enabled checks (or one). One check prints exactly `status: message`, as the script did; several print `[name] status: message`. Exit 0 when all are up, 1 when any is down, 2 on errors. `--json`: `{ "ok": true, "down": N, "checks": [{name, status, message, duration_ms, monitor, pushed}] }`. `--push` pushes each result and writes it to the journal under the check's `journal_tag`; `pushed` is `"ok"` or the reason nothing was pushed (no binding yet, `monitors.mode = "shadow"`, Kuma's answer). A failed push never changes the check's own result or the exit code.
- **Push** needs the monitor's token, which `moi monitors sync` records in `moi.db` when it adopts the monitor (06). The message is percent-encoded; three attempts in all (the scripts' `--retry 2`).

### Decisions (M4, `services`)

- **Order and concurrency.** Entries appear in the script's order: containers (the engine's list order, as `docker ps` gives it), swarm services by name, apps, databases, heal rules with a `container_on_network` check, LAN sites, public URLs, proxy log, firewall, heal rules with a `failing` trigger. The app checks, the database checks and the public probes each run side by side (threads) and are reported in configured order: with the network down every one of them runs into its timeout, and one after the other they would outlast the check's own timeout.
- **Containers**: a swarm task container (label `com.docker.swarm.task.id`) is neither counted nor judged. `ignore`d containers are counted. Health is shown whenever the container has a health status and is being reported (`(restarting, starting)`).
- **Swarm services**: running / wanted tasks come from `GET /services?status=true` (Docker ≥ 20.10). A service whose status the engine does not give is not judged. No swarm → 0 services.
- **Apps**: `checks.services.network` unset → the container's first network with an address. Only the status and the body decide; redirects are not followed.
- **Databases**: `cmd` is split on whitespace (no shell); stdout and stderr together are searched for `expect`.
- **Public probes**: a probe is fine when the status of the response it *started* to get is in `ok_status` — even if the rest never arrived, which is what the script's `case $P_CODE` did. `{ip}` is the address tried. The retry comes `public_retry_after` later; `DNS now` is the first IPv4 address the resolver gives within 5 s. Every probe that failed first is appended to `public_log` as `{RFC 3339 UTC time} {entry}` (the script wrote local time without a zone), whether the retry worked or not, also on a manual run; a log that cannot be written is ignored.
- **Proxy log**: lines of the proxy container's stdout and stderr since `proxy_error_window` containing ` ERR ` or `level=error` (any case). The window is shown as configured (`5m`). No `proxy_container` → not checked. **Firewall**: `iptables -S DOCKER-USER`; no output (no iptables, no chain) → every required text is missing.
- **Heal** (10): a rule with a `container_on_network` check fires when the container has no address on that network (also when the container is gone); a rule with `when = { failing = [...] }` fires when an entry *named* exactly like one of the list is failing (`gluetun`, `gluetun-portforward`, `traefik-lan:home`, `public:notes.example.com/`, `traefik`, `firewall`). The script matched `gluetun` as a substring; cestmoi's config lists `gluetun-portforward` to keep that. What is appended: `report` when the action ran, `report_cooldown` when it ran less than `cooldown` ago, the new `report_failed` when the action failed (the script's `traefik (not on homelab network, reconnect failed)`), or `[{stdout}; next check confirms]` with `report_stdout` (nothing when the action printed nothing). Defaults: `[{name}: healed; next check confirms]`, `[{name}: already tried in the last {cooldown}, needs a look]`, `[{name}: heal failed: {error}]`. The run is recorded in `moi.db` (`heal_cooldowns`) *before* the action starts, and a failed action counts for the cooldown, as the script's stamp did. `wait_healthy` polls every 5 s; a container without a health check is healthy when it runs; the dependants are restarted even if it never gets healthy.
- **A manual `moi check` never heals**: where a rule would fire it appends `[{name}: not healed by a manual check]`.
- **`[checks.edge]` with its own `monitor`** (or without `[checks.services]`) is a check of its own named `edge`: `Failing: {edge entries}` / `All {n} edge checks OK`, and `services` then reports `All {total} containers + {svc_total} swarm services running, {http} app, {exec} database checks OK`. Heal rules stay with `services`. An edge that is not enabled is left out of the counts.
- **Probe implementation**: resolver, TCP, TLS (rustls, handshake completed before the request is sent) and HTTP/1.1 are separate steps under one deadline; each step gets what is left of the 15 s. Times are measured on a monotonic clock and printed with six decimals.

### Parity evidence

`tests/parity/<check>/<case>/` cases run through `tests/parity.rs`. For `resources`, the same test file also runs the **legacy script itself** (paths redirected into a sandbox; `journalctl`, `docker` and `curl` stubbed with the case's data) and requires its output to equal the case's `expected` file — so those files are the script's word, not a transcription of the implementation. For `services` it does the same with `service-check.sh`: `docker`, `curl`, `getent` and `iptables` are `tests/parity/legacy/stub.py`, which answers from the case's files by the rules of the fixture network and the fake Docker engine (curl's `-w` timings included); the script's log and heal-stamp paths point into a sandbox and `IGNORE` comes from the case's config. All cases agree. Not covered by cases: section 4f of the script (Deviation 4).

`storage-check.sh` parses smartctl's text output, which the cases do not carry (moi reads `--json`), so its `expected` files are derived by hand from the script; the first runs on cestmoi in shadow mode (11) are the check on those.

## Built-ins

### `resources`
Exactly `resource-check.sh`:
- CPU: `/proc/stat` first line, total = user+nice+system+idle+iowait+irq+softirq+steal, idle = idle+iowait; % = 100·(Δtotal−Δidle)/Δtotal using integer division; info `CPU {n}%`; sustained over `cpu_limit` → `CPU {n}% for {M}+ min` where M = `sustain_runs × interval` in minutes (defaults 2 × 5 → `for 10+ min`, exactly the script's text).
- RAM: `100·(MemTotal−MemAvailable)/MemTotal` integer; info `RAM {n}%`; sustained → `RAM {n}% for 10+ min`.
- Swap (if SwapTotal > 0): info `swap {n}%`; ≥ limit (not sustained) → `swap {n}% used (memory pressure)`.
- CPU temp: hottest `temp*_input`/1000 across hwmon devices named `coretemp|k10temp|zenpower|cpu_thermal|acpitz`; info `CPU {t}C`; sustained → `CPU {t}C for 10+ min`.
- OOM: kernel journal entries in `oom_window` matching `out of memory|oom-kill|killed process` (case-insensitive) → `kernel killed processes for lack of memory ({n} events; journalctl -k --since -10min)`.
- Container restarts: RestartCount per container vs previous run (by name) → `crashed and restarted: name(+d) name2(+d) `.
- Message: problems joined as in the script (`bad+="… "`); `down` → `Problem: {bad}| {info}`, `up` → `OK: {info}`; info items joined `, `.

### `storage`
Exactly `storage-check.sh`, configured by `checks.storage` (disks/mounts default from discovery):
- Per disk: missing → `{name} (disk missing) `; `smartctl -H -A -d {type} {dev}` (use `--json` and map fields; exit-code bits as in the script: timeout → `SMART timed out`, bit 2 → `SMART unreadable`, bit 8 → `SMART says FAILING`, bit 16 → `SMART attribute past failure threshold`); NVMe: critical warning ≠ 0x00, percentage used (info `{name} {n}% worn`, ≥ limit → `({n}% of rated life used)`), media errors > 0; SATA: attrs 5+197+198 raw sum > allowed → `({n} bad/pending sectors)`, temp from 194/190; temp info `{name} {t}C`, ≥ max → `({t}C, too hot)`.
- Kernel log (window) matching `I/O error|EXT4-fs error|XFS .*error|Buffer I/O|blk_update_request|critical medium error|uas_eh|reset SuperSpeed|device offline` → `kernel log ({n} disk/USB errors in last 6 min, see: journalctl -k --since -10min) `.
- Mounts: `statvfs` used % (df rounding: ceil of used/(used+avail)) and inode %; info `{m} {pct}% used`; `({pct}% full)`, `({ipct}% of inodes used)`, `(read-only)` from mountinfo options; df failure → `({m} df failed)` form as in script.
- Writable mounts: not a mountpoint → `{m} (not mounted) `; touch `{m}/.storage-check` with 10 s timeout fails → `(not writable)`; remove the file.
- Message: `Problem: {bad}` / `OK: {info}`.

### `services`
Exactly `service-check.sh` sections 1–6, configured by `checks.services`, `checks.edge` and `[[heal]]`:
1. Containers (non-swarm-task): not running or unhealthy → `{name} ({state}[, {health}]) `; `ignore` skips.
2. Swarm services: replicas current ≠ desired → `{svc} (swarm {cur}/{want}) `.
3. HTTP checks: address = container IP on `services.network` (or 127.0.0.1 for `localhost`); 10 s timeout; no IP → `(no IP)`; no response / 000 / ≥500 → `(no response)`; with api_key and ≠200 → `(HTTP {code}, check API key)`; `trouble` substring in body → `(reports problem)`. Header `X-Api-Key`.
4. Exec checks: Docker exec with 15 s timeout; non-zero or missing `expect` → `{name} (not accepting connections) `.
5. Edge: heal `container_on_network` evaluated first (message from heal `report`); LAN Host-header probes to `http://127.0.0.1/` → `traefik-lan:{host} (HTTP {code}) ` unless `ok_status`; public probes with the staged diagnosis (§ Probe) + one retry after 3 s + DNS note + log line, failing entry `public:{url-without-scheme} ({first}; {again}; DNS now: {ip|NOT RESOLVING}) ` only if the retry also failed; proxy log errors `traefik ({n} errors in 5m: docker logs {proxy}) `; firewall required text missing in `iptables -S DOCKER-USER` → `firewall (missing: {text}) `.
6. Heal actions in config order (10 § Heal) append their bracketed reports.
- Message (decisions and details: § Decisions (M4, `services`)): `Failing: {bad}` / `All {total} containers + {svc_total} swarm services running, {http} app, {exec} database, {lan+public} edge checks OK`.

#### Probe (public URLs)
Staged timings like curl's `-w` (`time_namelookup`, `time_connect`, `time_appconnect`, `time_starttransfer`, `time_total`, `remote_ip`), implemented with an explicit resolver → TCP → TLS (rustls, system roots) → HTTP/1.1 request so each stage's failure is known. Messages: `HTTP {code} in {total}s` (+ ` (Cloudflare couldn't reach the origin)` for 52x/530); `DNS lookup failed`; `connection refused by {ip}`; timeouts: `timed out in DNS lookup` / `timed out connecting to {ip}` / `timed out in TLS handshake with {ip}` / `connected to {ip}, no response from origin` / `response too slow`; `TLS/cert error: {err}`; `connection dropped by {ip}: {err}`; else `error: {err}`; non-success adds ` after {total}s`. Times formatted like curl (`%.6f` trimmed as curl prints, e.g. `0.412345`). 15 s total timeout.

### `backup`
Not interval-based: pushed by `moi backup` (07) at the end of each run: `up` + `OK` on success; `down` + the last error line (≤ 180 chars) on failure.

### `custom`
`[[checks.custom]]`: runs `run` with `timeout`; exit 0 → up; message = first stdout line (or `OK`); stderr to journal.

## Deviations (bugs fixed on purpose)

1. **Backup push URL**: legacy `KUMA_PUSH_URL` already contained `?status=up&msg=OK&ping=` and the script appended `status`/`msg` again; Kuma stored the message as `[object Object]`. moi sends one status and one message.
2. Legacy scripts that crashed mid-run pushed nothing (Kuma alerted after the interval). moi pushes `down` with `check crashed: {error}` instead.
3. **Wording of connection errors in public probes.** Where the script quoted curl (`TLS/cert error: {curl's text}`, `connection dropped by {ip}: {curl's text}`, `curl error {rc}: {curl's text}`), moi quotes its own TLS library and the operating system (`TLS/cert error: invalid peer certificate: Expired`, `connection dropped by {ip}: Connection reset by peer`, `error: {text}`). The part before the colon is unchanged. Network errors other than "refused" while connecting (no route, network unreachable) are `error: {text}` rather than `connection refused by {ip}`.
4. **Section 4f of `service-check.sh` is not part of the services run.** The script ran the Traefik config guard and restarted the `traefik-homelab` watcher unit on every run. In moi the guard is a heal rule triggered by the file changing, and moi itself is the watcher (10), so `[traefik-homelab watcher was down: restarted]` can no longer occur; the guard's `[{stdout}; next check confirms]` report joins the services message when the daemon runs that rule (M6).
5. **A manual run does not heal** (see Decisions): the script restarted the VPN stack whoever ran it.

## Acceptance

- [ ] Every branch of each legacy script has a parity case; all pass byte-for-byte.
- [ ] `moi check` without `--push` never contacts Kuma (asserted with a mock server).
- [ ] A full `resources`+`storage` cycle on the fixture < 50 ms excluding smartctl; `services` < 200 ms excluding network.
