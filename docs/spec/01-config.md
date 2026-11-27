# 01 — Configuration

moi is configured by `/etc/moi/moi.toml` (override with `--config` or `MOI_CONFIG`) and secrets in `secrets.toml` next to it, so `/etc/moi/secrets.toml` (0600, root; override with `--secrets` or `MOI_SECRETS`). Everything has a sensible default; an empty `moi.toml` is valid and gives a capture-only setup (snapshot, context, audit) with no checks, backups or monitors.

`moi init` writes both files from discovery (§ Discovery). `moi config check` validates them and prints the effective config with secrets masked.

## Principles

- **TOML, one file**, sections per feature. Unknown keys are an error (typos must not silently disable a check).
- **Secrets never live in `moi.toml`.** Any string value may be `"secret:NAME"`, resolved from `secrets.toml` (`[secrets] NAME = "value"`) or, if absent there, from the environment variable `MOI_SECRET_NAME`. Missing secrets fail validation, naming the key.
- **Durations** are strings: `"30s"`, `"5m"`, `"1h"`, `"20h"`, `"7d"`. **Sizes**: `"20GB"`, `"512KB"` (powers of 1024). **Schedules**: systemd `OnCalendar` syntax (`"03:30"`, `"*-*-* 03:30:00"`, `"Sun 04:00"`).
- **Paths** are absolute. Globs (`*`, `**`) are allowed where noted.
- Validation reports *all* errors at once with `file:line:col`.

### Decisions (M1)

- **A feature is on when its section is present.** `[checks.resources]`, `[checks.storage]`, `[checks.services]`, `[checks.edge]`, `[backup]`, `[backup.offsite]`, `[backup.offsite.r2_guard]` and `[monitors.kuma]` are off in an empty config and on as soon as the section appears (a sub-section such as `[backup.offsite]` implies its parent). Each accepts `enabled = false` to switch it off without deleting its settings. `firewall.manage` stays an explicit opt-in. The effective config always contains every section, so defaults of features that are off are still shown.
- **Required when enabled:** `backup.local_dir`; `backup.offsite.repository` and `.password`; `backup.offsite.r2_guard.account_id` and `.api_token`; `monitors.kuma.url`; `name` + `run` of a custom check; `name`, `when`, `action` of a heal rule; `name`/`target`/`path` of an http check; `name`/`container`/`cmd` of an exec check.
- **Credentials must be references.** `password`, `api_token`, `api_key` and `backup.offsite.env` values whose name looks sensitive (`redact::is_sensitive_key`) are rejected unless written as `"secret:NAME"`. `username`, `repository` and `account_id` may be literal but are always masked in output.
- **Secret names** use letters, digits and `_`. The environment fallback for `secret:restic_password` is `MOI_SECRET_RESTIC_PASSWORD` (name upper-cased); `secrets.toml` wins when both exist. A missing `secrets.toml` is not an error by itself. Every reference must resolve, also in sections that are switched off.
- **`secrets.toml`** contains only `[secrets]` with string values, and is refused if any group/other permission bit is set (the error says `chmod 600`).
- **`"auto"`** is a valid value wherever the default is discovery: `capture.data_roots`, `checks.storage.disks`, `backup.databases`, `backup.volumes`.
- **Check settings:** `interval` defaults to `checks.default_interval`, `timeout` to `60s`, `journal_tag` to `moi-check-<name>`. `[checks.edge]` takes `enabled` and `monitor` only (it runs with `services`). `disks[].type` defaults to `auto`; `temp_max` defaults by disk type (05).
- **Defaults not stated in the schema above:** `checks.storage.mounts = ["/"]`, `writable = []`; `data.fungible`, `context.copies`, `backup.paths`, `restore.etc_paths`, `boot.*` lists, `checks.edge` host/URL lists empty; `monitors.healthchecks.expect_watchdog = false`; `backup.local_dir`, `require_mount`, `log_file` unset.
- **Also errors:** two settings naming the same monitor; a custom check named like a built-in; duplicate custom/heal/http/exec names; `fungible_reason` for a path not in `fungible`; limits outside 1–100 %; relative paths; a heal rule without exactly one trigger and one action. A `monitor` without `[monitors.kuma]` is a **warning** (nothing would be pushed).
- **Schedules** are syntax-checked at config time for the `[weekdays] [date] time` forms and the systemd shorthands (`daily`, `weekly`, …); the scheduler (10) is the authority.
- **Unknown keys** get a "did you mean" when a known key is within two edits. TOML that does not parse is reported with its syntax errors only (no schema errors on top).
- **Files:** `examples/moi.toml` is generated — the effective configuration of an empty `moi.toml` — and a test keeps it identical to the code. `examples/minimal.toml` is the short starting point.

### `moi config check` / `moi config path`

`moi config check` prints the effective configuration in `moi.toml` syntax on stdout (every default spelled out, keys sorted, settings without a value omitted, every secret shown as `***`) and `…: ok (N secrets resolved, masked above)` plus any warnings on stderr; exit 0. On errors it prints one `file:line:col: key: message` line per problem and `moi: configuration is invalid (N errors)` on stderr, nothing on stdout; exit 2.

Masking: credential settings always print `***`; in every other string, any value from `secrets.toml` or a resolved `MOI_SECRET_*` is replaced (whole-value matches always, substrings for secrets of 6+ characters).

With `--json` (contract, golden-tested):

```json
{ "ok": true, "files": { "config": "…", "secrets": "…" }, "secrets_resolved": 7, "warnings": [], "config": { … } }
{ "ok": false, "error": { "kind": "config", "message": "configuration is invalid (2 errors)", "exit_code": 2,
                          "diagnostics": [ { "file": "…", "line": 2, "col": 1, "key": "server.nmae", "message": "unknown key (did you mean `name`?)" } ] } }
```

`line`/`col` are 1-based; `0` means the file as a whole (missing, unreadable, wrong permissions). Every command's `--json` output is one document with an `ok` field; failures of any command use the `error` object above with `kind` one of `config`, `state`, `io`, `not_implemented`, `problems_found`.

`moi config path` prints the two file paths (`--json`: `{ "ok": true, "config": "…", "secrets": "…" }`) without reading them.

## Schema

```toml
[server]
name = "cestmoi"                 # default: short hostname; used in monitor names and bundle tags
timezone = "America/New_York"    # default: system timezone (informational)

[state]
dir = "/var/lib/moi"             # state root (state/, moi.db, archive/)
history_keep = "365d"            # prune git history older than this (squash), default keep all

# ---------------------------------------------------------------- capture (02-snapshot)
[capture]
data_roots = ["/opt", "/mnt/storage", "/home"]   # dirs whose STRUCTURE is recorded (not contents); default: auto
tree_depth = 4
compose_search = ["/opt", "/srv", "/home", "/root", "/docker", "/data", "/mnt"]
script_dirs = ["/usr/local/bin", "/usr/local/sbin", "/opt/scripts", "/root/bin", "/home/*/bin"]
home_files = [".bashrc", ".profile", ".ssh", ".config/rclone", ".config/restic", ".gitconfig"]
extra = []                       # more files/dirs to capture verbatim
etc_exclude = ["/etc/dokploy/applications", "/etc/dokploy/logs", "/etc/dokploy/compose", "/etc/ssl/certs"]
max_file = "1MB"                 # larger files are recorded (path, size, hash) but not copied
prune_dirs = ["node_modules", ".git", "cache", "logs", "data", "db", "postgres", "pgdata", "media"]  # inside stack dirs

# ---------------------------------------------------------------- what is fungible (07-backup, 04-audit)
[data]
# Never backed up, never reported as a gap. Re-downloadable or rebuildable.
fungible = ["/mnt/storage/media", "/var/lib/incus"]
# Why (shown in CONTEXT.md / audit next to each path)
fungible_reason = { "/mnt/storage/media" = "media library, re-downloadable", "/var/lib/incus" = "devbox VM, rebuilt from github.com/prabhask5/dotfiles" }

# ---------------------------------------------------------------- context (03)
[context]
copies = ["/home/prabhas/SERVER_CONTEXT.md"]   # readable copies (owner = owner of the dir)
claude_pointer = true            # maintain a marked block in ~/.claude/CLAUDE.md of each copy owner + root

[context.share]                  # `moi context share`: expiring links for pasting into AI chats (03 § Sharing)
enabled = true
endpoint = "secret:share_endpoint"   # S3 API endpoint, e.g. https://<account_id>.r2.cloudflarestorage.com
bucket = "moi-share"                 # PRIVATE bucket (no public access, no custom domain)
access_key_id = "secret:share_key_id"         # R2 API token scoped to Object Read & Write on this bucket only
secret_access_key = "secret:share_secret"
default_ttl = "1h"
max_ttl = "24h"                      # refuse longer links (S3 presigning allows up to 7 days)
notify = true                        # push a message to the freshness monitor when a link is created

# ---------------------------------------------------------------- health checks (05)
[checks]
default_interval = "5m"
# Every check also accepts: interval, monitor, timeout, journal_tag (default "moi-check-<name>";
# cestmoi keeps its legacy tags "resource-check", "storage-check", "service-check" so log viewers keep working).

[checks.resources]
enabled = true
interval = "5m"
monitor = "Server Resources"     # Kuma monitor to adopt/create (push)
cpu_limit = 90                   # % average since previous run
mem_limit = 90                   # % used excluding reclaimable (MemTotal - MemAvailable)
swap_limit = 80
temp_limit = 85                  # °C hottest CPU sensor
sustain_runs = 2                 # cpu/mem/temp alert only after N consecutive runs over the limit
oom_window = "6m"                # kernel OOM events within this window -> problem
container_restarts = true        # report containers whose RestartCount grew since last run

[checks.storage]
enabled = true
monitor = "Server Storage"
mounts = ["/", "/mnt/storage"]   # space / inodes / read-only
writable = ["/mnt/storage"]      # must be mounted AND writable (touch test, 10 s timeout)
disk_limit = 85
inode_limit = 85
kernel_error_window = "6m"
nvme_wear_limit = 90
allowed_bad_sectors = 0
disks = [
  { device = "/dev/nvme0n1", type = "nvme", temp_max = 70 },
  { device = "/dev/sda", type = "sat", temp_max = 55 },
]                                # default: auto from lsblk (all physical disks, type auto)

[checks.services]
enabled = true
monitor = "Server Internal Services"
ignore = []                      # container names allowed to be stopped
network = "homelab"              # docker network used to reach containers for HTTP checks (default: first shared network)
http = [
  { name = "jellyfin", target = "jellyfin", path = "8096/health" },
  { name = "sonarr", target = "sonarr", path = "8989/api/v3/health", api_key = "secret:sonarr_key", trouble = '"error"' },
  { name = "qbittorrent", target = "gluetun", path = "8081/" },
  { name = "adguard", target = "localhost", path = "8085/" },
]
exec = [
  { name = "outline-postgres", container = "outline-postgres", cmd = "pg_isready -q" },
  { name = "outline-redis", container = "outline-redis", cmd = "redis-cli ping", expect = "PONG" },
]

[checks.edge]                    # part of the services message (same monitor) unless `monitor` is set
proxy_container = "dokploy-traefik"
lan_hosts = ["home", "dokploy.home", "jellyfin.home"]          # Host: header sent to http://127.0.0.1/
public_urls = ["https://notes.example.com/", "https://database.example.com/auth/v1/health"]
ok_status = ["2xx", "3xx", "401"]
public_retry_after = "3s"
public_log = "/var/log/moi/public-probes.log"
proxy_error_window = "5m"        # count ' ERR ' / level=error lines in the proxy container log
firewall_required = ["Intra-network container traffic", "Swarm outbound Internet", "-A DOCKER-USER -j DROP"]  # text that must appear in `iptables -S DOCKER-USER`

[checks.backup]
monitor = "Server Backup"        # pushed by the backup run itself (07)

[[checks.custom]]
name = "something"
run = "/usr/local/bin/my-check"  # exit 0 = up, non-zero = down; first stdout line = message
interval = "10m"
monitor = "My Check"             # optional: its own push monitor; otherwise reported only in `moi check`
timeout = "60s"

# ---------------------------------------------------------------- self-healing (10)
[[heal]]
name = "traefik-homelab"
when = { docker_event = { container = "dokploy-traefik", action = "start" } }   # also evaluated each services run
check = { container_on_network = { container = "dokploy-traefik", network = "homelab" } }
action = { network_connect = { container = "dokploy-traefik", network = "homelab" } }
report = "[traefik had lost the homelab network: reconnected; next check confirms the fix]"

[[heal]]
name = "traefik-config-guard"
when = { path_changed = "/etc/dokploy/traefik/traefik.yml" }
action = { run = "/opt/scripts/traefik-config-guard.py" }    # prints a message only when it changed something
report_stdout = true

[[heal]]
name = "vpn-stack"
when = { failing = ["gluetun", "qbittorrent", "sabnzbd"] }    # names from services results
action = { restart = ["gluetun"], wait_healthy = { container = "gluetun", timeout = "2m" }, then_restart = ["qbittorrent", "sabnzbd"] }
cooldown = "30m"
report = "[auto-restarted gluetun + qbittorrent sabnzbd; next check confirms the fix]"
report_cooldown = "[auto-restart already tried in the last 30 min, needs a look]"

# ---------------------------------------------------------------- boot (10)
[boot]
wait_for_mounts = ["/mnt/storage"]           # before starting stacks
compose_up = ["/opt/services/**", "/opt/yams"]  # stacks to `up -d` at boot, in this order (globs sorted)
verify_after = "4m"                          # run `moi verify` this long after boot; result to journal + monitor
verify_monitor = "Server Boot"               # optional push monitor for the post-boot verify

# ---------------------------------------------------------------- firewall (10)
[firewall]
manage = true                    # moi applies these rules at boot; replaces a custom firewall unit
chains = ["INPUT", "DOCKER-USER"]   # moi owns ONLY these chains; Docker owns the rest
rules_v4 = "/etc/moi/firewall/rules4.txt"   # iptables-restore syntax, chain bodies only
rules_v6 = "/etc/moi/firewall/rules6.txt"

# ---------------------------------------------------------------- backup (07)
[backup]
enabled = true
schedule = "03:30"               # daily; missed runs (machine off) run 5 min after boot
local_dir = "/mnt/storage/backups"
require_mount = "/mnt/storage"   # never write backups to another disk if this is not mounted
keep_local = "7d"
max_local = "20GB"               # total local backups above this = failed run
paths = ["/opt/services", "/opt/yams", "/opt/scripts", "/etc/dokploy", "/etc/moi"]   # identity data beyond auto-discovery
exclude = ["*/cache", "*/Cache", "*/transcodes", "*/logs", "*/log"]
databases = "auto"               # dump every running Postgres/MySQL/MariaDB container incl. swarm tasks; or a list
volumes = "auto"                 # every named volume that is not a DB data dir and not fungible
special_users = { "supabase/postgres" = "supabase_admin" }   # image substring -> dump user
log_file = "/var/log/server-backup.log"   # legacy-format log lines are also appended here (optional)

[backup.offsite]
enabled = true
repository = "secret:restic_repository"      # e.g. s3:https://<acct>.r2.cloudflarestorage.com/bucket
password = "secret:restic_password"
env = { AWS_ACCESS_KEY_ID = "secret:r2_key_id", AWS_SECRET_ACCESS_KEY = "secret:r2_secret" }
tag = "nightly"
min_interval = "20h"
pack_size = "64MB"
keep = { daily = 7, weekly = 4, monthly = 6 }
prune_on = "Sun"
check_subset = "2%"
max_repo = "8GB"

[backup.offsite.r2_guard]        # refuse to upload past half the Cloudflare R2 free tier; fails closed
enabled = true
account_id = "secret:cf_account_id"
api_token = "secret:cf_api_token"
max_class_a = 500000
max_class_b = 5000000
max_storage = "8GB"

# ---------------------------------------------------------------- restore (09)
[restore]
etc_paths = ["/etc/ssh", "/etc/sysctl.d", "/etc/modules-load.d", "/etc/sudoers.d", "/etc/docker", "/etc/dokploy"]  # added to the built-in list
restore_modified_conffiles = true   # put back package config files the owner changed (diff shown)

# ---------------------------------------------------------------- monitors (06)
[monitors]
mode = "live"                    # "shadow" = run checks, push nothing (migration, 11)

[monitors.kuma]
url = "http://uptime.home"
username = "secret:kuma_username"
password = "secret:kuma_password"
notifications = ["Email"]        # attach these existing notifications to monitors moi creates
group = ""                       # optional group monitor to put created monitors in
containers = true                # one Kuma `docker` monitor per container (needs a Kuma docker host)
docker_host = "local"            # Kuma docker host name to use/create (socket /var/run/docker.sock)
freshness = true                 # push monitors for snapshot + backup freshness
push_base = ""                   # base URL moi uses to push (default: url)

[monitors.healthchecks]
# Informational: moi detects and preserves a Kuma -> healthchecks.io watchdog; it never edits it.
expect_watchdog = true
```

## Discovery (`moi init`)

`moi init` inspects the host and writes a commented config containing only what applies:
- `capture.data_roots`: real mounted filesystems except `/`, `/boot*`, `/var`; plus `/opt`, `/srv`, `/home` if non-empty.
- `checks.storage.disks/mounts`: physical disks from sysfs; mounts = `/` + data roots that are separate filesystems; `writable` = removable/USB mounts.
- `checks.services`: nothing is invented; `http`/`exec` start empty with commented examples built from running containers (DB containers get `pg_isready`/`redis-cli ping` suggestions).
- `monitors.kuma.url`: if an `uptime-kuma` container is running, its reachable URL; credentials are prompted (TTY) or left as `secret:` refs.
- `backup`: proposes `paths` from compose stack dirs and bind-mount sources that are not fungible-looking (media/downloads/movies/tv/music/torrents are proposed as `data.fungible`), detects an existing restic env file and repository and proposes adopting it (07 § Adoption).
- Existing legacy setup (serverstate state dir, cron health scripts, backup units) is detected and reported with a pointer to `moi migrate plan`.

Re-running `moi init` never overwrites an existing config; it prints a diff of suggested additions.

## Acceptance

- [x] Unknown key, wrong type, bad duration, missing secret → validation error with location; all errors reported in one run.
- [x] `secret:` resolution from file and env; `moi config check` masks every resolved secret.
- [x] Empty config is valid; defaults documented in `examples/moi.toml` match the code (golden test).
- [ ] `moi init` on the `cestmoi` fixture host produces a config equivalent to `examples/cestmoi.toml` minus hand-written checks.
