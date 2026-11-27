# Test fixtures

Tests never touch the real system. They run against **fixture hosts** through `host::FixtureHost`.

```
hosts/<name>/
  root/            # a fake filesystem root: proc/, sys/, etc/, var/, opt/, home/ … (only what tests need)
  docker/          # Docker Engine API responses: GET paths -> JSON files (containers/json.json, containers/<id>/json.json, …) (M2)
  commands/        # canned outputs of external commands: <key>.out, optional <key>.err and <key>.exit, <key>.cmd for humans
  clock            # fixed "now" (RFC 3339); 2026-01-01T00:00:00Z if absent
  modes            # "mode uid gid /path" lines for what git cannot store (0600 files, owners); everything else is root:root
  secrets          # every secret value planted in this fixture (used by no-leak tests) (M2)
expected/<name>/   # golden outputs (snapshot tree, CONTEXT.md, audit.md, check messages, migrate plan, config-check)
```

How `FixtureHost` behaves:

- **Files** are read from `root/`; absolute symlinks and `..` resolve inside it. The fixture is read-only: a test that writes gives the host an overlay directory (`with_overlay`, under `CARGO_TARGET_TMPDIR`) that shadows `root/`.
- **Commands** never run. The key of a command line (program and arguments joined by single spaces) is its 64-bit FNV-1a hash in hex, `FixtureHost::command_key`. Running a command with no canned output fails with the exact file name to add, so the easy way to add one is to run the test once. Put the command line in `<key>.cmd` so the directory stays readable. Tests can also can outputs in memory (`add_command`).
- **Clock** is fixed and moves only with `advance`/`set_now`. **Environment** is empty unless set. **Journal** entries are collected in memory.

## `minimal`

A server with nothing on it: hostname `myserver.lan`, timezone link, an empty `/etc/moi/moi.toml`, and `cases/` with config files for the `moi config check` tests (`full` valid with secrets, `bad` with one error of each kind, `open-secrets` whose `secrets.toml` is world-readable because `modes` does not list it).

## `cestmoi` (to build in M2)

Built from the prototype's real output on cestmoi (`reference/cestmoi/PROFILE.md`), **redacted**: every token, key,
password and domain replaced (`FAKE…`, `example.com`). It must reproduce the situations the specs call out: Dokploy swarm
services and task containers, `container:<id>` network modes, socket-activated ssh, host-firewall.service with rules files,
an Incus loop mount, read-only and log bind mounts, fungible media, a stale `backup.sh.bak` with a push token, a Kuma
`kuma.db` with the 21 monitors (one with last message `[object Object]`), a modified `resolved.conf`.

Golden files are regenerated with `MOI_UPDATE_GOLDEN=1 cargo test` and reviewed in the PR diff.
