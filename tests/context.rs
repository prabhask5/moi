//! CONTEXT.md (docs/spec/03-context.md): content, redaction, notes placement, copies and the Claude Code
//! pointer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::config::{self, Paths};
use cestmoi::context;
use cestmoi::docker::Docker;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::state::StateDir;

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    state: StateDir,
}

fn server(test: &str) -> Server {
    let overlay: PathBuf = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture, state: StateDir::new("/var/lib/moi") }
}

impl Server {
    fn moi(&self, args: &[&str]) -> Result<String, String> {
        let mut out = Vec::new();
        let result = cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out);
        let text = String::from_utf8(out).unwrap();
        match result {
            Ok(()) => Ok(text),
            // "problems found" is an exit code, not a failure to run
            Err(e) if e.exit_code() == 1 => Ok(text),
            Err(e) => Err(e.to_string()),
        }
    }

    fn context(&self) -> String {
        self.host.read_to_string(&self.state.context()).unwrap()
    }

    /// Regenerate and return the context file.
    fn regenerate(&self) -> String {
        self.moi(&["context"]).unwrap();
        self.context()
    }

    fn secrets(&self) -> Vec<String> {
        std::fs::read_to_string(common::fixture_dir("cestmoi").join("secrets"))
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect()
    }
}

fn section<'a>(doc: &'a str, heading: &str) -> &'a str {
    let start = doc.find(&format!("\n{heading}\n")).unwrap_or_else(|| panic!("no section {heading}"));
    let level = heading.chars().take_while(|c| *c == '#').count();
    let body = &doc[start + 1..];
    let end = body[heading.len()..]
        .match_indices("\n#")
        .find(|(i, _)| {
            let rest = &body[heading.len() + i + 1..];
            let l = rest.chars().take_while(|c| *c == '#').count();
            l <= level && rest[l..].starts_with(' ')
        })
        .map_or(body.len(), |(i, _)| heading.len() + i);
    &body[..end]
}

#[test]
fn golden_context_of_the_cestmoi_fixture() {
    let s = server("ctx-golden");
    s.moi(&["audit", "--quiet"]).unwrap();
    let doc = s.context().replace(&format!("Version {} ·", env!("CARGO_PKG_VERSION")), "Version <version> ·");
    let expected = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/cestmoi/CONTEXT.md");
    common::assert_golden(&expected, &doc);
    assert!(doc.len() < 120 * 1024, "{} bytes: the file must stay small enough to paste into a chat", doc.len());
    assert_eq!(s.host.metadata(&s.state.context()).unwrap().mode, 0o600);
}

#[test]
fn sections_come_in_the_specified_order() {
    let s = server("ctx-order");
    s.moi(&["audit", "--quiet"]).unwrap();
    let doc = s.context();
    let headings: Vec<&str> = doc.lines().filter(|l| l.starts_with("# ") || l.starts_with("## ")).collect();
    assert_eq!(
        headings,
        [
            "# Server context: cestmoi",
            "## How to use this document (for AI assistants)",
            "## ⚠ Attention",
            "## Owner notes",
            "## System",
            "## Storage",
            "## Network",
            "## Domains / reverse-proxy routes",
            "## Docker",
            "## VMs & system containers (Incus)",
            "## Monitoring & alerting (existing: adopt, never recreate)",
            "## moi",
            "## Firewall & access control",
            "## Scheduled jobs",
            "## Services (systemd)",
            "## Scripts",
            "## Installed software",
            "## Users & access",
            "## Backups & rebuild",
            "## Coverage & reboot audit — cestmoi",
            "## Recent changes (newest first)",
        ]
    );
    assert!(doc.contains("**health checks, backups and monitors in `/etc/moi/moi.toml`**"));
}

#[test]
fn no_secret_planted_in_the_fixture_reaches_the_context_file_or_its_copies() {
    let s = server("ctx-no-leak");
    s.moi(&["audit", "--quiet"]).unwrap();
    let doc = s.context();
    let copy = s.host.read_to_string(Path::new("/home/prabhas/SERVER_CONTEXT.md")).unwrap();
    assert_eq!(copy, doc);
    let secrets = s.secrets();
    assert!(secrets.len() > 35, "{}", secrets.len());
    for secret in &secrets {
        assert!(
            !doc.contains(secret.as_str()),
            "{secret} leaked:\n{}",
            doc.lines().filter(|l| l.contains(secret.as_str())).collect::<Vec<_>>().join("\n")
        );
    }
    // what stands in their place
    assert!(
        doc.contains("`` POSTGRES_PASSWORD=*** ``")
            && doc.contains("postgres://outline:***@outline-postgres:5432/outline")
    );
    assert!(doc.contains("| Server Resources | push | push …ok01 |"), "push tokens keep their last four characters");
    assert!(doc.contains("| healthchecks.io watchdog | http | https://hc-ping.com/… (key hidden) |"));
    assert!(doc.contains("`` UPTIMEKUMA=http://uptime-kuma:3001/api/push/…sh01 ``"));
    assert!(!doc.contains("AAAAC3NzaC1lZDI1NTE5"), "public keys are listed by their comment only");
    assert!(doc.contains("| prabhas | 1000 | /home/prabhas | /bin/bash | sudo, docker, incus-admin | 1: laptop |"));
}

#[test]
fn the_attention_list_gathers_what_needs_a_look() {
    let s = server("ctx-attention");
    s.moi(&["audit", "--quiet"]).unwrap();
    let attention: Vec<String> = section(&s.context(), "## ⚠ Attention")
        .lines()
        .filter_map(|l| l.strip_prefix("- ⚠ "))
        .map(str::to_string)
        .collect();
    assert_eq!(
        attention,
        [
            "/opt/scripts/backup.sh.bak is a stale copy that still holds a push token (…pTok): delete it",
            "Kuma monitor \"Server Backup\" last received a malformed message ([object Object]): its feeder sends an object where text belongs",
            "audit: 1 custom file(s) in /etc that no package owns and no restore phase puts back — add the ones that matter to `restore.etc_paths`:",
            "audit: 2 large directories are outside the backup (see the table)",
            "audit: container supabase-pooler has 350 MB written inside the container itself (not in a volume): lost when it is recreated",
            "audit: not in the backup: `/var/lib/private-app`",
            "audit: supabase-db `/etc/postgresql-custom` ← `/mnt/storage/database/db-config`: database/config on HDD",
        ]
    );

    // more trouble: a full disk, a stopped container that should run, a failed unit, a service short of replicas,
    // password logins, a script pushing to a token nobody has (the monitor it used to feed is still fed by
    // moi's own check, so that one is not orphaned)
    let df = "df -hT -x tmpfs -x devtmpfs -x overlay -x squashfs -x efivarfs";
    s.fixture.add_command(
        df,
        0,
        "Filesystem Type Size Used Avail Use% Mounted on\n/dev/sda1 ext4 3.6T 3.2T 0.4T 91% /mnt/storage\n",
    );
    s.fixture.add_command(
        "systemctl list-units --failed --no-legend --no-pager --plain",
        0,
        "server-backup.service loaded failed failed Server backup\n",
    );
    let docker = Docker::new(s.host.clone());
    let mut list = docker.containers(true).unwrap();
    list.retain(|c| !c["Names"][0].as_str().unwrap().starts_with("/stellar-planner-x7k2."));
    s.fixture.set_docker("/containers/json", &serde_json::Value::Array(list).to_string());
    let mut sonarr = docker.container("sonarr").unwrap();
    sonarr["State"]["Status"] = serde_json::json!("exited");
    s.fixture.set_docker(&format!("/containers/{}/json", sonarr["Id"].as_str().unwrap()), &sonarr.to_string());
    s.host.write(Path::new("/etc/ssh/sshd_config"), b"PasswordAuthentication yes\n", 0o644).unwrap();
    s.host.remove_file(Path::new("/etc/ssh/sshd_config.d/50-cloud-init.conf")).ok();
    s.host.write(Path::new("/etc/ssh/sshd_config.d/50-cloud-init.conf"), b"# emptied\n", 0o644).unwrap();
    s.host
        .write(
            Path::new("/opt/scripts/storage-check.sh"),
            b"#!/bin/bash\ncurl http://uptime.home/api/push/FAKEunknownTok99\n",
            0o755,
        )
        .unwrap();
    s.moi(&["snapshot"]).unwrap();
    let doc = s.context();
    for expected in [
        "- ⚠ /mnt/storage is 91% full",
        "- ⚠ container sonarr is exited although its restart policy is unless-stopped",
        "- ⚠ systemd unit failed: server-backup.service",
        "- ⚠ swarm service stellar-planner-x7k2 has 0 of 1 replicas running",
        "- ⚠ SSH password authentication is enabled (or not explicitly disabled)",
        "- ⚠ /opt/scripts/storage-check.sh pushes to a Kuma token (…ok99) that matches no monitor",
    ] {
        assert!(doc.contains(expected), "{expected}\nnot in:\n{}", section(&doc, "## ⚠ Attention"));
    }
}

#[test]
fn a_description_that_stopped_being_refreshed_says_so() {
    let s = server("ctx-stale");
    s.moi(&["snapshot"]).unwrap();
    assert!(!s.context().contains("days old"));
    s.fixture.advance(std::time::Duration::from_secs(5 * 86_400));
    let loaded = config::load(s.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let doc = context::generate(s.host.as_ref(), &loaded.config, &s.state, &loaded.masker);
    assert!(
        doc.contains("- ⚠ this description is 5 days old: the last snapshot was taken at 2026-11-26T17:00:00Z"),
        "{}",
        section(&doc, "## ⚠ Attention")
    );
}

#[test]
fn notes_appear_next_to_what_they_describe_and_leftovers_at_the_end() {
    let s = server("ctx-notes");
    s.moi(&["snapshot"]).unwrap();
    let notes = s.host.read_to_string(&s.state.notes()).unwrap();
    let replace_stub = |text: String, heading: &str, body: &str| -> String {
        let start = text.find(&format!("## {heading}\n")).unwrap_or_else(|| panic!("{heading}")) + heading.len() + 4;
        let end = text[start..].find('\n').unwrap() + start;
        format!("{}{body}{}", &text[..start], &text[end..])
    };
    let mut edited =
        replace_stub(notes, "Overview", "Home server in the closet.\nServes the family; reached at `*.example.com`.");
    edited = replace_stub(
        edited,
        "Stack: yams",
        "Media stack. qBittorrent and SABnzbd share gluetun's network: restart gluetun first.",
    );
    edited = replace_stub(edited, "Container: dokploy-traefik", "The edge proxy for everything.");
    edited = replace_stub(edited, "Service: stellar-planner-x7k2", "Side project, SQLite in the volume.");
    edited = replace_stub(edited, "Instance: devbox", "Dev VM, rebuilt from dotfiles.");
    edited = replace_stub(edited, "Unit: host-firewall.service", "Applies INPUT and DOCKER-USER only.");
    edited = replace_stub(edited, "Script: /opt/scripts/backup.sh", "Nightly backup: dumps, tar, then off-site.");
    edited.push_str(
        "\n## Power\nUPS on the left, 20 minutes.\n\n## Stack: wiki\nRemoved in 2025; data archived on the NAS.\n",
    );
    s.host.write(&s.state.notes(), edited.as_bytes(), 0o600).unwrap();
    let doc = s.regenerate();

    let owner = section(&doc, "## Owner notes");
    assert!(
        owner.contains("### Overview\nHome server in the closet.\nServes the family; reached at `*.example.com`."),
        "{owner}"
    );
    assert!(owner.contains("### Power\nUPS on the left, 20 minutes."), "the owner's own headings are general notes");
    assert!(!owner.contains("Gotchas"), "sections still holding their placeholder are not shown");

    let yams = section(&doc, "#### Stack `yams`");
    assert!(
        yams.contains(
            "\n> Media stack. qBittorrent and SABnzbd share gluetun's network: restart gluetun first.\n- **bazarr**"
        ),
        "{}",
        &yams[..600]
    );
    assert!(section(&doc, "## Docker").contains("restart: always\n  - ports: 443→443/tcp, 80→80/tcp"));
    assert!(doc.contains("> The edge proxy for everything."));
    assert!(doc.contains("- **stellar-planner-x7k2** — env: `` NODE_ENV=production ``, `` SESSION_SECRET=*** ``\n> Side project, SQLite in the volume."));
    assert!(doc.contains("- **devbox** (limits.cpu: 4, limits.memory: 6GiB) — snapshots: base, before-upgrade, snap0\n> Dev VM, rebuilt from dotfiles."));
    assert!(doc.contains("  - ExecStart: `/usr/local/sbin/host-firewall apply /etc/host-firewall/rules4.txt /etc/host-firewall/rules6.txt`\n> Applies INPUT and DOCKER-USER only."));
    // a script's note replaces the description taken from its header comment
    assert!(doc.contains(
        "| /opt/scripts/backup.sh | Nightly backup: dumps, tar, then off-site. | server-backup.service | docker | 90 |"
    ));

    let leftovers = section(&doc, "## Notes about things no longer present");
    assert_eq!(
        leftovers,
        "## Notes about things no longer present\n\n### Stack: wiki\nRemoved in 2025; data archived on the NAS.\n"
    );
    assert!(doc.ends_with("Removed in 2025; data archived on the NAS.\n"));
}

#[test]
fn what_the_document_says_about_the_server() {
    let s = server("ctx-content");
    s.moi(&["audit", "--quiet"]).unwrap();
    let doc = s.context();
    let routes = section(&doc, "## Domains / reverse-proxy routes");
    for expected in [
        "| notes.example.com | outline | traefik label (router outline) |",
        "| media.example.com | jellyfin | traefik label (router jellyfin-public) |",
        "| tv.example.com | jellyfin | traefik label (router jellyfin-public) |",
        "| stellar.example.com | service stellar-planner-x7k2 | traefik label (router stellar-planner-x7k2) |",
        "| dokploy.home |  | traefik file /etc/dokploy/traefik/dynamic/dokploy.yml |",
        "| status.example.com | http://uptime-kuma:3001 | cloudflared tunnel /opt/services/cloudflared/config.yml |",
    ] {
        assert!(routes.contains(expected), "{expected}\n{routes}");
    }
    let docker = section(&doc, "## Docker");
    assert!(docker.starts_with(
        "## Docker\n35 containers (35 running), 14 compose stacks, 7 swarm services, 10 networks, 4 named volumes.\n"
    ));
    assert!(docker.contains("| homelab | bridge | 172.19.0.0/16 gw 172.19.0.1 |  | manual / external | "));
    assert!(
        docker.contains(
            "| stellar-data | local | svc:stellar-planner-x7k2 | /var/lib/docker/volumes/stellar-data/_data |"
        ),
        "volumes show the swarm services using them"
    );
    assert!(docker.contains("- **qbittorrent** — `lscr.io/linuxserver/qbittorrent:5.1.4` · running · restart: unless-stopped\n  - network_mode: container:gluetun"));
    assert!(docker.contains("#### Stack `old-wiki`\n- dir: `/opt/services/old-wiki` · files: `/opt/services/old-wiki/docker-compose.yml` · source: found on disk (no containers)\n- services defined: wiki, wiki-db\n"));
    assert!(docker.contains("  - cap_add NET_ADMIN · devices /dev/net/tun · healthcheck"));
    assert!(!docker.contains("PATH=/usr/local/sbin"), "environment set by the image is noise");

    let moi = section(&doc, "## moi");
    assert!(moi.contains("| resources | 5m | Server Resources | resource-check |"));
    assert!(moi.contains("- **Self-healing rules**: `traefik-homelab`, `traefik-config-guard`, `vpn-stack`."));
    let monitoring = section(&doc, "## Monitoring & alerting (existing: adopt, never recreate)");
    assert!(monitoring.contains("| Server Storage | push | push …0003 | 360 | yes |  | Email | /opt/scripts/storage-check.sh, moi check `storage` |"));
    assert!(monitoring.contains("| devbox | ping | 10.143.9.20 | 60 | paused |"));
    assert!(
        monitoring
            .contains("| /opt/scripts/backup.sh.bak | uptime-kuma-push | …pTok | (not scheduled by cron or systemd) |")
    );
    assert!(monitoring.contains("**Watchdog:** healthchecks.io watchdog → https://hc-ping.com/… (key hidden)"));

    assert!(
        section(&doc, "## Firewall & access control")
            .contains("-A DOCKER-USER -j DROP\n```\n_Custom DOCKER-USER rules exist")
    );
    assert!(section(&doc, "## Firewall & access control").contains("**sshd:** passwordauthentication=no"));
    assert!(
        section(&doc, "## Scheduled jobs")
            .contains("| server-backup.timer | custom | OnCalendar=03:30 | /opt/scripts/backup.sh |")
    );
    assert!(
        section(&doc, "## Scheduled jobs")
            .contains("| crontab -u prabhas | 0 9 * * 1 | prabhas | /home/prabhas/bin/weekly-note.sh |")
    );
    assert!(section(&doc, "## Services (systemd)").contains("- **traefik-config-guard.path** (enabled) — Watch traefik.yml\n  - PathChanged: `/etc/dokploy/traefik/traefik.yml`"));
    assert!(
        section(&doc, "## Installed software").contains("**Binaries installed by hand in /usr/local/(s)bin:** ctop")
    );
    let backups = section(&doc, "## Backups & rebuild");
    assert!(backups.contains("- **Databases dumped:** bookorbit-db, outline-postgres, supabase-db, dokploy-postgres"));
    assert!(backups.contains(
        "| /mnt/storage/media | bind mount → jellyfin:/data | fungible (media library — temporary, re-downloadable) |"
    ));
    assert!(backups.contains("| /opt/services/outline/pgdata | bind mount → outline-postgres:/var/lib/postgresql | ✓ dump of outline-postgres |"));
    assert!(backups.contains(
        "| /var/lib/docker/volumes/stellar-data/_data | volume stellar-data → service stellar-planner-x7k2 | ✓ |"
    ));
    assert!(doc.contains("\n### Would it survive a reboot?\n"), "the audit is embedded one heading level down");
    assert!(section(&doc, "## Recent changes (newest first)").contains("initial snapshot of cestmoi"));
}

#[test]
fn copies_belong_to_the_owner_of_their_directory_and_claude_code_gets_a_pointer() {
    let s = server("ctx-copies");
    s.moi(&["snapshot"]).unwrap();
    let copy = Path::new("/home/prabhas/SERVER_CONTEXT.md");
    let md = s.host.metadata(copy).unwrap();
    assert_eq!((md.mode, md.uid, md.gid), (0o644, 1000, 1000));

    let pointer = Path::new("/home/prabhas/.claude/CLAUDE.md");
    let text = s.host.read_to_string(pointer).unwrap();
    assert!(text.starts_with("<!-- moi:begin -->\n## This server"), "{text}");
    assert!(text.contains("read `/home/prabhas/SERVER_CONTEXT.md`") && text.ends_with("<!-- moi:end -->\n"));
    assert_eq!(s.host.metadata(pointer).unwrap().uid, 1000);
    assert_eq!(s.host.metadata(Path::new("/home/prabhas/.claude")).unwrap().uid, 1000);
    // root reads the original in the state dir
    assert!(
        s.host
            .read_to_string(Path::new("/root/.claude/CLAUDE.md"))
            .unwrap()
            .contains("read `/var/lib/moi/state/CONTEXT.md`")
    );

    // the owner's own instructions around the block are never touched, and the block is updated in place
    let own = format!(
        "# My rules\nAlways answer in French.\n\n{}\n## More\nUse tabs.\n",
        text.replace("SERVER_CONTEXT.md", "OLD.md")
    );
    s.host.write(pointer, own.as_bytes(), 0o644).unwrap();
    for _ in 0..3 {
        s.regenerate();
    }
    let after = s.host.read_to_string(pointer).unwrap();
    assert_eq!(after, own.replace("OLD.md", "SERVER_CONTEXT.md"));

    // uninstall: exactly the block goes; a file that only held the block goes with it
    assert!(context::remove_pointer(s.host.as_ref(), 1000).unwrap());
    assert_eq!(
        s.host.read_to_string(pointer).unwrap(),
        "# My rules\nAlways answer in French.\n\n\n## More\nUse tabs.\n"
    );
    assert!(!context::remove_pointer(s.host.as_ref(), 1000).unwrap());
    assert!(context::remove_pointer(s.host.as_ref(), 0).unwrap());
    assert!(!s.host.exists(Path::new("/root/.claude/CLAUDE.md")));
}

#[test]
fn pointer_and_copies_can_be_switched_off_and_a_bad_copy_path_is_only_a_warning() {
    let s = server("ctx-copies-off");
    let path = Path::new("/etc/moi/moi.toml");
    let text = s.host.read_to_string(path).unwrap();
    let text = text.replace("claude_pointer = true", "claude_pointer = false").replace(
        "copies = [\"/home/prabhas/SERVER_CONTEXT.md\"]",
        "copies = [\"/home/nobody/CONTEXT.md\", \"/home/prabhas/CTX.md\"]",
    );
    s.host.write(path, text.as_bytes(), 0o644).unwrap();
    let out = s.moi(&["context", "--json"]).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["context"]["copies"], serde_json::json!(["/home/prabhas/CTX.md"]));
    assert!(v["context"]["failed_copies"][0].as_str().unwrap().starts_with("/home/nobody/CONTEXT.md: "), "{v}");
    assert!(!s.host.exists(Path::new("/home/nobody")), "a missing directory is not created for a copy");
    assert!(!s.host.exists(Path::new("/home/prabhas/.claude")) && !s.host.exists(Path::new("/root/.claude")));
}

#[test]
fn moi_context_reports_what_it_wrote_and_every_snapshot_refreshes_the_file() {
    let s = server("ctx-cli");
    // no snapshot yet: `moi context` takes one
    let out = s.moi(&["context"]).unwrap();
    assert!(out.starts_with("/var/lib/moi/state/CONTEXT.md written ("), "{out}");
    assert!(out.contains(" KB, 2 items need attention)\ncopy: /home/prabhas/SERVER_CONTEXT.md\n"), "{out}");
    assert!(s.host.exists(&s.state.snapshot().join("meta/hostname")));

    // a change shows up in the context file after the next snapshot, without asking
    s.host
        .write(Path::new("/etc/cron.d/nightly-report"), b"15 4 * * * root /opt/scripts/backup.sh --report\n", 0o644)
        .unwrap();
    s.moi(&["snapshot"]).unwrap();
    let doc = s.context();
    assert!(doc.contains("| /etc/cron.d/nightly-report | 15 4 * * * | root | /opt/scripts/backup.sh --report |"));
    assert!(section(&doc, "## Recent changes (newest first)").contains("/etc/cron.d/nightly-report"));
    // the context file is generated, not versioned
    assert_eq!(s.moi(&["log"]).unwrap().lines().count(), 2);
}

#[test]
fn a_server_with_nothing_on_it_still_gets_a_useful_file() {
    let overlay = common::scratch("ctx-minimal");
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_overlay(&overlay).unwrap());
    let host: Arc<dyn Host> = fixture;
    let mut out = Vec::new();
    cli::execute(&cli::parse(&["context"]).unwrap(), host.clone(), &mut out).unwrap();
    let doc = host.read_to_string(Path::new("/var/lib/moi/state/CONTEXT.md")).unwrap();
    assert!(doc.starts_with("# Server context: myserver.lan\n"));
    for absent in ["## Docker", "## VMs", "## Monitoring", "## Scripts", "## Domains"] {
        assert!(!doc.contains(absent), "{absent}");
    }
    assert!(doc.contains("- Health checks: none configured.") && doc.contains("**No backup is configured**"));
}
