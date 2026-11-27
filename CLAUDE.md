# moi

Context for agents and maintainers. This file, `docs/spec/` and the code are the whole truth; there is no other design document. When the code and this file disagree, fix one of them in the same change.

## 1. What it is

**moi** is a single Rust binary that runs a Linux server's infrastructure: it watches everything on the machine, keeps a versioned record of the whole setup, writes an always-current description of the server for AI assistants, runs the health checks, keeps Uptime Kuma in sync, backs up what matters off-site, and rebuilds the whole server on new hardware with one command, proving afterwards that the rebuild survives a reboot.

It replaces the pile of cron jobs, shell scripts and one-off systemd units that self-hosted servers accumulate. The first real user is a homelab called **cestmoi** (see `reference/cestmoi/`); every feature must work on *any* Linux server with systemd, configured only through `moi.toml`.

Crate: `cestmoi` (crates.io). Binary: `moi`. License: MIT. Rust stable, edition 2024.

Product pillars, used to break ties:

1. **Never lose identity, never back up the fungible.** Configs, databases and app state are the server's identity and are backed up and restorable. Media libraries, downloads, rebuildable VMs, images and caches are fungible: never backed up, never reported as gaps once declared.
2. **Adopt, never duplicate, never sever.** Existing monitors, push tokens, backup repositories, schedules and integrations are discovered and adopted. moi creates only what is missing and never deletes or breaks what it did not create. Every migration step is reversible.
3. **Fast and quiet.** One long-running daemon, no process-per-check. Reads the kernel and the Docker API directly. A full check cycle costs milliseconds; idle cost is near zero. Budgets are enforced in CI (`docs/spec/13-performance.md`).
4. **Safe by default.** Read-only unless told otherwise. Destructive actions (formatting a disk, applying a firewall over SSH, overwriting files, deleting anything) need an explicit confirmation, keep a backup of what they replace, and can be rolled back.
5. **Truthful output.** Health messages are precise and stable (other tools and humans parse them); the context file states only what was observed, with secrets redacted.

## 2. Commands

| Command | What it does | Spec |
|---|---|---|
| `moi init` | Discover the server, write `/etc/moi/moi.toml` + `secrets.toml`, install `moi.service`, take the first snapshot | 01, 10 |
| `moi snapshot` / `moi log` / `moi diff [rev]` | Capture full server state into the versioned state dir; show history | 02 |
| `moi context` / `moi notes` | Regenerate `CONTEXT.md`; edit the owner's notes merged into it | 03 |
| `moi context share [--ttl 1h]` | Expiring link to CONTEXT.md (private R2 bucket, presigned) for pasting into AI chats | 03 |
| `moi audit` | Gaps: not backed up, not restorable, lost on reboot, data on the wrong disk | 04 |
| `moi check [name] [--push]` | Run health checks (built-in + custom) and print/push results | 05 |
| `moi monitors sync [--dry-run]` | Adopt/create Uptime Kuma monitors for checks, containers, backups | 06 |
| `moi backup [--dry-run]` | Database dumps + identity data → local archive → restic off-site (+ state bundle) | 07, 08 |
| `moi push` / `moi pull` | Upload/download the encrypted state bundle | 08 |
| `moi restore` / `moi verify` | Phased rebuild on new hardware; post-reboot verification | 09 |
| `moi daemon` | The service: scheduler, watcher, checks, heal actions, boot tasks, backups | 10 |
| `moi migrate {plan,shadow,compare,switch,rollback}` | Move a server from legacy scripts/cron to moi without breaking monitors | 11 |
| `moi config check` | Validate config + secrets, print the effective config | 01 |
| `moi uninstall [--purge]` | Remove moi, restore anything it replaced | 11 |

## 3. Architecture

Single crate, library + thin binary (`src/main.rs` only parses args and calls `cestmoi::cli`).

| Module | Responsibility |
|---|---|
| `cli` | clap definitions, output formatting (human / `--json`), exit codes |
| `config` | `moi.toml` + `secrets.toml` schema (serde), defaults, validation, `secret:NAME` references |
| `host` | **All system access**: files under a root prefix, `/proc`, `/sys`, commands, time, sockets, DNS, inotify, sd_notify. `RealHost` and `FixtureHost` (tests). Nothing else touches the OS directly |
| `docker` | Docker Engine API over the unix socket (containers, networks, volumes, services/swarm, events, exec, logs): a small blocking HTTP/1.1 client on a connection from `Host`, payloads as JSON values. No `docker` CLI, no HTTP crate |
| `systemd` | Unit states, enable/start, timers, `TriggeredBy`, journal reads — via D-Bus (`zbus`); `systemctl` only as fallback |
| `snapshot` | Collectors (one per area, run in parallel) → normalized files in the state dir; volatile vs stable split |
| `state` | State dir layout, git history (gitoxide), change summaries, NOTES.md stubs |
| `context` | CONTEXT.md generation + redaction |
| `audit` | Coverage, restorability, reboot survival, disk placement |
| `checks` | Check engine + built-ins (`resources`, `storage`, `services`, `edge`, `backup`) + `custom` scripts; sustained-state store |
| `monitors` | Uptime Kuma (socket.io client over HTTP long-polling, push HTTP, read-only `kuma.db` inventory), healthchecks.io awareness, adoption planner and its application |
| `backup` | DB dump discovery/execution, volume/path selection, local archive, restic driver, R2 free-tier guard, retention |
| `bundle` | State bundle (stored as a tagged restic snapshot), bootstrap |
| `restore` | Phases, interactive storage mapping, DB import, verify + reboot expectations |
| `firewall` | The managed chains: rules files, atomic apply (`iptables-restore --noflush`), save and roll back, drift status |
| `heal` | Heal rule actions (network connect, restart sequence, run), cooldowns and reports; fired by the services check and by the daemon's triggers |
| `daemon` | The service loop (threads and a channel, no async runtime): scheduler, watchers (docker events, inotify), heal triggers, boot tasks, IPC socket `/run/moi.sock`, sd_notify + watchdog |
| `migrate` | Legacy import (serverstate), shadow/compare/switch/rollback, cleanup |
| `log` | Structured journal entries under systemd (native protocol via `Host`), stderr lines otherwise; masks known secrets |
| `net` | Outbound HTTP(S): a blocking HTTP/1.1 client over connections from `Host`, TLS by rustls with the system CA bundle; the staged probe (DNS → TCP → TLS → HTTP, each timed) |
| `redact` | One place for secret detection and masking (env keys, URLs with credentials, tokens, hc-ping keys) |

Rules:
- **Every OS interaction goes through `host::Host`** so every feature is testable against fixtures in `tests/fixtures/`. A test never reads the real `/proc`, `/etc` or Docker socket.
- No async runtime. The daemon is a loop sleeping on a channel with one thread per running job; outbound HTTP (`net`) is blocking, and so is Kuma's socket.io client, which uses the long-polling transport on top of it. The Docker API client is blocking (local socket, one short request per connection; the daemon follows the event stream on its own thread). Collectors may use rayon or tokio tasks; pick one per module and say why in a comment.
- External tools moi shells out to on purpose: `restic` (backups), `smartctl`, `journalctl -k` (kernel log for checks), `iptables` / `iptables-save/restore` (and `ip6tables…`) / `nft`, `systemd-run` (the firewall's rollback timer), `pg_dumpall`/`mysqldump` (inside containers via Docker exec), `docker compose up -d` (heal and boot: Compose has no API), `incus`; and for snapshots the read-only system tools `systemctl`, `ip`, `ss`, `resolvectl`, `lsblk`, `blkid`, LVM/mdadm/zfs/btrfs, `ufw`/`fail2ban-client`/`cscli`, `tailscale`, `wg`. Each is optional and feature-detected (`snapshot::run_tool`): absent means "nothing to record", never an error.
- Errors: `thiserror` in the library, `anyhow`-style context only at the CLI edge. User-facing errors say what to do next.
- No `unsafe` without a comment proving it is needed. `#![forbid(unsafe_code)]` unless a module opts out explicitly.
- Output that other systems consume (Kuma messages, `--json`) is covered by golden tests; changing it is a breaking change.

### Files on a server

| Path | What |
|---|---|
| `/usr/bin/moi` (packages) or `/usr/local/bin/moi` (installer) | the binary |
| `/etc/moi/moi.toml` | config (0644) |
| `/etc/moi/secrets.toml` | secrets (0600, root) |
| `/var/lib/moi/state/` | git repo: `snapshot/` (stable), `NOTES.md`; `volatile/`, `CONTEXT.md`, `audit.md` are git-ignored |
| `/var/lib/moi/moi.db` | SQLite: check state, sustained counters, monitor bindings (check → Kuma id + push token), heal cooldowns, schedule last-runs |
| `/var/lib/moi/archive/` | everything moi replaced or removed (legacy scripts, cron files, units) for rollback |
| `/run/moi.sock` | daemon IPC |
| `/etc/systemd/system/moi.service` | the only unit moi installs |

## 4. Development

```bash
scripts/ci.sh            # everything CI runs: fmt, clippy, deny, tests, docs links, budgets
cargo test               # unit + integration tests (fixtures only)
cargo run -- --help
MOI_ROOT=tests/fixtures/hosts/minimal cargo run -- config check                      # run against a fixture host
MOI_ROOT=tests/fixtures/hosts/cestmoi cargo run -- snapshot --state /tmp/moi-state   # (fixture arrives with M2)
```

- **Tests live in `tests/`** (CI fails on test files elsewhere; small `#[cfg(test)]` unit tests inside modules are allowed for pure functions). Fixture hosts: `tests/fixtures/hosts/<name>/` (a fake root: `proc/`, `sys/`, `etc/`, `docker/` API responses, `commands/` canned outputs).
- **`MOI_ROOT=<fixture dir>`** makes the binary use `FixtureHost` instead of the real machine: files from `<dir>/root`, canned commands, fixed clock, no journal, only `MOI_*` environment. It is read-only unless `MOI_ROOT_OVERLAY=<dir>` gives it somewhere to write. A `MOI_ROOT` that is not a fixture is an error, never a fallback to the real system.
- **Parity tests**: `tests/parity/` holds inputs and the exact Kuma messages the legacy scripts in `reference/cestmoi/scripts/` produce; built-in checks must reproduce them byte for byte (see `docs/spec/05-checks.md`).
- **Integration tests that need services** (Uptime Kuma 1.23.17 and 2.x, Postgres, an S3 server, restic with a local repo) run in CI service containers and are `#[ignore]` locally unless `MOI_IT=1`.
- Never run moi against the development machine's real system in tests. Never commit secrets: `reference/` is redacted; fixtures use fake tokens (`FAKE…`).
- Formatting `rustfmt` (config in `rustfmt.toml`), lints `clippy -D warnings` with the lint set in `Cargo.toml`, dependency policy `cargo deny` (`deny.toml`: permissive licenses only, no duplicate majors without reason, no git deps).
- Keep dependencies few and justified; add a line to `docs/spec/14-security.md#dependencies` when adding one.

## 5. How work is organized

Milestones are in `docs/milestones.md` (also GitHub issues). One milestone ≈ one or a few PRs. For each:
1. Read this file and the spec sections the milestone names. If the spec is ambiguous, decide, write the decision into the spec in the same PR.
2. Write tests from the acceptance criteria first, then implement.
3. Cleanup pass: remove what the milestone superseded (stubs, `NotImplemented` arms), debug code, scratch files, unused fixtures and dependencies (`scripts/check-hygiene.sh`). The repo must be publishable after every milestone; M12 makes it public-ready (`check-hygiene.sh --release`).
4. `scripts/ci.sh` green, update `README.md`/spec if user-visible behaviour changed, tick the milestone checkbox.

The agent workflow (prompts, PR conventions) is in `docs/agent-workflow.md`.

## 6. Non-goals (v1)

Multi-server orchestration, a web UI, Kubernetes, non-systemd init (OpenRC is planned for later), Windows/macOS servers, re-implementing restic, managing application-level settings inside apps.
