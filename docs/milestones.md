# Milestones

Ordered. Each milestone lists the spec sections it implements; acceptance criteria in those sections are the definition of done. `scripts/create-issues.sh` turns this file into GitHub issues (one per milestone, label `milestone`).

**Every milestone ends clean** — the last box of each is the same cleanup pass, and `scripts/check-hygiene.sh` enforces most of it in CI:
- remove the `NotImplemented` arm(s) of the commands it delivers, and any stub, shim or fallback code it superseded;
- no debug output, `dbg!`/`todo!`/`unimplemented!`, commented-out code, or untracked TODOs (allowed only as `TODO(M<n>)`/`TODO(#<issue>)` naming who removes them);
- research spikes: conclusions go into the spec, scratch code and notes are deleted;
- fixtures and golden files: only what tests use, minimal, redacted;
- unused dependencies and features removed (`cargo machete`), new ones listed in spec 14;
- docs updated for what shipped (README for users, spec for behaviour), nothing describing behaviour that doesn't exist.

The repo must be publishable after any milestone; M12 makes it public-ready for 1.0.

## M0 — Repository scaffold ✅
- [x] Crate `cestmoi`, binary `moi`, every command wired (prints "not implemented yet", exit 3)
- [x] rustfmt, clippy lint set, cargo-deny, tests/ layout, CI with `required` gate, release workflow skeleton, installer skeleton
- [x] Spec, CLAUDE.md, reference material (prototype + redacted legacy scripts)

## M1 — Foundations: host, config, state, CLI plumbing ✅
Specs: 01, CLAUDE.md §3 (host), 14
- [x] `host::Host` trait with `RealHost` and `FixtureHost` (root prefix, canned command outputs, fake clock)
- [x] Config schema with defaults, `secret:` resolution, all-errors validation with locations; `moi config check`
- [x] State dir + `moi.db` with schema versioning and forward migrations
- [x] Logging to journal (structured fields), `--json` output plumbing, exit codes
- [x] `examples/moi.toml` golden test against defaults
- [x] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M2 — Snapshot & history ✅
Specs: 02
- [x] Docker Engine API client over the unix socket (containers, networks, volumes, services, events, exec, logs) with fixture-replay tests
- [x] All collectors, parallel, with timeouts; stable/volatile split; incremental capture cache
- [x] Git history via gix, commit summaries incl. docker/swarm diffs; `moi log`, `moi diff`
- [x] Fixture host `cestmoi` built from the prototype's real output (redacted); golden snapshot test
- [x] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M3 — Context, notes, audit ✅
Specs: 03, 04
- [x] CONTEXT.md generator with redaction; golden test; no-secret-leak test
- [x] NOTES.md stubs + `moi notes`; Claude Code pointer block
- [x] `moi context share` (expiring presigned links from a private R2 bucket, `--revoke`, `--list`, lifecycle safety net)
- [x] Audit incl. reboot survival, coverage with fungible paths, restorability, placement (disk-audit parity), monitoring integrity
- [x] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M4 — Checks with exact legacy parity
Specs: 05
- [x] Check engine (intervals, timeouts, sustained state, deltas, history)
- [x] `resources`, `storage`, `services` + `edge` + staged probe, `custom`
- [x] Parity suites for every branch of resource/storage/service-check.sh
- [x] `moi check [name] [--push] [--json]`
- [x] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M5 — Uptime Kuma
Specs: 06
- [x] Read-only kuma.db inventory; push client (one status/msg)
- [ ] socket.io client: login, monitor list, add/edit/pause/tag, docker hosts (1.23 + 2.x)
- [ ] Adoption planner + `moi monitors sync [--dry-run] [--update] [--prune]`
- [ ] CI integration job with `louislam/uptime-kuma:1.23.17`
- [ ] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M6 — Daemon
Specs: 10
- [ ] Scheduler (interval + persistent calendar), watcher (docker events + inotify) with debounce, IPC socket, sd_notify/watchdog
- [ ] Heal rules (network_connect, restart sequence, run, compose_up), cooldowns, reports
- [ ] Boot tasks (mounts, firewall, compose_up, post-boot verify), firewall apply/diff with SSH rollback
- [ ] `moi init` installs/enables `moi.service`; `moi uninstall`
- [ ] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M7 — Backup
Specs: 07, 08
- [ ] DB dump discovery + streaming dumps (incl. swarm tasks), SQLite online backup, Redis BGSAVE
- [ ] Identity set computation, `--dry-run` report, local archive + retention + size guard
- [ ] restic driver, rate limit, R2 free-tier guard (GraphQL), retention/prune/check, repo size guard
- [ ] State bundle (`moi-bundle` tag), `moi push`/`pull`, `moi recovery-card`
- [ ] Legacy log format to `backup.log_file`; backup monitor push; drill (optional)
- [ ] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M8 — Restore & verify
Specs: 09
- [ ] Phases with dry-run/resume; interactive storage mapping; DB-first import
- [ ] Swarm/Dokploy research spike → implementation; Incus preseed
- [ ] `moi verify`, expectations, guided reboot, post-boot verify every boot
- [ ] Nightly VM end-to-end test
- [ ] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M9 — Migration & cleanup (cestmoi)
Specs: 11
- [ ] Import serverstate history/notes/secrets/legacy state
- [ ] shadow / compare / switch / rollback / finish
- [ ] `plan` golden test on the cestmoi fixture; switch+rollback byte-identity test
- [ ] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M10 — Distribution
Specs: 12
- [ ] Release workflow end to end, installer with checksum verification, deb/rpm, crates.io, Homebrew tap
- [ ] Shell completions, man page, `moi --version` build info
- [ ] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M11 — Performance & hardening
Specs: 13, 14
- [ ] `budgets` job with real measurements and thresholds
- [ ] Fuzz config parsing and Docker payload parsing; review every `unwrap` on external data
- [ ] Cleanup pass (see top of this file); `scripts/check-hygiene.sh` green

## M12 — Public release readiness (v1.0.0)
Specs: all; this milestone removes every development artifact
- [ ] `reference/` deleted: legacy script semantics fully captured in `tests/parity/` (inputs + expected messages) and spec 05; prototype behaviour captured in specs and tests; all links to `reference/` updated (git history keeps the originals)
- [ ] No scaffolding: `Error::NotImplemented` and exit code 3 removed from code, docs and tests; no `TODO(M…)` left; `scripts/check-hygiene.sh --release` green (and made the default in CI)
- [ ] Dead code and unused deps removed (`cargo machete`, `cargo +nightly udeps` if available); minimal crate features; `cargo deny` clean; MSRV verified
- [ ] Personal data review: only the redacted `examples/cestmoi.toml` and `tests/fixtures/hosts/cestmoi` remain; full-history secret scan (gitleaks) clean
- [ ] Docs for users: README (what/why, install, quickstart, config, migration guide, FAQ, exit codes), man page, `--help` texts reviewed; spec describes shipped behaviour only; `docs/milestones.md` replaced by a short roadmap; `docs/agent-workflow.md` kept as contributor docs
- [ ] Community files: CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, issue/PR templates, CHANGELOG `1.0.0`
- [ ] Repo settings: description, topics, website, branch protection on `required`, Dependabot (cargo + actions), Actions pinned by SHA
- [ ] Release `v1.0.0` through `release.yml`; installer and deb/rpm verified on clean Ubuntu, Debian and Fedora VMs; crates.io `cestmoi` and Homebrew tap published
- [ ] cestmoi fully on moi: `moi migrate finish` done, serverstate uninstalled, legacy scripts/cron/units gone
