# 07 — Backup

`moi backup` (scheduled by the daemon) backs up the server's **identity and data** — configs, databases, app state, moi's own state — locally and off-site, and **never anything fungible**. It replaces `reference/cestmoi/scripts/backup.sh` + `offsite-backup.sh` and must be at least as safe: every guard in those scripts is kept.

## What is backed up

The identity set is computed fresh each run and shown by `moi backup --dry-run` (path, size, why):
1. **Databases** (`databases = "auto"`): every running Postgres container (has `PGDATA` and `pg_dumpall`) and MySQL/MariaDB container (`MARIADB_ROOT_PASSWORD`/`MYSQL_ROOT_PASSWORD`), **including swarm tasks** (dump file named after the service, which is stable). Dump user: `special_users` by image substring (cestmoi: `supabase/postgres` → `supabase_admin`), else `POSTGRES_USER`, else `postgres`; password from the container env. `pg_dumpall -h 127.0.0.1 -U <user>` / `mariadb-dump|mysqldump -u root --all-databases --single-transaction`, run with Docker exec, streamed and gzip-compressed by moi (no temp file in the container), exit code checked via exec inspect. The raw data directories of dumped databases are excluded from file backup (a live copy is useless).
2. **Paths**: `backup.paths` ∪ compose stack directories ∪ sources of non-fungible read-write bind mounts ∪ named volumes that are not DB data dirs and not fungible (swarm-service volumes included: e.g. `stellar-data`, the Shortless Redis volume, Dokploy's `/root/.docker` volume) ∪ `/etc/moi` ∪ the moi state dir ∪ custom `/etc` dirs read by units/scripts.
3. **Minus** `backup.exclude` globs, `data.fungible`, raw DB dirs, caches (`--exclude-caches`).
4. **SQLite consistency**: SQLite files inside the set that belong to a running app (Kuma's `kuma.db`, Jellyfin/arr DBs, Dozzle…) are copied with SQLite's online backup API into a staging dir and the staged copy is backed up instead of the live file. Redis volumes: `BGSAVE` and wait for it before reading.

## Run

1. **Preflight**: lock (one run at a time; replaces `flock`); `require_mount` mounted else fail ("never write backups to the SSD if the HDD isn't mounted"); restic present when off-site is enabled.
2. **Local**: `local_dir/<YYYY-MM-DD_HHMM>/db/<name>.sql.gz` + `configs.tar.zst` of the path set (`tar` semantics: warnings for changed/removed files are not errors). Retention: delete run dirs older than `keep_local`. Total size of `local_dir` > `max_local` → run failed (`ERROR: backups exceed 20 GB`).
3. **Off-site** (only if every previous step succeeded):
   - Rate limit: at most one upload per `min_interval` (state in `moi.db`; on migration import `/var/lib/offsite-backup/last-upload`). A skipped upload is a success (`skipped: last upload was Nh ago`).
   - **R2 free-tier guard** (fails closed): Cloudflare GraphQL query of `r2OperationsAdaptiveGroups` and `r2StorageAdaptiveGroups` for the last 30 days, whole account; Class A = the action list in `offsite-backup.sh`, free actions excluded, Class B = the rest; refuse if Class A ≥ `max_class_a`, Class B ≥ `max_class_b` or storage ≥ `max_storage`, or if usage cannot be read. Port the query verbatim (see the script).
   - `restic unlock`; `restic backup --tag <tag> --pack-size <MB> --exclude-file <generated> --exclude-caches <db dir> <paths…>` (raw paths, not the tarball, so restic dedupes); retry once after 10 min; exit 3 = warning ("some files could not be read").
   - Retention: `restic forget --tag <tag> --keep-daily/weekly/monthly`; on `prune_on` day also `--prune` and `restic check --read-data-subset=<check_subset>`.
   - Repository size (`restic stats --mode raw-data --json`) ≥ `max_repo` → failed.
   - **State bundle** (08): `restic backup --tag moi-bundle` of `/var/lib/moi/state` + `/etc/moi`.
4. **Report**: log lines in the legacy format to the journal and to `backup.log_file` (cestmoi: `/var/log/server-backup.log`, which the `host-logs` container mounts — keep appending there); push the backup monitor (05 § backup); record size/duration/result in `moi.db`; run the audit.

Log line format (kept from backup.sh): `[%F %T] message`, off-site lines prefixed `offsite: `, errors start with `ERROR:`; the failure push message is the last `ERROR` line, max 180 chars.

### Decisions (M7, local run)

- **`configs.tar.gz`, not `.tar.zst`.** The legacy script wrote `configs.tar.gz`; moi keeps the name and the format (gzip from a pure-Rust encoder, the same one the dumps use). zstd would have meant a C library in the build for an archive that is the *local* copy — restic deduplicates and compresses what goes off-site.
- **The identity set of a run** is `Identity` (04) on a snapshot taken at the start of the run, then made concrete against the machine: nested paths collapse into the outermost, paths that do not exist are skipped (and listed by `--dry-run`), sizes come from the walk a run would do.
- **Never in the archive**: `data.fungible`; raw data directories of dumped databases; `backup.exclude` (an entry with `*` is a pattern over the whole path, `*` matching any run of characters; one without is a path and everything below it); `backup.local_dir` itself; moi's staging directory; directories holding a valid `CACHEDIR.TAG`; sockets, devices and pipes. Symlinks are stored as symlinks and never followed.
- **Dumps** run through the Docker API (exec), streamed into a gzip writer on the host: nothing is written inside the container and nothing is held in memory. The password travels in the exec's environment (`PGPASSWORD` / `MYSQL_PWD`), never in a command line. A dump that exits non-zero, or produces nothing, or whose container is not running, is `ERROR: dump failed for {name} ({why})` — the script's line with the reason added. A swarm service is dumped from one of its running tasks into `<service>.sql.gz`. MariaDB's `mariadb-dump` is used where the image has it, `mysqldump` otherwise. Which containers are databases is decided as the audit decides it (environment or image name), not by probing for `pg_dumpall`.
- **SQLite**: every regular file in the set that starts with SQLite's header is copied with the online backup API (source opened read-only; immutable as a second try) into `<state>/staging/sqlite/`, and that copy goes into the archive under the file's own name; `-wal`, `-shm` and `-journal` files of such a database are left out. If the online backup fails the file is archived as it lies, with a warning line. Staging is emptied before and after.
- **Redis** (`redis`, `valkey`, `keydb` images with something mounted at `/data`): `BGSAVE`, then wait up to 30 s for `LASTSAVE` to move. A store that cannot be asked (a password) is read as it lies, with a line in the log.
- **Tar semantics**: a file that vanished or changed size while being read is a `warning:` line (the first 20 are logged), not an error; an error writing the archive is `ERROR: tar failed (…)`.
- **Run directory** `local_dir/<YYYY-MM-DD_HHMM>` in the server's local time, mode 0700; dumps and the archive 0600; `raw-db-dirs` lists the raw database directories as the script did.
- **Retention** goes by the time in a run directory's name, not by mtime: directories named like a run and older than `keep_local` are removed; anything else in `local_dir` is never touched.
- **Size guard**: more than `max_local` bytes under `local_dir` → `ERROR: backups exceed 20 GB` (whole gigabytes as the script printed them; a limit that is not whole gigabytes in its own unit).
- **Lock**: `flock` on `/run/moi-backup.lock`; a second run exits 2 with `another backup is running`.
- **Log lines** are the script's: `[%F %T] message` in local time (and `Backup size: <du -h style>\t<dir>` without a timestamp), written to the journal (`MOI_JOB=backup`), appended to `backup.log_file`, and printed by `moi backup`.
- **After every run**, also a failed one: one push to the backup monitor (`up`/`OK`, or `down` with the last `ERROR` line cut to 180 characters), a row in `moi.db` (`backup_runs`), audit and context refreshed. `moi backup` exits 1 when the run failed.
- **`moi backup --dry-run`** prints the dumps (with the user each is taken as), the paths (size, files, why), what is not on this machine, what is left out and why, and the guards (`ok` / `FAIL`: backups switched on, `require_mount`, the size limit, databases not running). It writes nothing under `local_dir`, runs no dump, and exits 1 if a guard fails. `--json`: `{ plan: {dumps, roots, excluded, missing}, guards }` — never a password.
- The daemon runs the `backup` job at `backup.schedule` (local time, caught up after downtime like every calendar job).

## Adoption (existing restic repository)

`moi init`/`moi migrate` detects an existing restic env file (cestmoi: `/root/.config/restic/r2.env` with `RESTIC_REPOSITORY`, `RESTIC_PASSWORD_FILE`, `AWS_*`, `CF_ACCOUNT_ID`, `CF_API_TOKEN`) and moves the values into `secrets.toml`, keeping the **same repository, password and tag** so snapshot history and dedupe continue. moi never initializes a new repository over an existing one.

## Restore drill (`moi backup drill`)

Optional, scheduled weekly: restore the latest dumps into throwaway database containers (same image), check they load and contain tables; restore a sample of files to a temp dir and compare hashes. Push result to an optional `drill_monitor`. Proves the backups are usable, not just present.

## Acceptance

- [x] On the cestmoi fixture, `--dry-run` lists: dumps for outline-postgres, bookorbit-db, supabase-db (user supabase_admin), dokploy-postgres (swarm); volumes stellar-data, shortless redis, dokploy; paths from config; excludes media and incus; raw DB dirs excluded.
- [ ] Each guard has a test: mount missing, size limit, rate limit skip, R2 usage over limit, R2 usage unreadable, restic failure + retry, exit 3 warning, Sunday prune/check.
- [x] Failure push message = last ERROR line; success = `OK`; exactly one status/msg.
- [ ] Integration test with a local restic repo and a Postgres container: backup, then drill restores and finds the table.
