# Changelog

All notable changes are documented here. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning: [SemVer](https://semver.org/).
Health-check message formats, config keys, CLI flags and the state layout are part of the public interface.

## [Unreleased]

### Added
- Repository scaffold: spec, CLI surface for every command, tooling, CI, release workflow skeleton.
- `moi config check` / `moi config path`: `moi.toml` + `secrets.toml` validation that reports every error with `file:line:col`, `secret:NAME` references resolved from `secrets.toml` or `MOI_SECRET_*`, effective config printed with secrets masked; `--secrets` / `MOI_SECRETS`.
- `--json` output contract: one document with an `ok` field; errors as `{"ok": false, "error": {kind, message, exit_code}}`.
- State directory layout and `moi.db` with versioned, forward-only schema migrations.
- Logging to the systemd journal with structured `MOI_*` fields (stderr when run by hand), known secrets masked.

- Docker Engine API client over the unix socket (containers, networks, volumes, swarm services/nodes/secrets/configs, events, exec, logs) with no HTTP dependency.
- `moi snapshot`: captures packages, `/etc`, users and dotfiles, storage, network, firewall, systemd units, cron, scripts, Docker (containers, networks, volumes, swarm services, compose projects, run scripts for standalone containers), Incus, Uptime Kuma's monitors and every push/healthcheck integration into the state directory, split into stable and volatile parts, incrementally (unchanged files are not read again).
- Change history: every snapshot that changed something is a git commit in the state directory, with a subject and body that say what changed (containers and services in words). `moi log`, and `moi diff [rev]` with secrets masked unless `--show-secrets` is given by root.
- Owner notes: `NOTES.md` with a stub heading for every stack, standalone container, swarm service, VM, unit and scheduled script; `moi notes` opens it in your editor and records the change. What you write is never modified.
- `moi audit`: what would not survive a reboot, what the backup does not cover (fungible paths declared once, never reported as gaps), what a restore would not put back, storage placement (port of `disk-audit.sh`), secrets in scripts, host listeners, monitoring integrity. Exit code 1 when there are gaps.
- `CONTEXT.md`: the whole server in one Markdown file for AI assistants — attention list, owner notes inline, system, storage, network, reverse-proxy routes, Docker, Incus, monitoring, moi's own configuration, firewall, jobs, units, scripts, software, users, backups and coverage, the audit, recent changes. Secrets masked by shape and by value. Regenerated after every snapshot and audit; `moi context` on demand. Copies for non-root readers and a pointer block in `~/.claude/CLAUDE.md`.
- `moi context share [--ttl]`: uploads CONTEXT.md to a private S3/R2 bucket under a random key and prints a presigned link that expires (default 1 h, capped by `max_ttl`); `--list`, `--revoke` (deletes every shared copy at once), a bucket lifecycle rule as a safety net.
- Health checks: `resources` (CPU, RAM, swap, CPU temperature, OOM kills, container restarts) and `storage` (SMART, kernel disk errors, space, inodes, read-only, writable mounts), whose messages are byte-identical to the legacy scripts; custom check scripts; `moi check [name] [--push] [--json]`.
- `services` check: containers, swarm services, apps over HTTP, databases, reverse-proxy routing on the LAN and from the Internet with a staged diagnosis (DNS, connect, TLS, response), proxy errors, firewall rules — byte-identical to the legacy script — and the heal rules these results trigger (reattach a container to its network, restart sequence with cooldown). `[[heal]]` gains `report_failed`.
- Uptime Kuma: read-only inventory from `kuma.db` (no credentials), the adoption planner, `moi monitors list` and `moi monitors sync [--dry-run] [--update] [--prune]`, which adopts existing monitors (never changing their token), creates the missing ones over Kuma's socket API (1.23 and 2.x), and pauses — or with `--prune` deletes — only monitors it created.
- The daemon: checks on their intervals (recorded, pushed, in the journal), a daily snapshot + audit with a freshness push, calendar jobs in the server's local time that are caught up after downtime, `sd_notify` readiness and watchdog, and `moi status` over `/run/moi.sock`. It watches the Docker engine and the file system and snapshots once things are quiet, keeps container monitors current, reloads its configuration when it changes, and runs heal rules on container events and file changes.

### Changed
- `examples/moi.toml` is now the generated reference of every default; the short example moved to `examples/minimal.toml`.
