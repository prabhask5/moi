# moi specification

Read [CLAUDE.md](../../CLAUDE.md) first. Each file states behaviour and ends with acceptance criteria that become tests.

| # | Spec | Covers |
|---|---|---|
| 01 | [Configuration](01-config.md) | `moi.toml`, `secrets.toml`, `moi init` discovery |
| 02 | [Snapshot & history](02-snapshot.md) | collectors, state layout, git history |
| 03 | [Context](03-context.md) | `CONTEXT.md`, redaction, owner notes, Claude Code pointer |
| 04 | [Audit](04-audit.md) | coverage, restorability, reboot survival, disk placement |
| 05 | [Health checks](05-checks.md) | built-ins, exact legacy message parity, probes |
| 06 | [Monitors](06-monitors.md) | Uptime Kuma adoption/sync, pushes, healthchecks.io watchdog |
| 07 | [Backup](07-backup.md) | identity vs fungible, DB dumps, restic off-site, R2 free-tier guard |
| 08 | [Bundle](08-bundle.md) | state bundle, push/pull, bootstrap, recovery card |
| 09 | [Restore & verify](09-restore-verify.md) | phased rebuild, DB-first import, guided reboot |
| 10 | [Daemon](10-daemon.md) | scheduler, watcher, heal rules, boot tasks, firewall |
| 11 | [Migration](11-migration.md) | legacy → moi without severing monitors; cleanup |
| 12 | [Distribution](12-distribution.md) | releases, installer, deb/rpm, crates.io, Homebrew |
| 13 | [Performance](13-performance.md) | budgets enforced in CI |
| 14 | [Security](14-security.md) | secrets, egress, destructive ops, dependencies |
| 15 | [devbox](15-devbox.md) | `moi devbox` over devbox's Rust library; snapshot, context, backup, monitor, restore integration |
| 16 | [SSH from anywhere](16-ssh.md) | `moi ssh`: the main server over a Cloudflare Tunnel run as a host service; one command on the server, one on each computer, shared client contract with devbox |

These specs were derived from a working prototype, serverstate (bash and Python; in this repository's history up to `69f44c0` under `reference/`). The legacy scripts that define check semantics live on in the cestmoi fixture, [`tests/fixtures/hosts/cestmoi/root/opt/scripts/`](../../tests/fixtures/hosts/cestmoi/root/opt/scripts/), where the parity tests run them.
