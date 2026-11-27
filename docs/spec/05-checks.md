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
- Message: `Failing: {bad}` / `All {total} containers + {svc_total} swarm services running, {http} app, {exec} database, {lan+public} edge checks OK`.

#### Probe (public URLs)
Staged timings like curl's `-w` (`time_namelookup`, `time_connect`, `time_appconnect`, `time_starttransfer`, `time_total`, `remote_ip`), implemented with an explicit resolver → TCP → TLS (rustls, system roots) → HTTP/1.1 request so each stage's failure is known. Messages: `HTTP {code} in {total}s` (+ ` (Cloudflare couldn't reach the origin)` for 52x/530); `DNS lookup failed`; `connection refused by {ip}`; timeouts: `timed out in DNS lookup` / `timed out connecting to {ip}` / `timed out in TLS handshake with {ip}` / `connected to {ip}, no response from origin` / `response too slow`; `TLS/cert error: {err}`; `connection dropped by {ip}: {err}`; else `error: {err}`; non-success adds ` after {total}s`. Times formatted like curl (`%.6f` trimmed as curl prints, e.g. `0.412345`). 15 s total timeout.

### `backup`
Not interval-based: pushed by `moi backup` (07) at the end of each run: `up` + `OK` on success; `down` + the last error line (≤ 180 chars) on failure.

### `custom`
`[[checks.custom]]`: runs `run` with `timeout`; exit 0 → up; message = first stdout line (or `OK`); stderr to journal.

## Deviations (bugs fixed on purpose)

1. **Backup push URL**: legacy `KUMA_PUSH_URL` already contained `?status=up&msg=OK&ping=` and the script appended `status`/`msg` again; Kuma stored the message as `[object Object]`. moi sends one status and one message.
2. Legacy scripts that crashed mid-run pushed nothing (Kuma alerted after the interval). moi pushes `down` with `check crashed: {error}` instead.

## Acceptance

- [ ] Every branch of each legacy script has a parity case; all pass byte-for-byte.
- [ ] `moi check` without `--push` never contacts Kuma (asserted with a mock server).
- [ ] A full `resources`+`storage` cycle on the fixture < 50 ms excluding smartctl; `services` < 200 ms excluding network.
