# moi

**One binary that runs your Linux server.** moi watches everything on the machine, keeps a versioned record of the whole setup, writes an always-current description of the server for AI assistants, runs your health checks, keeps Uptime Kuma in sync, backs up what matters off-site, and rebuilds the entire server on new hardware with one command — then proves the rebuild survives a reboot.

It replaces the cron jobs, shell scripts and one-off systemd units a self-hosted server accumulates.

> **Status: 0.1, before its first real server.** Every command is implemented and tested in CI against fixture servers, real services and real virtual machines (backup → rebuild on a second machine → reboot → verify). The first migration of a real server is what stands between this and 1.0: see the [roadmap](docs/roadmap.md).

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

The installer downloads the static binary for your CPU from the GitHub release, checks it against the release's `SHA256SUMS`, and installs nothing if they disagree. Every release also has `.deb` and `.rpm` packages (binary in `/usr/bin`, the systemd unit — not enabled until `moi init` —, shell completions, man page), which are on the [releases page](https://github.com/prabhask5/moi/releases). From source: `cargo install --git https://github.com/prabhask5/moi` (the crate `cestmoi` on crates.io and a Homebrew formula come with 1.0). `moi completions bash|zsh|fish` prints a completion script. See [distribution](docs/spec/12-distribution.md).

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
| `moi devbox up\|down\|status\|doctor\|connect\|password` | set up and run a [devbox](https://github.com/prabhask5/devbox) VM (sleeps when idle, VS Code and coding agents from any browser): devbox's own commands, with moi's firewall, monitor, records and rebuild |
| `moi ssh up\|status\|connect\|authorize\|confirm\|down` | `ssh <server>` from anywhere through a Cloudflare Tunnel run as a host service: one command on the server (`moi ssh up`), one on each computer (`moi ssh connect` prints it) |
| `moi status` · `moi firewall apply\|diff\|status` · `moi uninstall` | the daemon's jobs; the managed firewall chains; remove moi |
| `moi completions bash\|zsh\|fish` | shell completion script |
| `moi config check` · `moi config path` | validate `moi.toml` + `secrets.toml`, print the effective config with secrets masked |

Global flags: `--config` (`MOI_CONFIG`), `--secrets` (`MOI_SECRETS`, default `secrets.toml` next to the config), `--json`, `--quiet`.

Exit codes: `0` ok · `1` problems found · `2` error.

## Quickstart

```bash
sudo moi init              # looks at the server, writes /etc/moi/moi.toml, starts moi.service, takes the first snapshot
sudo moi audit             # what is not backed up, would not survive a reboot, would not be restored
sudo moi context           # /var/lib/moi/state/CONTEXT.md: the whole server in one file for an AI assistant
sudo moi check             # the health checks, once, by hand
sudo moi status            # what the daemon runs and when
```

`moi init` never overwrites a configuration that exists and switches nothing on that changes the server: backups are a commented proposal in the file it writes, monitors and the firewall are sections you add. Everything moi does before that is reading.

To rebuild a server elsewhere: `moi pull --repository <URL>` on the new machine, `moi restore --bundle <DIR> --dry-run` to see every step, then without `--dry-run`. It asks before each phase, formats a disk only after you type `FORMAT` and its name, and ends with `moi verify` and the reboot that proves the result.

## Moving from your own scripts

If cron jobs and scripts already check the server, push to Uptime Kuma and run the backup, moi takes over one piece at a time, on the same monitors with the same tokens:

```bash
sudo moi migrate plan       # what would replace what; changes nothing
sudo moi migrate shadow     # moi runs alongside and only watches; missing secrets are taken from your scripts
sudo moi migrate compare    # after an hour: do moi's messages match what your scripts told Kuma?
sudo moi migrate switch     # per item: your cron file or unit is stopped, moi reports in its place
sudo moi migrate rollback   # puts back what switch stopped
sudo moi migrate finish     # a week later: removes the old files (copies stay in /var/lib/moi/archive)
```

A check is switched only when its last twelve messages matched (`--force` overrides). Nothing is deleted before `finish`, and `finish` refuses until moi's own backup has succeeded. Details: [docs/spec/11-migration.md](docs/spec/11-migration.md).

## Configuration

One file, `/etc/moi/moi.toml`; an empty file is valid and gives snapshots, context and audits only. A feature is switched on by adding its section. Secrets never go in it: write `"secret:NAME"` and put the value under `[secrets]` in `/etc/moi/secrets.toml` (mode 0600) or in the environment as `MOI_SECRET_NAME`.

```bash
moi config check          # every problem at once, as file:line:col; then the effective config, secrets masked
moi config check --json   # the same for scripts
```

Reference: [docs/spec/01-config.md](docs/spec/01-config.md) — examples: [minimal](examples/minimal.toml), [every default](examples/moi.toml), [a real homelab](examples/cestmoi.toml).

## Questions

**Does it need Docker, or Uptime Kuma, or restic?** No. Each is used when it is there: without Docker there is nothing of Docker to record, without a `[monitors.kuma]` section nothing is pushed, and restic is needed only for off-site backups.

**What does it change on my server?** Until you add sections to `moi.toml`: its own files (`/etc/moi`, `/var/lib/moi`, `moi.service`). Backups, monitors, heal rules, boot tasks and the firewall act only when configured; anything destructive asks first and keeps what it replaces.

**Where are my secrets?** In `/etc/moi/secrets.toml` (root only), never in `moi.toml`, the journal, `CONTEXT.md` or `--json` output — values are masked wherever text leaves the process. The state directory does hold copies of your configuration files as they are; it is root-only, and the off-site bundle is encrypted by restic.

**What is backed up?** What makes the server *that* server: configuration, databases (as dumps), application data, moi's own state. What you declare replaceable under `[data] fungible` — media, downloads, rebuildable VMs — is never backed up and never reported as a gap. `moi backup --dry-run` shows the list.

**How do I remove it?** `sudo moi uninstall` stops the service and removes what moi wrote outside its own directories; `--purge` also deletes the state. Monitors in Kuma, firewall rules in the kernel and backups in their repository are left as they are.

**Which systems?** Linux with systemd, x86_64 and aarch64. The binary is static.

## Development

```bash
scripts/ci.sh     # everything CI runs
cargo test
```

Read [CLAUDE.md](CLAUDE.md) (architecture and rules) and [docs/agent-workflow.md](docs/agent-workflow.md). Security: [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
