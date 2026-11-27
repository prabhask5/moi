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
- `moi snapshot`: captures packages, `/etc`, users and dotfiles, storage, network, firewall, systemd units, cron and scripts into the state directory, split into stable and volatile parts, incrementally (unchanged files are not read again).

### Changed
- `examples/moi.toml` is now the generated reference of every default; the short example moved to `examples/minimal.toml`.
