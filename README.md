# moi

**One binary that runs your Linux server.** moi watches everything on the machine, keeps a versioned record of the whole setup, writes an always-current description of the server for AI assistants, runs your health checks, keeps Uptime Kuma in sync, backs up what matters off-site, and rebuilds the entire server on new hardware with one command — then proves the rebuild survives a reboot.

It replaces the cron jobs, shell scripts and one-off systemd units a self-hosted server accumulates.

> **Status: pre-alpha, under active development.** Every command is implemented and tested against fixture servers, real service containers and (for backup → restore → reboot → verify) real virtual machines in CI; packaging, performance budgets for the daemon and the first real migration are still ahead ([docs/milestones.md](docs/milestones.md)). The spec is in [docs/spec](docs/spec/README.md).

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

The installer downloads the static binary for your CPU from the GitHub release, checks it against the release's `SHA256SUMS`, and installs nothing if they disagree. Every release also has `.deb` and `.rpm` packages (binary in `/usr/bin`, the systemd unit — not enabled until `moi init` —, shell completions, man page), and the crate is `cestmoi` (`cargo install cestmoi`); a Homebrew formula is published to `prabhask5/tap` on release. `moi completions bash|zsh|fish` prints a completion script. See [distribution](docs/spec/12-distribution.md).

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
| `moi devbox up\|down\|status\|doctor` | set up and run a [devbox](https://github.com/prabhask5/devbox) VM (sleeps when idle, agents from your phone) |
| `moi config check` · `moi config path` | validate `moi.toml` + `secrets.toml`, print the effective config with secrets masked |

Global flags: `--config` (`MOI_CONFIG`), `--secrets` (`MOI_SECRETS`, default `secrets.toml` next to the config), `--json`, `--quiet`.

Exit codes: `0` ok · `1` problems found · `2` error.

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
