# 08 — State bundle, push & pull, bootstrap

The **state bundle** is everything needed to rebuild the server *except* the data: `/var/lib/moi/state` (snapshot, history, notes) and `/etc/moi` (config, secrets, firewall rules). It is small (MBs).

## Storage

The bundle is stored **in the same restic repository as the data backup**, as snapshots tagged `moi-bundle` with `--host <server.name>`. One off-site system, one encryption scheme (restic's), one retention policy (`moi-bundle` snapshots: keep last 30 + daily 30), one free-tier guard. Without off-site backup configured, `moi push` fails with instructions; the local copy is the state dir itself.

- `moi push [--force]`: back up the bundle now (skipped if nothing changed since the last bundle push and not `--force`). The daemon pushes automatically after changes, at most once per hour, and every backup run includes it.
- `moi pull [--repository URL]`: on a fresh machine; asks for the repository password (or reads `RESTIC_PASSWORD_FILE`) and S3 keys (env or prompt), restores the latest `moi-bundle` snapshot for the chosen host into `/var/lib/moi/restore/<timestamp>/`, and prints the next command.

## Bootstrap on new hardware

```bash
curl -fsSL https://raw.githubusercontent.com/prabhask5/moi/main/install.sh | sh     # installs the moi binary (+ restic if missing)
sudo moi pull --repository s3:https://<account>.r2.cloudflarestorage.com/<bucket>
sudo moi restore --bundle /var/lib/moi/restore/<timestamp> --dry-run
sudo moi restore --bundle /var/lib/moi/restore/<timestamp>
```

What the owner must keep outside the server (shown by `moi recovery-card`, which prints a checklist with the non-secret parts filled in): restic repository URL, restic password, S3/R2 access key pair, Kuma username/password (Kuma data itself comes back from the backup), and where the dotfiles/devbox rebuild lives if declared fungible.

## Acceptance

- [ ] Push is skipped when unchanged; `--force` pushes; daemon auto-push ≤ 1/hour.
- [ ] Pull on an empty machine (integration test with a local restic repo) recreates the bundle dir byte for byte, including the git history.
- [ ] `moi recovery-card` never prints a secret value.
