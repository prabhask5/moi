# moi

**One binary that runs your Linux server.** moi watches everything on the machine, keeps a versioned record of the whole setup, writes an always-current description of the server for AI assistants, runs your health checks, keeps Uptime Kuma in sync, backs up what matters off-site, and rebuilds the entire server on new hardware with one command — then proves the rebuild survives a reboot.

It replaces the cron jobs, shell scripts and one-off systemd units a self-hosted server accumulates.

> **Status: pre-alpha, under active development.** The spec is complete ([docs/spec](docs/spec/README.md)); commands are being implemented milestone by milestone ([docs/milestones.md](docs/milestones.md)). Working today: `moi config check`, `moi config path` and `moi snapshot`, `moi log`, `moi diff`, `moi notes`, `moi audit`, `moi context`, `moi context share` `moi check` (resources, storage, services with edge probes, custom checks), `moi monitors list`, `moi monitors sync`, `moi daemon` (checks on their intervals, snapshots when something changes, heal rules, boot tasks), `moi status`, `moi firewall apply|diff|status`, `moi init`, `moi uninstall` `moi backup` (database dumps, the local archive, the off-site copy with restic), `moi push`, `moi pull`, `moi recovery-card` `moi verify` (also run by the daemon after every boot) and `moi restore` (phase by phase, with a full `--dry-run`); every other command still exits with "not implemented yet".

## What it does

- **Pick up** — `moi snapshot` captures Docker (compose, swarm, networks, volumes), systemd, cron, firewall, packages, users, disks and directory layouts, scripts, Incus, and your monitoring setup into a git history. The daemon re-captures within minutes of any change.
- **Context for AI** — `CONTEXT.md` describes the server as it is (secrets redacted) with rules for making safe, reproducible changes; Claude Code on the server reads it automatically.
- **Audit** — what isn't backed up, what a rebuild wouldn't restore, what wouldn't survive a reboot, what data sits on the wrong disk.
- **Health checks** — resources, storage (incl. SMART), containers/swarm services, HTTP and database checks, edge/public probes, firewall rules, self-healing actions; results pushed to Uptime Kuma.
- **Uptime Kuma sync** — adopts the monitors you already have (same push tokens, same history), creates only what's missing, never duplicates.
- **Backups** — dumps every database, backs up configs and app state (never fungible data like media libraries) locally and off-site with restic, with a Cloudflare R2 free-tier guard.
- **Put down** — `moi pull` + `moi restore` rebuild the server on new hardware phase by phase, databases first, then `moi verify` after a guided reboot.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/prabhask5/moi/main/install.sh | sh
sudo moi init
```

Also planned: `.deb`/`.rpm` packages, `cargo install cestmoi`, Homebrew (`brew install prabhask5/tap/moi`). See [distribution](docs/spec/12-distribution.md).

## Commands

| | |
|---|---|
| `moi init` | discover the server, write `/etc/moi/moi.toml`, install the service |
| `moi snapshot` · `moi log` · `moi diff` | capture state, history |
| `moi context` · `moi notes` · `moi audit` | AI context, your notes, gaps |
| `moi check [name]` | run health checks |
| `moi monitors sync [--dry-run]` | Uptime Kuma |
| `moi backup [--dry-run]` · `moi push` · `moi pull` | backups and the state bundle |
| `moi restore` · `moi verify` | rebuild and prove it |
| `moi migrate plan\|shadow\|compare\|switch\|rollback\|finish` | move off legacy scripts without breaking monitors |
| `moi config check` · `moi config path` | validate `moi.toml` + `secrets.toml`, print the effective config with secrets masked |

Global flags: `--config` (`MOI_CONFIG`), `--secrets` (`MOI_SECRETS`, default `secrets.toml` next to the config), `--json`, `--quiet`.

Exit codes: `0` ok · `1` problems found · `2` error · `3` not implemented yet.

## Configuration

One file, `/etc/moi/moi.toml`; an empty file is valid and gives snapshots, context and audits only. A feature is switched on by adding its section. Secrets never go in it: write `"secret:NAME"` and put the value under `[secrets]` in `/etc/moi/secrets.toml` (mode 0600) or in the environment as `MOI_SECRET_NAME`.

```bash
moi config check          # every problem at once, as file:line:col; then the effective config, secrets masked
moi config check --json   # the same for scripts
```

Reference: [docs/spec/01-config.md](docs/spec/01-config.md) — examples: [minimal](examples/minimal.toml), [every default](examples/moi.toml), [a real homelab](examples/cestmoi.toml).

## Development

```bash
scripts/ci.sh     # everything CI runs
cargo test
```

Read [CLAUDE.md](CLAUDE.md) (architecture and rules) and [docs/agent-workflow.md](docs/agent-workflow.md). Contributions: [CONTRIBUTING.md](CONTRIBUTING.md). Security: [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
