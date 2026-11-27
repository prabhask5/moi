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

The working prototype these specs were derived from is in [`reference/serverstate/`](../../reference/serverstate/); the legacy scripts that define check semantics are in [`reference/cestmoi/scripts/`](../../reference/cestmoi/scripts/).
