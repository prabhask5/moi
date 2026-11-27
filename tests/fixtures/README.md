# Test fixtures

Tests never touch the real system. They run against **fixture hosts** through `host::FixtureHost`.

```
hosts/<name>/
  root/            # a fake filesystem root: proc/, sys/, etc/, var/, opt/, home/ … (only what tests need)
  docker/          # Docker Engine API responses: GET paths -> JSON files (containers/json.json, containers/<id>/json.json, …)
  commands/        # canned outputs of external commands: <command-line-hash>.out / .exit (smartctl, iptables-save, restic …)
  clock            # fixed "now" (RFC 3339)
  secrets          # every secret value planted in this fixture (used by no-leak tests)
expected/<name>/   # golden outputs (snapshot tree, CONTEXT.md, audit.md, check messages, migrate plan)
```

## `cestmoi` (to build in M2)

Built from the prototype's real output on cestmoi (`reference/cestmoi/PROFILE.md`), **redacted**: every token, key,
password and domain replaced (`FAKE…`, `example.com`). It must reproduce the situations the specs call out: Dokploy swarm
services and task containers, `container:<id>` network modes, socket-activated ssh, host-firewall.service with rules files,
an Incus loop mount, read-only and log bind mounts, fungible media, a stale `backup.sh.bak` with a push token, a Kuma
`kuma.db` with the 21 monitors (one with last message `[object Object]`), a modified `resolved.conf`.

Golden files are regenerated with `MOI_UPDATE_GOLDEN=1 cargo test` and reviewed in the PR diff.
