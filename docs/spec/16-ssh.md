# 16 — SSH from anywhere

`ssh <server>` works from any computer through a tunnel. The path doesn't depend on Docker, the reverse proxy or anything else that might be the reason you need to log in.

- **On the server:** one command, `moi ssh up`.
- **On each computer (macOS or Linux):** one command that `moi ssh connect` prints. It installs a ~5 MB client and writes one block of SSH config, and nothing else.

```
ssh <server> ─► devbox-ssh proxy ══ wss://ssh.example.com ══► Cloudflare edge ══ tunnel ══► cloudflared (host systemd service) ─► sshd :22
```

The client side is shared with devbox. The transport is "SSH over WebSocket" (binary frames on `wss://<host>/`), which is what Cloudflare Tunnel's `ssh://` ingress speaks. The client is devbox's `devbox-ssh`, and the setup script is devbox's `devbox-ssh-setup.sh`, both published with devbox's `ssh-v*` releases. Their contract (what is written in `~/.ssh`, how it is undone) is devbox's spec 18 (`docs/spec/18-ssh.md` in the devbox repository). So one computer can carry both `ssh <server>` and `ssh devbox` from the same client, the same key and the same `~/.ssh/config.d` layout.

The two server sides are independent:
- **moi** owns the main server's path: the tunnel connector, its ingress rule for the SSH host, and the host's sshd.
- **devbox** owns the VM's path: its gateway's own SSH server behind the reverse proxy. Under `moi devbox`, that path is embedded like the rest of devbox (15).

Neither changes the other's path. moi never routes SSH through the reverse proxy or the devbox gateway, and devbox never touches cloudflared or the host's sshd.

## Commands

```
moi ssh up [--hostname ssh.example.com] [--dry-run] [--yes]   # set up or adopt the tunnel path; idempotent
moi ssh status [--json]                                       # connector, ingress, DNS, public handshake, keys
moi ssh connect                                               # print the one command to run on a computer
moi ssh authorize <key line|file> [--name N]                  # allow a computer's key (the owner's authorized_keys)
moi ssh confirm                                               # keep a change made over the tunnel (§ Safety)
moi ssh down [--dry-run]                                      # restore what `up` replaced
```

All of them work under `MOI_ROOT` fixtures, with the Cloudflare API faked.

## Providers

`[ssh] provider` selects a `ssh::Provider` (a trait with `plan`, `apply`, `status`, `down`), so another way in (Tailscale, a plain public port) can be added without touching the rest. v1 ships `cloudflare`. `none` means the server is already reachable, so `moi ssh up` only does the parts that aren't provider-specific: checks, records and the client command.

## `moi ssh up` (provider `cloudflare`)

A **plan → confirm → apply** run of idempotent steps, like `moi devbox up`. Anything replaced is archived under `<state>/archive/ssh/<time>/` for `down`. Re-running with nothing to change prints `everything already up to date`.

| # | Step | Detail |
|---|---|---|
| 1 | Find the tunnel | in order: `[ssh] tunnel_token`; a running cloudflared **container** (its `TUNNEL_TOKEN` environment or `--token` argument, read through the Docker API); a cloudflared **systemd service** (its unit's `--token`, or the file its `--token-file` names — where `cloudflared service install` keeps the token since 2026.7.2 — or a locally managed `config.yml` with `tunnel:` and `credentials-file:`). The token's payload (base64 JSON `{a, t, s}`) gives the account and tunnel ids. A token found on the machine is stored as `secret:cloudflare_tunnel_token` when `[ssh]` is written. No tunnel at all → stop and say how to create one in the Cloudflare dashboard (moi never creates tunnels) |
| 2 | Credentials | `[ssh] api_token` (a `secret:` reference) is required for remotely managed tunnels and for DNS. Its permissions are checked up front with read calls; when one is missing, moi names it: **Account → Cloudflare Tunnel → Edit** and **Zone → DNS → Edit** for the zone of the hostname |
| 3 | Connector on the host | if the `cloudflared` binary is missing: on apt systems, add Cloudflare's package repository with its signing key pinned by fingerprint (14) and `apt-get install cloudflared`; elsewhere, stop and say how to install it. Then `cloudflared service install <token>` (cloudflared writes its own unit) unless a host service for this tunnel already runs, plus moi's drop-in `/etc/systemd/system/cloudflared.service.d/moi.conf`, which adds `--metrics 127.0.0.1:<metrics port>` (default 20241). Wait ≤ 60 s for `/ready` to answer 200 with at least one edge connection |
| 4 | Retire the container connector | a cloudflared container serving the same tunnel is stopped through the Docker API and its restart policy set to `no` (recorded for `down`). A container that belongs to an orchestrator (Dokploy/Compose project labels, a swarm service) is **not** stopped, because the orchestrator would bring it back. moi says where to stop it and continues; `status` reports the second connector until it's gone. Nothing is deleted |
| 5 | Ingress rule | `{hostname: <ssh hostname>, service: ssh://localhost:22}`. Remotely managed tunnels go through the API (`GET`/`PUT /accounts/{a}/cfd_tunnel/{t}/configurations`); locally managed ones edit `config.yml` and restart the service. Rules are keyed by hostname: moi adds or updates only its own, keeps every other rule in its order, and keeps the catch-all last. An existing rule for the hostname is adopted, and only its `service` is changed when needed |
| 6 | DNS | a proxied `CNAME <hostname> → <tunnel id>.cfargotunnel.com`. An existing record already pointing there is adopted; a record pointing elsewhere stops the run and is shown (moi never overwrites someone else's DNS) |
| 7 | sshd | read-only checks: `sshd -T` shows `passwordauthentication no` and `permitrootlogin` not `yes`. Otherwise it's an audit Attention entry (04), not a change: moi doesn't edit sshd |
| 8 | Verify | through the public path: a WebSocket handshake to `https://<hostname>/` answered with `101` and a first binary message starting with `SSH-2.0-`. The `Sec-WebSocket-Accept` value is not checked, which avoids a SHA-1 dependency; the probe only needs the 101 and the banner |
| 9 | Records | write `[ssh]` to `moi.toml` (a marked block, like `moi init`), snapshot, CONTEXT.md, the `ssh` check and its monitor (§ Checks) |

Order matters on a live server. Steps 3–5 run in this order so that during the switch at least one connector can reach the origin the ingress names. For example, an old container rule `ssh://172.17.0.1:22` also works from the host until step 5 changes it to `localhost`.

### Safety

Running `moi ssh up` (or `down`) **through the tunnel it changes** must not lock the owner out. moi treats it like `moi firewall apply` over SSH (10):
- When `SSH_CONNECTION` is set and the ingress for the hostname already exists, moi first arms `systemd-run --unit=moi-ssh-rollback --on-active=5min moi ssh down --rollback <archive>`.
- It runs steps 3–6 in a transient unit (`systemd-run --unit=moi-ssh-apply --wait`), so a dropped session doesn't stop it halfway.
- It then asks the owner to open a **new** connection and run `moi ssh confirm` within 5 minutes. Confirming stops the rollback timer; otherwise everything is put back.
- If the timer can't be scheduled, nothing is changed.

## `moi ssh connect`

Prints the one command for a computer, which is golden-tested:

```
curl -fsSL https://github.com/prabhask5/devbox/releases/download/ssh-v0.1.0/devbox-ssh-setup.sh | sh -s -- add \
  --alias cestmoi --url https://ssh.example.com --user owner \
  --host-key 'ssh-ed25519 AAAA…' --authorize-hint 'On the server, run: sudo moi ssh authorize'
```

- `--alias`: `[ssh] alias` (default `server.name`).
- `--user`: `[ssh] user` (default the user who ran `sudo`, from `SUDO_USER`).
- `--host-key`: `/etc/ssh/ssh_host_ed25519_key.pub`, so the computer pins the key without trust-on-first-use.
- The release tag is `[ssh] client_release`, which defaults to the devbox client release this moi version was tested with.

Below the command, moi says that `devbox-ssh` and the script come from devbox's release, and that running the same script with `remove --alias <alias>` undoes it. When the computer's key isn't authorized yet, the script's test connection fails and prints the hint with the key, so the owner can run `moi ssh authorize`.

`moi ssh authorize` appends the key, with `--name` as its comment, to `~<user>/.ssh/authorized_keys` (created 0600 owned by the user, directory 0700). A key that's already there changes nothing. It validates the line first: same types and RSA minimum as devbox's pairing.

## Configuration

```toml
[ssh]                            # on when present; written by `moi ssh up`
provider = "cloudflare"          # cloudflare | none
hostname = "ssh.example.com"
alias = "cestmoi"                # default: server.name
user = "owner"                   # the login the printed command uses
api_token = "secret:cloudflare_api_token"
tunnel_token = "secret:cloudflare_tunnel_token"   # optional when the tunnel can be found on the machine
metrics = "127.0.0.1:20241"
client_release = "ssh-v0.1.0"    # devbox release the printed command uses
monitor = "SSH tunnel"           # push monitor for the `ssh` check; "" for none
```

## Checks (05)

A built-in check `ssh`, on when `[ssh]` is present, runs every `checks.default_interval` and is pushed to `ssh.monitor`. Messages (stable, golden):

| State | Message |
|---|---|
| ok | `SSH tunnel up: {n} edge connections ({hostname})` |
| ok, with a public probe | `SSH tunnel up: {n} edge connections ({hostname}), handshake {ms} ms` |
| down | `SSH tunnel down: cloudflared not running` · `SSH tunnel down: no edge connections` · `SSH tunnel down: metrics not answering ({error})` · `SSH tunnel down: public handshake failed ({stage}: {error})` |

The public probe (step 8's handshake) runs at most every 15 minutes; the metrics check runs every interval. The check never changes anything (no heal rule by default; a `[[heal]]` rule may restart `cloudflared`).

## Records

- **Snapshot (02):** an `ssh` collector records the connector (unit state, version, metrics address), the ingress rules (from the local config, or from the API at most once a day and cached), the DNS record, `sshd -T` key settings, and the authorized keys' fingerprints and comments. Keys are never recorded in full.
- **Context (03):** an "SSH access" block. It lists the main server path (`ssh <alias>` via `<hostname>`), the devbox path when devbox is present (`ssh devbox` via devbox's SSH host, from its host config), and the rescue line for the VM: `ssh <alias>`, then `incus exec <instance> -- su - <user>`.
- **Audit (04):** `/etc/cloudflared` and moi's drop-in are identity data (already in `restore.etc_paths`). An sshd that allows passwords or root logins while a public SSH hostname exists is an Attention entry.
- **Monitors (06):** a push monitor row for the `ssh` check (default name `SSH tunnel`, interval = check interval + 20 %, retries 1), adopted by name.
- **Restore (09):** phase `ssh` after `config`: re-runs `moi ssh up --yes` from `[ssh]` and the secrets (the connector reinstalled, ingress and DNS confirmed unchanged), then verifies the public handshake.

## Security (14)

- The SSH hostname is public. Protection is sshd's own public-key authentication, plus Cloudflare Access if the owner adds a policy (the client then needs a service token: `--header` lines in the script's `add`).
- `api_token` is a secret reference with the two permissions above and nothing else. `tunnel_token` is a secret.
- Cloudflare's apt repository key is pinned by fingerprint in moi's source. A different key stops the install.
- The printed command carries only public values: hostname, user, host key.
- No new dependencies: the API and the probe use `net` (HTTPS, the WebSocket upgrade, one frame read), and secrets go through `redact`.

## Decisions (M14)

- **What counts as orchestrator-managed** (step 4): a swarm service's task (`com.docker.swarm.service.name`) and a Dokploy stack (a Compose working directory under `/etc/dokploy`, or `dokploy.*` labels) — something that reconciles and would start a stopped container again. A *plain* Compose project has nobody watching it, so its container is retired (this is cestmoi's `cloudflared` stack). Because moi's own `boot.compose_up` would be that somebody, the stack's directory is remembered (`ssh.retired_stacks` in `moi.db`) and the boot job leaves it down; `moi ssh down` forgets it again.
- **The metrics address** is set through `Environment=TUNNEL_METRICS=<address>` in moi's drop-in — the variable cloudflared reads for `--metrics` — so the unit `cloudflared service install` wrote is not overridden and survives cloudflared's own updates of it.
- **"Pinned by fingerprint"** is the SHA-256 of the key file Cloudflare publishes at `https://pkg.cloudflare.com/cloudflare-main.gpg` (`ssh::apply::KEY_SHA256`), not the OpenPGP fingerprint, which would need SHA-1 and a packet parser. Any other file stops the install before apt is touched. If Cloudflare rotates the key, the owner installs cloudflared by hand and moi goes on from there; the constant changes with the next release.
- **Before `[ssh]` exists**, `moi ssh up` reads the secrets `cloudflare_api_token` and `cloudflare_tunnel_token` from `secrets.toml` (or `MOI_SECRET_*`) by name. A token found on the machine is appended to `secrets.toml` under that name.
- **The `[ssh]` block** in `moi.toml` is appended between `# >>> moi ssh up …` and `# <<< moi ssh up`; `down` removes exactly that. An `[ssh]` section the owner wrote by hand is never rewritten: a different `--hostname` stops with a message.
- **A cloudflared unit of another tunnel** is not replaced: the run stops and says so.
- **Plan output**: one line per step — `ok`, `adopt`, `change`, `note` or `STOP`, the step's name, what it is about. A `STOP` ends the plan (exit 1, nothing changed). `--dry-run --json`: `{ hostname, changes, steps: [{name, action, text}] }`.
- **Order of the apply**: connector installed and *connected* (an edge connection on `/ready`, 60 s) before the old container is stopped or the rule is touched; if it does not connect, the run ends there and what worked still works.
- **Over the tunnel** means: `SSH_CONNECTION` is set, a rule for the hostname exists, and the plan touches the connector, the container or the rule. A first setup over a plain SSH port needs no timer. The outer process arms `moi-ssh-rollback` (`moi ssh down --rollback <archive>`), runs `moi ssh up --inner <archive>` in `moi-ssh-apply`, then writes the records and verifies; the pending change is remembered in `moi.db` (`ssh.pending`) and shown by `moi ssh status`. If the inner run fails, everything is put back at once and the timer is stopped.
- **`down`** restores in the order that keeps a way in: the old container first, then the rule (its previous form, or removed when moi added it), the DNS record only if moi created it, the drop-in, the service only if moi installed it. The cloudflared package and apt repository stay (said, not removed). Adopted rules and records are never touched.
- **The `ssh` check** exists for the `cloudflare` provider only. Between two looks from outside (15 minutes), a failed look is remembered and keeps the check down even though the connector itself is connected. A run by hand (`moi check ssh`) always looks from outside.
- **Snapshot**: `snapshot/ssh/access.json` (hostname, alias, user, provider, tunnel id and where it was found, connector unit/version/metrics, ingress as hostname → service, DNS, the five sshd settings, authorized keys as fingerprint + comment) and `volatile/ssh-connector.txt` (the unit's state). Cloudflare is asked at most once a day (`ssh.cloudflare_cache`).
- **Restore** (`ssh` phase): uses the *bundle's* `[ssh]` and secrets, applies only the provider's steps (the records come back with the `moi` phase), and verifies the handshake.
- **Key validation in `authorize`**: Ed25519, ECDSA P-256/384/521, their security-key forms, RSA from 3072 bits; the key's inner type must match the one written in front of it.

## Acceptance

- [x] Fake Cloudflare API (in-process HTTP, `tests/fakes/cloudflare`): tunnel lookup, configurations GET/PUT, DNS list/create; permission errors → the named missing permission.
- [x] Tunnel discovery from a container token, a systemd unit token, a local `config.yml`, and the config secret (fixtures); no tunnel → the actionable message.
- [x] `up` plans golden for: fresh host with a tunnel, cestmoi (container connector with an existing `ssh.` rule; the plan adopts the rule and retires the container), orchestrated container (not stopped, reported), already set up (`everything already up to date`).
- [x] Ingress merge: other rules and their order kept, catch-all last, existing rule adopted, only `service` changed; DNS pointing elsewhere refused.
- [x] Over SSH: rollback timer armed before any change, apply in a transient unit, `confirm` keeps, no confirm restores (connector, container, ingress), timer unavailable → nothing changed.
- [x] `connect` output golden; `authorize` appends once, validates, sets modes and ownership.
- [x] `ssh` check messages golden for each state; monitor row adoption; snapshot, context and audit entries golden on the cestmoi fixture.
- [x] `down` restores byte for byte what `up` changed (files, unit drop-in, container restart policy, ingress, DNS it created), and never deletes the tunnel or records it didn't create.
- [x] Restore phase `ssh` on a fixture rebuild.
- [ ] With devbox's `ssh-v0.1.0` published: the printed command run in CI against a fake HOME and a fake tunnel origin (a WebSocket server bridging to a test sshd) connects with the real OpenSSH client.
