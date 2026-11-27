# 08 — State bundle, push & pull, bootstrap

The **state bundle** is everything needed to rebuild the server *except* the data: `/var/lib/moi/state` (snapshot, history, notes) and `/etc/moi` (config, secrets, firewall rules). It is small (MBs).

## Storage

The bundle is stored **in the same restic repository as the data backup**, as snapshots tagged `moi-bundle` with `--host <server.name>`. One off-site system, one encryption scheme (restic's), one retention policy (`moi-bundle` snapshots: keep last 30 + daily 30), one free-tier guard. Without off-site backup configured, `moi push` fails with instructions; the local copy is the state dir itself.

- `moi push [--force]`: back up the bundle now (skipped if nothing changed since the last bundle push and not `--force`). The daemon pushes automatically after changes, at most once per hour, and every backup run includes it.
- `moi pull [--repository URL]`: on a fresh machine; asks for the repository password (or reads `RESTIC_PASSWORD_FILE`) and S3 keys (env or prompt), restores the latest `moi-bundle` snapshot for the chosen host into `/var/lib/moi/restore/<timestamp>/`, and prints the next command.

### Decisions (M7)

- **What "changed" means for a push**: a digest over the head of the state repository, `NOTES.md` and every file under `/etc/moi`. Equal to the digest of the last push → `nothing changed since the last push`. `--force` and every backup run push regardless.
- A push is `restic backup --tag moi-bundle --host <server.name> <state>/state /etc/moi`, then `restic forget --tag moi-bundle --host <server.name> --keep-last 30 --keep-daily 30`.
- **The daemon** pushes after a snapshot that recorded a change, unless the last push is less than an hour old; what waits is pushed by the next change or the nightly backup. The R2 guard is not asked for these pushes (megabytes, a few dozen operations a day at most); it guards every backup run.
- **`moi pull`**: on a configured machine, from its own repository; otherwise `--repository <URL>` with the password from `RESTIC_PASSWORD` / `RESTIC_PASSWORD_FILE` — or typed at a prompt, where it is visible while typed (moi cannot switch the terminal's echo off; the prompt says so) — and the access keys from the environment, which restic inherits. It restores the newest `moi-bundle` snapshot, whichever host it is from (the output names the host), into `<state root>/restore/<time>/` (0700), where it lies as `var/lib/moi/state` and `etc/moi`.
- **`moi recovery-card`** names, for each thing to keep, the *secret it is stored under* (`secret restic_password in /etc/moi/secrets.toml`) — the repository URL too, because on cestmoi it is itself a secret reference. The whole card goes through the masker before it is printed.

## Bootstrap on new hardware

```bash
curl -fsSL https://raw.githubusercontent.com/prabhask5/moi/main/install.sh | sh     # installs the moi binary (+ restic if missing)
sudo moi pull --repository s3:https://<account>.r2.cloudflarestorage.com/<bucket>
sudo moi restore --bundle /var/lib/moi/restore/<timestamp> --dry-run
sudo moi restore --bundle /var/lib/moi/restore/<timestamp>
```

What the owner must keep outside the server (shown by `moi recovery-card`, which prints a checklist with the non-secret parts filled in): restic repository URL, restic password, S3/R2 access key pair, Kuma username/password (Kuma data itself comes back from the backup), and where the dotfiles/devbox rebuild lives if declared fungible.

## Acceptance

- [x] Push is skipped when unchanged; `--force` pushes; daemon auto-push ≤ 1/hour.
- [ ] Pull on an empty machine (integration test with a local restic repo) recreates the bundle dir byte for byte, including the git history.
- [x] `moi recovery-card` never prints a secret value.
