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

The fake Docker engine (`docker/`, absent = no Docker on this host):

- `GET /x/y?query` is answered from `docker/x/y.json`, the query ignored (`/containers/json` → `containers/json.json`, `/networks` → `networks.json`). Containers can be addressed by id, id prefix or name; their files live under `containers/<full id>/`.
- `GET /events` streams the lines of `events.jsonl` and closes.
- `exec` is answered from `exec/<container name>/<key>.out` (+ `.err`, `.exit`; `.cmd` for humans), keyed like `commands/` by the command line.
- `GET /containers/<c>/logs` returns `containers/<id>/logs.txt` as stdout and `logs.err` as stderr.
- Anything else (`POST`, `DELETE`) is accepted with 204 and changes nothing. Every request is recorded (`docker_requests()`), and a test can replace an answer with `set_docker(path, json)`.

## `minimal`

A server with nothing on it: hostname `myserver.lan`, timezone link, an empty `/etc/moi/moi.toml`, and `cases/` with config files for the `moi config check` tests (`full` valid with secrets, `bad` with one error of each kind, `open-secrets` whose `secrets.toml` is world-readable because `modes` does not list it).

## `docker`

A small Docker host for the client tests: a compose stack (`web`, `db`), a stopped container sharing `web`'s network namespace (`container:<id>`), a swarm service `api` with its task container, bridge and overlay networks, a named and an anonymous volume, events, exec outputs and logs. All tokens are `FAKE…`.

- **Modes and owners**: git stores neither. Fixture files are 0644 (0755 when executable), directories 0755, everything `root:root`, unless `modes` says otherwise. A `.gitkeep` keeps an otherwise empty directory in git and is invisible to the host.

## `cestmoi`

The first real server, rebuilt from its non-secret profile (`reference/cestmoi/PROFILE.md`) and its redacted legacy
scripts — **not** from a dump of the machine: every token, key, password and domain is fake (`FAKE…`, `example.com`).
It has the filesystem (`/etc` with the custom units, cron jobs and firewall rule files, dpkg state, the legacy
scripts with fake push tokens that match the Kuma database, a stale `backup.sh.bak` holding another token, a
user's dotfiles, the `/mnt/storage` layout, a compose file per stack), the canned outputs of the read-only tools
snapshots run (incl. Incus), the Docker engine state (34 containers in 13 compose stacks plus the standalone
`dokploy-traefik`, 7 Dokploy swarm services with their task containers, `container:<id>` network modes) and
Uptime Kuma's `kuma.db` with 21 monitors (one whose last message is `[object Object]`).
It must reproduce the situations the specs call out: Dokploy swarm
services and task containers, `container:<id>` network modes, socket-activated ssh, host-firewall.service with rules files,
an Incus loop mount, read-only and log bind mounts, fungible media, a stale `backup.sh.bak` with a push token, a Kuma
`kuma.db` with the 21 monitors (one with last message `[object Object]`), a modified `resolved.conf`.

`mtimes` (optional) gives fixture files a modification time (`<RFC 3339> /path` per line); every other fixture file is 30 days older than the clock.

`expected/cestmoi/audit.md` is the golden audit of this host. `expected/cestmoi/snapshot/` is the golden stable tree of a snapshot of this host (without `files/`, which the manifest pins by hash).

Golden files are regenerated with `MOI_UPDATE_GOLDEN=1 cargo test` and reviewed in the PR diff.
