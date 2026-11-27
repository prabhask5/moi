# Milestones

Ordered. Each milestone lists the spec sections it implements; acceptance criteria in those sections are the definition of done. `scripts/create-issues.sh` turns this file into GitHub issues (one per milestone, label `milestone`).

## M0 — Repository scaffold ✅
- [x] Crate `cestmoi`, binary `moi`, every command wired (prints "not implemented yet", exit 3)
- [x] rustfmt, clippy lint set, cargo-deny, tests/ layout, CI with `required` gate, release workflow skeleton, installer skeleton
- [x] Spec, CLAUDE.md, reference material (prototype + redacted legacy scripts)

## M1 — Foundations: host, config, state, CLI plumbing
Specs: 01, CLAUDE.md §3 (host), 14
- [ ] `host::Host` trait with `RealHost` and `FixtureHost` (root prefix, canned command outputs, fake clock)
- [ ] Config schema with defaults, `secret:` resolution, all-errors validation with locations; `moi config check`
- [ ] State dir + `moi.db` with schema versioning and forward migrations
- [ ] Logging to journal (structured fields), `--json` output plumbing, exit codes
- [ ] `examples/moi.toml` golden test against defaults

## M2 — Snapshot & history
Specs: 02
- [ ] Docker Engine API client over the unix socket (containers, networks, volumes, services, events, exec, logs) with fixture-replay tests
- [ ] All collectors, parallel, with timeouts; stable/volatile split; incremental capture cache
- [ ] Git history via gix, commit summaries incl. docker/swarm diffs; `moi log`, `moi diff`
- [ ] Fixture host `cestmoi` built from the prototype's real output (redacted); golden snapshot test

## M3 — Context, notes, audit
Specs: 03, 04
- [ ] CONTEXT.md generator with redaction; golden test; no-secret-leak test
- [ ] NOTES.md stubs + `moi notes`; Claude Code pointer block
- [ ] Audit incl. reboot survival, coverage with fungible paths, restorability, placement (disk-audit parity), monitoring integrity

## M4 — Checks with exact legacy parity
Specs: 05
- [ ] Check engine (intervals, timeouts, sustained state, deltas, history)
- [ ] `resources`, `storage`, `services` + `edge` + staged probe, `custom`
- [ ] Parity suites for every branch of resource/storage/service-check.sh
- [ ] `moi check [name] [--push] [--json]`

## M5 — Uptime Kuma
Specs: 06
- [ ] Read-only kuma.db inventory; push client (one status/msg)
- [ ] socket.io client: login, monitor list, add/edit/pause/tag, docker hosts (1.23 + 2.x)
- [ ] Adoption planner + `moi monitors sync [--dry-run] [--update] [--prune]`
- [ ] CI integration job with `louislam/uptime-kuma:1.23.17`

## M6 — Daemon
Specs: 10
- [ ] Scheduler (interval + persistent calendar), watcher (docker events + inotify) with debounce, IPC socket, sd_notify/watchdog
- [ ] Heal rules (network_connect, restart sequence, run, compose_up), cooldowns, reports
- [ ] Boot tasks (mounts, firewall, compose_up, post-boot verify), firewall apply/diff with SSH rollback
- [ ] `moi init` installs/enables `moi.service`; `moi uninstall`

## M7 — Backup
Specs: 07, 08
- [ ] DB dump discovery + streaming dumps (incl. swarm tasks), SQLite online backup, Redis BGSAVE
- [ ] Identity set computation, `--dry-run` report, local archive + retention + size guard
- [ ] restic driver, rate limit, R2 free-tier guard (GraphQL), retention/prune/check, repo size guard
- [ ] State bundle (`moi-bundle` tag), `moi push`/`pull`, `moi recovery-card`
- [ ] Legacy log format to `backup.log_file`; backup monitor push; drill (optional)

## M8 — Restore & verify
Specs: 09
- [ ] Phases with dry-run/resume; interactive storage mapping; DB-first import
- [ ] Swarm/Dokploy research spike → implementation; Incus preseed
- [ ] `moi verify`, expectations, guided reboot, post-boot verify every boot
- [ ] Nightly VM end-to-end test

## M9 — Migration & cleanup (cestmoi)
Specs: 11
- [ ] Import serverstate history/notes/secrets/legacy state
- [ ] shadow / compare / switch / rollback / finish
- [ ] `plan` golden test on the cestmoi fixture; switch+rollback byte-identity test

## M10 — Distribution
Specs: 12
- [ ] Release workflow end to end, installer with checksum verification, deb/rpm, crates.io, Homebrew tap
- [ ] Shell completions, man page, `moi --version` build info

## M11 — Performance & hardening
Specs: 13, 14
- [ ] `budgets` job with real measurements and thresholds
- [ ] Fuzz config parsing and Docker payload parsing; review every `unwrap` on external data
