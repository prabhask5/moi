# 11 — Migration from a legacy setup, and cleanup

Moving a running server to moi must not sever a single monitor, change a single message format, lose backup history, or leave duplicates behind. Every step is reversible until `moi migrate finish`.

```
moi migrate plan        # what will be replaced by what (read-only)
moi migrate shadow      # moi runs checks without pushing; legacy keeps running
moi migrate compare     # moi's shadow messages vs the legacy messages Kuma received
moi migrate switch [--only resources,storage,…]   # per item: stop legacy, moi takes over the same monitor/token
moi migrate rollback [--only …]                   # undo switch from the archive
moi migrate finish      # remove archived legacy files from disk (copies stay in the archive git), uninstall serverstate, clean stale state
```

## Plan for cestmoi (what `moi migrate plan` must produce on the fixture)

| Legacy | Replaced by | Kept for continuity |
|---|---|---|
| `/etc/cron.d/resource-check` + `/opt/scripts/resource-check.sh` + `/var/lib/resource-check/` | `checks.resources` | push token of "Server Resources"; CPU/restart deltas imported; journal tag `resource-check` |
| `/etc/cron.d/storage-check` + `storage-check.sh` | `checks.storage` | token of "Server Storage"; tag `storage-check` |
| `/etc/cron.d/service-check` + `service-check.sh` | `checks.services` + `checks.edge` + 3 heal rules | token of "Server Internal Services"; `/var/log/service-check-public.log`; `/var/tmp/service-check-heal` cooldown imported; tag `service-check`; API keys moved to `secrets.toml` |
| `server-backup.timer/.service` + `backup.sh` + `offsite-backup.sh` + `/var/lib/offsite-backup/last-upload` + `/root/.config/restic/r2.env` | `backup` (+ `backup.offsite`) | token of "Server Backup" (message bug fixed); same restic repo/password/tag; `/var/log/server-backup.log`; schedule 03:30; last-upload stamp imported |
| `traefik-homelab.service` + `traefik-homelab.sh` | heal `traefik-homelab` | |
| `traefik-config-guard.path/.service` | heal `traefik-config-guard` | the Python guard stays as the action script (moved to `/etc/moi/hooks/`) |
| `homelab-services.service`, `yams.service` | `boot.compose_up` | |
| `homelab-selfcheck.timer/.service` + `/usr/local/sbin/homelab-selfcheck` | post-boot `moi verify` | `/var/log/homelab-selfcheck.log` written too |
| `host-firewall.service` + `/usr/local/sbin/host-firewall` + `/etc/host-firewall/` | `firewall` | rules files copied verbatim |
| `/opt/scripts/disk-audit.sh` | `moi audit` (placement) | |
| `/opt/scripts/backup.sh.bak` | — (stale copy, archived and removed) | |
| serverstate prototype (`/var/lib/serverstate`, `/etc/serverstate.conf`, `/usr/local/lib/serverstate`) | moi state | git history and NOTES.md imported; then `serverstate uninstall --purge` |

Items moi does not replace are listed as *untouched* (apps' own containers such as cloudflare-ddns pushing to its own Kuma monitor, Watchtower, etc.).

## Steps

1. **Import** (part of `plan`/`shadow`): serverstate history → `/var/lib/moi/state` (history rewritten under the new layout, commit dates kept), NOTES.md, secrets from scripts/env files into `secrets.toml`, legacy state values into `moi.db`.
2. **Shadow**: `monitors.mode = "shadow"`; moi runs checks on the same schedule, stores messages, pushes nothing. Backups in shadow run `--dry-run` only.
3. **Compare**: for each check, legacy messages = Kuma heartbeats of the adopted monitor (read-only `kuma.db`) at times near moi's shadow runs. Messages are compared after masking numbers (`\d+(\.\d+)?` → `N`), then raw. Report: identical / same-shape / different (with diff). `switch` refuses a check whose last 12 comparisons are not identical-or-same-shape, unless `--force`.
4. **Switch** (per item, atomic, logged to `/var/lib/moi/archive/migration.log`): copy legacy files into `/var/lib/moi/archive/` (git), disable the legacy trigger (move the cron file into the archive; `systemctl disable --now` the unit), bind the check to the same monitor/token, enable pushing. Backup: disable `server-backup.timer`, enable moi's schedule at the same time; the next run must succeed before the item is marked switched.
5. **Rollback**: re-install archived files and units from the archive, re-enable them, unbind moi.
6. **Finish**: after all items are switched and stable for 7 days (or `--now`): delete the legacy files from their original locations (archive keeps them), remove stale copies and legacy state dirs, run the serverstate uninstaller, re-run `moi audit` and `moi monitors sync --dry-run` (must be all keep/adopt).

## Acceptance

- [ ] `plan` on the cestmoi fixture equals the table above.
- [ ] After `switch` + `rollback` the fixture host is byte-identical to before (files, units, cron).
- [ ] No step ever creates a Kuma monitor for a check that has a legacy monitor; the token in use never changes.
- [ ] `finish` leaves no duplicate scripts/cron/units and frees the legacy files' space; archive contains everything removed.
