# 14 — Security

moi runs as root and holds the keys to the server's backups and monitoring. Rules:

- **Secrets** only in `/etc/moi/secrets.toml` (0600 root, refused if group or others have any access) or `MOI_SECRET_*` env. Never in `moi.toml`, logs, journal, CONTEXT.md, `--json` output, panic messages or commit messages. `redact` is applied to every string that leaves the process except pushes to their own endpoint. Credentials are held in a type whose debug and serialized forms are `***`, and every value loaded from `secrets.toml` or a resolved `MOI_SECRET_*` is masked wherever it appears in logs and output.
- **State dir** `/var/lib/moi` is 0700 root: it contains copies of `/etc/shadow`, `.env` files, SSH keys. Off-site it is protected by restic's encryption.
- **Network egress** only to configured endpoints: Kuma URL, restic repository, Cloudflare GraphQL (guard), the `context.share` bucket, public URLs under `checks.edge`, HTTP checks.
- **Context sharing**: presigned links are bearer tokens — short default TTL (1 h), hard cap (`max_ttl`), private bucket only, scoped API token, revocation by deleting the object, notification on every share. No telemetry, no update checks.
- **Destructive operations** need explicit confirmation and keep a backup: disk format (typed `FORMAT /dev/x`), firewall apply over SSH (auto-rollback), file overwrite during restore (`*.pre-moi`), migration switch (archive + rollback), monitor deletion (only moi-owned, only with `--prune`).
- **Kuma credentials**: used only for `monitors sync`; the daemon logs in only when creating container monitors (`containers = true`). Prefer a dedicated Kuma user if Kuma supports it.
- **Supply chain**: `cargo deny` (advisories, licenses: MIT/Apache-2.0/BSD/ISC/Zlib/Unicode/MPL-2.0 only, bans, sources: crates.io only), `Cargo.lock` committed, GitHub Actions pinned by SHA, release artifacts with checksums and build attestations.
- **Reporting vulnerabilities**: see `SECURITY.md`.

## Dependencies

Each dependency and why (keep this list current when adding one):

| Crate | Why |
|---|---|
| clap | CLI parsing, completions |
| serde, toml, serde_json | config, state files, Docker/Kuma payloads |
| thiserror | library errors |
| sha2 | SHA-256 of captured files (manifest, change detection) |
| md-5 | comparing package config files with the MD5 sums dpkg records (audit § 4); never used for security |
| tokio | daemon runtime, async I/O |
| *(none)* for the Docker Engine API | Decided in M2: a ~200-line blocking HTTP/1.1 client over the unix socket (`docker::http`) instead of hyper/hyperlocal or bollard. moi makes a handful of local requests per run; an async HTTP stack would be most of the dependency tree for no measurable gain |
| rustls (`ring` provider, no default features) | TLS for outbound HTTPS: the share bucket now; staged probes, Kuma and the R2 guard later. Trust anchors are the system CA bundle read through `Host` — neither `webpki-roots` (certificates compiled in) nor `rustls-native-certs` is used. The HTTP/1.1 client on top is moi's own (`net::http1`, shared with the Docker client); HMAC-SHA256 and SigV4 are implemented on `sha2` and verified against published vectors |
| rusqlite (bundled SQLite) | `moi.db`; later read-only `kuma.db` and SQLite online backups. Bundled so behaviour does not depend on the distro's SQLite |
| gix (`sha1`, `revision` features only) | state history without a git binary: object writing, commits, revision parsing and walking. No network, worktree, index-status or blob-diff features; tree comparison and the line diff are moi's own |
| (planned) zbus | systemd over D-Bus |
| (planned) notify | inotify watcher |
| (planned) rust_socketio or a minimal socket.io v4 client | Uptime Kuma API |
| (planned) flate2 / zstd, tar | dumps and local archives |
