# 15 — devbox

[devbox](https://github.com/prabhask5/devbox) turns a server with Incus into a personal development machine: an Incus VM that sleeps when idle and wakes on demand, VS Code in the browser, an agents app for coding agents, repo sync and developer context. It is a separate project: Go services (a gateway container on the host, a hub in the VM) plus Rust libraries and a `devbox` CLI. devbox's side of this integration is devbox's spec 12 (`docs/spec/12-moi.md` in the devbox repository), which defines what its libraries guarantee.

moi integrates devbox in two ways and never merges with it:

- **Embeds** devbox's Rust library `devbox-core` (crates.io) and exposes its host commands as `moi devbox …`, running them through moi's own `Host`, firewall and records.
- **Observes** devbox like any other workload: snapshot, context, backup classification, Uptime Kuma monitor, restore.

Everything here is optional and feature-detected: no `/etc/devbox/devbox.toml` → no devbox section anywhere, and `moi devbox` explains how to set it up.

## `moi devbox`

```
moi devbox up [devbox up flags]     # set up or adopt the VM, gateway config, routing, firewall rule, monitor
moi devbox down [--keep-vm|--destroy]
moi devbox status [--json]
moi devbox doctor [--json]
moi devbox connect
moi devbox password wake|app
```

Same flags, same plans, same output as the `devbox` CLI, because both call `devbox_core` and its renderers (`--json` byte-identical; golden tests compare them). What moi adds:

| | |
|---|---|
| System access | an adapter `DevboxHost<'a>(&'a dyn Host)` implementing `devbox_core::system::System` method for method (devbox spec 12 has the mapping; `run` with a working directory becomes `env -C <dir> …` because moi's `Cmd` has none). Fixture hosts therefore work: `MOI_ROOT=tests/fixtures/hosts/<h> moi devbox status` |
| Prompts | moi asks (wake/app passwords, plan confirmation) through `Host::ask` and passes them in `UpOptions` / a `Confirm` implementation; `--yes` confirms plans but never supplies passwords |
| Firewall | moi's `firewall::Manager` implementation for devbox: the rule ("gateway → VM on code/hub/ssh ports") goes into moi's rules file for the `DOCKER-USER` chain (`/etc/moi/firewall/rules{4,6}.txt`) as a marked block (`# devbox: begin` … `# devbox: end`, replaced in place), then applied exactly like `moi firewall apply` (saved rules, SSH rollback timer, typed confirmation over SSH). With `firewall.manage = false` moi refuses and says to enable it or to run `devbox up --firewall <manager>` instead |
| Monitor | the desired-monitor row below, adopted by name; the URL change is drift, applied by `moi devbox up` after showing it (the owner asked for it by running the command) |
| Records | after `up`/`down`/`password`: `moi snapshot` (with the reason `moi devbox up`), CONTEXT.md regenerated, and `/etc/devbox` + `/var/lib/devbox/gateway` added to `backup.paths` if missing (written to `moi.toml` with a comment) |
| Plain `devbox` on this host | still works (it detects moi and uses `--firewall moi`, which asks the moi daemon over `/run/moi.sock` to add the block and apply); the daemon then snapshots on its own via the watcher |

## Configuration

```toml
[devbox]
enabled = "auto"                     # auto (when /etc/devbox/devbox.toml exists) | true | false
monitor = "Devbox"                   # Kuma HTTP monitor adopted/created for the health endpoint ("" = none)
```

`moi init` writes `[devbox]` only when it finds `/etc/devbox/devbox.toml`, and declares `/var/lib/incus` (the VM disk and its saved memory state) fungible with the reason "devbox VM, rebuilt by `moi devbox up`".

## Snapshot (02)

Collector `devbox` (only when enabled):

- stable: `devbox/devbox.toml` (the host config as read), `devbox/routing.yml` (the Traefik dynamic file it names), the gateway compose app's description, `devbox/status.json` with the stable fields of `devbox_core::status` (instance, hosts, ports, sleep settings, deployed versions)
- volatile: `devbox/state.json` (VM state, last wake/sleep, health problems) from `GET <code host>/__devbox/status` through the staged probe, timeout 3 s
- never: `/etc/devbox/secrets.toml` (only its presence and mode are recorded)

## Context (03)

Section **10. VMs & system containers** gets a "devbox" block when enabled: instance and its role, URLs (`code.` / `agents.`), sleep policy (idle minutes, stateful), current state and last wake/sleep with reason, typical wake time, deployed gateway and hub versions, health (`ok` / problems), where the owner's developer context lives (the dotfiles repo from `[provision]`), and the commands (`moi devbox status|doctor`). The ⚠ Attention list includes devbox health problems and `doctor` failures.

## Backup and audit (07, 04)

- Identity: `/etc/devbox/` (config and secrets — secrets only ever in the encrypted restic snapshot) and `/var/lib/devbox/gateway/` (wake history). Both are proposed for `backup.paths` and reported by the audit as gaps until covered.
- Fungible: `/var/lib/incus` (devbox VM disk and saved state) with the rebuild note; the audit shows it as fungible, never as a gap.
- The gateway container and its Traefik route are already covered as a Dokploy app and as `/etc` capture.

## Monitors (06)

| Source | Monitor | Type | Defaults |
|---|---|---|---|
| devbox (`devbox.monitor`) | the configured name (default `Devbox`) | http | URL `https://<code host>/__devbox/health`, accepted status 200–299, interval 60 s, retries 2 |

Adoption follows the usual rules: an existing monitor with that name and type `http` is adopted; its URL, interval and retries differing from the defaults is drift (shown by `monitors sync`, applied with `--update` or by `moi devbox up`); a paused monitor is reported, and resumed only by `moi devbox up` after confirmation. Asleep is healthy (the endpoint never wakes the VM), so sleep never alerts.

## Restore (09)

New phase **devbox**, after `incus` and `docker` (the gateway container is a Dokploy app restored with the rest) and before `firewall`:

1. If `/etc/devbox` came back with the data restore and the instance does not exist (its disk is fungible), run `devbox_core::up` non-interactively from the restored config and secrets — `[provision]` holds the VM size, image, user, agents and dotfiles URL, so no input is needed. It recreates and provisions the VM, re-clones the dotfiles (which bring back the owner's personal setup and developer context), and re-applies routing; its firewall rule is part of moi's restored rules files.
2. If the instance exists (its disk was not fungible), run `up` in adopt mode, which only reconciles config.
3. Verify: the health endpoint answers `ok` through the public path; the phase reports the VM's first wake time.

Repositories in the VM come back through devbox's own repo sync on the first wake; nothing in them is backed up by moi.

## Dependencies

`devbox-core` (crates.io, pinned to a `0.<minor>` until devbox 1.0; MIT; it brings `argon2` and `getrandom`, both within the license allowlist). moi's MSRV is the ceiling for devbox's library crates (devbox spec 12 enforces it in devbox's CI with moi's `deny.toml`); if moi raises its MSRV it tells devbox (an issue) so devbox's `compat` job follows.

## Acceptance

- [ ] `moi devbox status|doctor --json` byte-identical to `devbox … --json` for the same fixture (golden), including under `MOI_ROOT` fixture hosts.
- [ ] `DevboxHost` adapter: every `System` method round-trips through `FixtureHost` (files with modes, commands incl. working directory, unix connection to a fake Incus, clock, randomness).
- [ ] `moi devbox up` plan on the cestmoi fixture: adopts the VM, writes the routing file, adds the marked block to `rules4.txt` (and `rules6.txt` if present) and applies through moi's firewall path, proposes the monitor drift, extends `backup.paths`; `down` removes the block and restores files; both re-snapshot.
- [ ] Firewall: with `firewall.manage = false` → actionable refusal; over SSH → rollback timer armed first; re-running is a no-op.
- [ ] Snapshot collector writes the listed files and never the secrets; context block and Attention entries from a fixture status; audit marks `/var/lib/incus` fungible and `/etc/devbox` covered/uncovered correctly.
- [ ] Monitors: adopt `Devbox` by name, drift on URL, paused monitor reported; create when missing (tagged `moi`).
- [ ] Restore phase on a fixture: non-interactive rebuild when the instance is missing, adopt when present, health verification; no prompt when config + secrets were restored.
- [ ] `[devbox]` config validated; `enabled = "auto"` off when `/etc/devbox/devbox.toml` is absent; `moi init` writes it only when devbox is present.
