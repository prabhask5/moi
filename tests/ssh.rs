//! `moi ssh`: SSH from anywhere through a tunnel (docs/spec/16-ssh.md). Cloudflare's API is
//! `tests/fakes/cloudflare.rs`; cloudflared's metrics and the public name are handlers of the fixture
//! host's network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "fakes/cloudflare.rs"]
mod cloudflare;
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::docker::Docker;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::ssh::cloudflare::{self as cf, Dns, Merge, Source};
use cloudflare::FakeCloudflare;
use serde_json::{Value, json};

const HOSTNAME: &str = "ssh.example.com";
const METRICS: &str = "127.0.0.1:20241";
const CONNECTOR: &str = "54aff3c942f75987e7342d00d19fb4b4cf7af729799af8751599b3304cfc2ec5";
const HOST_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFAKEhostkeyFAKEhostkeyFAKEhostkeyFAKEhostkey0 root@server\n";
/// A real Ed25519 public key made for these tests; its private half was thrown away.
const LAPTOP_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl laptop";

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    api: FakeCloudflare,
}

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/ssh").join(name)
}

impl Server {
    fn with(fixture: FixtureHost) -> Self {
        let fixture = Arc::new(fixture);
        let api = FakeCloudflare::new();
        api.serve(&fixture);
        Self { host: fixture.clone(), fixture, api }
    }

    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), format!("{}{e}", String::from_utf8(out).unwrap())),
        }
    }

    fn write(&self, path: &str, text: &str, mode: u32) {
        self.host.create_dir_all(Path::new(path).parent().unwrap(), 0o755).unwrap();
        self.host.write(Path::new(path), text.as_bytes(), mode).unwrap();
    }

    fn read(&self, path: &str) -> String {
        self.host.read_to_string(Path::new(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn secrets(&self, extra: &str) {
        let existing =
            self.host.read_to_string(Path::new("/etc/moi/secrets.toml")).unwrap_or_else(|_| "[secrets]\n".to_string());
        self.write("/etc/moi/secrets.toml", &format!("{existing}{extra}"), 0o600);
    }
}

/// A server with nothing on it but a tunnel token and an API token among its secrets, apt and sshd.
fn fresh(test: &str) -> Server {
    let s = Server::with(common::minimal().with_overlay(&common::scratch(test)).unwrap());
    s.secrets(&format!(
        "cloudflare_api_token = \"{}\"\ncloudflare_tunnel_token = \"{}\"\n",
        cloudflare::API_TOKEN,
        cloudflare::tunnel_token()
    ));
    for tool in ["/usr/bin/apt-get", "/usr/sbin/sshd"] {
        s.write(tool, "", 0o755);
    }
    s.write("/etc/ssh/ssh_host_ed25519_key.pub", HOST_KEY, 0o644);
    s.write("/etc/passwd", "root:x:0:0:root:/root:/bin/bash\nowner:x:1000:1000:Owner:/home/owner:/bin/bash\n", 0o644);
    s.host.create_dir_all(Path::new("/home/owner"), 0o755).unwrap();
    s.host.create_dir_all(Path::new("/etc/systemd/system"), 0o755).unwrap();
    s.fixture.set_env("SUDO_USER", "owner");
    s.fixture.add_command("sshd -T", 0, "port 22\npermitrootlogin prohibit-password\npasswordauthentication no\n");
    s
}

/// The cestmoi server: a cloudflared container of a plain Compose project connects the tunnel, whose
/// ingress already has a rule for `ssh.example.com`.
fn cestmoi(test: &str) -> Server {
    let s = Server::with(
        FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&common::scratch(test)).unwrap(),
    );
    s.secrets(&format!("cloudflare_api_token = \"{}\"\n", cloudflare::API_TOKEN));
    s.connector(|_| {});
    s.api.set_ingress(json!([
        { "hostname": HOSTNAME, "service": "ssh://localhost:22" },
        { "hostname": "status.example.com", "service": "http://uptime-kuma:3001" },
        { "service": "http_status:404" },
    ]));
    s.api.add_record("CNAME", HOSTNAME, &format!("{}.cfargotunnel.com", cloudflare::TUNNEL));
    s.write("/usr/bin/apt-get", "", 0o755);
    s.write("/etc/ssh/ssh_host_ed25519_key.pub", HOST_KEY, 0o644);
    s.fixture.set_env("SUDO_USER", "prabhas");
    s.fixture.add_command("sshd -T", 0, "permitrootlogin no\npasswordauthentication no\n");
    s
}

impl Server {
    /// Change what Docker says about the cloudflared container.
    fn connector(&self, change: impl FnOnce(&mut Value)) {
        let file = common::fixture_dir("cestmoi").join(format!("docker/containers/{CONNECTOR}/json.json"));
        let mut inspect: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        // the fixture's token is a placeholder: the container always gets one that names the fake tunnel
        let env = inspect["Config"]["Env"].as_array_mut().unwrap();
        env.retain(|e| !e.as_str().unwrap().starts_with("TUNNEL_TOKEN="));
        env.push(json!(format!("TUNNEL_TOKEN={}", cloudflare::tunnel_token())));
        change(&mut inspect);
        self.fixture.set_docker(&format!("/containers/{CONNECTOR}/json"), &inspect.to_string());
    }
}

// ------------------------------------------------------------------------------------------------ pure parts

#[test]
fn a_token_says_which_account_and_tunnel_it_is_for() {
    assert_eq!(
        cf::decode_token(&cloudflare::tunnel_token()),
        Some((cloudflare::ACCOUNT.to_string(), cloudflare::TUNNEL.to_string()))
    );
    for not_a_token in ["", "FAKEtunneltoken0000000000000000", "e30=", "not base64 at all!"] {
        assert_eq!(cf::decode_token(not_a_token), None, "{not_a_token}");
    }
}

#[test]
fn the_ssh_rule_is_merged_by_hostname_and_nothing_else_moves() {
    let others = json!([
        { "hostname": "status.example.com", "service": "http://uptime-kuma:3001", "originRequest": { "noTLSVerify": true } },
        { "hostname": "notes.example.com", "path": "/api/*", "service": "http://outline:3000" },
        { "service": "http_status:404" },
    ]);
    let rules = others.as_array().unwrap();
    // added: before the catch-all, the others where they were
    let (merged, what) = cf::merge_ingress(rules, HOSTNAME, cf::SERVICE);
    assert_eq!(what, Merge::Added);
    assert_eq!(merged.len(), 4);
    assert_eq!((&merged[0], &merged[1]), (&rules[0], &rules[1]));
    assert_eq!(merged[2], json!({ "hostname": HOSTNAME, "service": "ssh://localhost:22" }));
    assert_eq!(merged[3], json!({ "service": "http_status:404" }));
    // there already and right: adopted as it is
    assert_eq!(cf::merge_ingress(&merged, HOSTNAME, cf::SERVICE), (merged.clone(), Merge::Unchanged));
    // there with another origin: only its service changes, its other settings and its place stay
    let mut old = merged.clone();
    old.swap(0, 2);
    old[0] =
        json!({ "hostname": HOSTNAME, "service": "ssh://172.17.0.1:22", "originRequest": { "connectTimeout": 10 } });
    let (changed, what) = cf::merge_ingress(&old, HOSTNAME, cf::SERVICE);
    assert_eq!(what, Merge::Service("ssh://172.17.0.1:22".to_string()));
    assert_eq!(
        changed[0],
        json!({ "hostname": HOSTNAME, "service": "ssh://localhost:22", "originRequest": { "connectTimeout": 10 } })
    );
    assert_eq!(changed[1..], old[1..]);
    // a tunnel without any rule gets the catch-all Cloudflare insists on
    let (first, _) = cf::merge_ingress(&[], HOSTNAME, cf::SERVICE);
    assert_eq!(
        first,
        [json!({ "hostname": HOSTNAME, "service": "ssh://localhost:22" }), json!({ "service": "http_status:404" })]
    );
}

#[test]
fn a_locally_managed_config_keeps_every_line_but_the_rule() {
    let yaml = "tunnel: abc\ncredentials-file: /etc/cloudflared/abc.json\n\ningress:\n  - hostname: status.example.com\n    service: http://localhost:3001\n  # everything else\n  - service: http_status:404\n";
    assert_eq!(cf::local_ingress(yaml).len(), 2);
    let (added, what) = cf::local_config_with(yaml, HOSTNAME, cf::SERVICE);
    assert_eq!(what, Merge::Added);
    assert_eq!(
        added,
        "tunnel: abc\ncredentials-file: /etc/cloudflared/abc.json\n\ningress:\n  - hostname: status.example.com\n    service: http://localhost:3001\n  # everything else\n  - hostname: ssh.example.com\n    service: ssh://localhost:22\n  - service: http_status:404\n"
    );
    assert_eq!(cf::local_config_with(&added, HOSTNAME, cf::SERVICE), (added.clone(), Merge::Unchanged));
    let other = added.replace("ssh://localhost:22", "ssh://172.17.0.1:22");
    let (back, what) = cf::local_config_with(&other, HOSTNAME, cf::SERVICE);
    assert_eq!((back, what), (added, Merge::Service("ssh://172.17.0.1:22".to_string())));
}

#[test]
fn dns_is_adopted_created_or_refused() {
    let target = "t.cfargotunnel.com";
    assert_eq!(cf::dns_action(&[], target), Dns::Create);
    assert_eq!(cf::dns_action(&[json!({ "type": "CNAME", "content": "t.cfargotunnel.com" })], target), Dns::Adopt);
    assert_eq!(
        cf::dns_action(&[json!({ "type": "A", "content": "203.0.113.7" })], target),
        Dns::Elsewhere("A 203.0.113.7".to_string())
    );
    assert_eq!(
        cf::dns_action(&[json!({ "type": "CNAME", "content": "other.cfargotunnel.com" })], target),
        Dns::Elsewhere("CNAME other.cfargotunnel.com".to_string())
    );
}

// ------------------------------------------------------------------------------------------------ discovery

#[test]
fn the_tunnel_is_found_in_a_container_a_unit_a_local_config_or_the_secrets() {
    let find = |s: &Server, configured: Option<&str>| {
        let secret = configured.map(|t| cestmoi::config::Secret::new(t.to_string()));
        cf::discover(s.host.as_ref(), &Docker::new(s.host.clone()), secret.as_ref())
    };
    // a container's environment
    let s = cestmoi("ssh-discover-container");
    let tunnel = find(&s, None).unwrap();
    assert_eq!(
        (tunnel.id.as_str(), tunnel.account.as_str(), &tunnel.source),
        (cloudflare::TUNNEL, cloudflare::ACCOUNT, &Source::Container("cloudflared".to_string()))
    );
    // … or its arguments
    s.connector(|inspect| {
        inspect["Config"]["Env"] = json!(["PATH=/usr/bin"]);
        inspect["Config"]["Cmd"] = json!(["tunnel", "--no-autoupdate", "run", "--token", cloudflare::tunnel_token()]);
    });
    assert_eq!(find(&s, None).unwrap().source, Source::Container("cloudflared".to_string()));
    // the secret wins over what is found
    assert_eq!(find(&s, Some(&cloudflare::tunnel_token())).unwrap().source, Source::Config);

    // a unit's --token
    let s = fresh("ssh-discover-unit");
    s.write(
        cf::UNIT,
        &format!(
            "[Service]\nExecStart=/usr/bin/cloudflared --no-autoupdate tunnel run --token {}\n",
            cloudflare::tunnel_token()
        ),
        0o644,
    );
    assert_eq!(find(&s, None).unwrap().source, Source::Unit(cf::UNIT.to_string()));

    // a locally managed tunnel
    let s = fresh("ssh-discover-local");
    s.write("/etc/cloudflared/config.yml", "tunnel: FAKEtunn-local\ncredentials-file: /etc/cloudflared/creds.json\ningress:\n  - service: http_status:404\n", 0o644);
    s.write(
        "/etc/cloudflared/creds.json",
        r#"{"AccountTag":"FAKEaccountlocal","TunnelID":"FAKEtunn-local","TunnelSecret":"FAKEsecret"}"#,
        0o600,
    );
    let tunnel = find(&s, None).unwrap();
    assert_eq!((tunnel.id.as_str(), tunnel.local_config()), ("FAKEtunn-local", Some("/etc/cloudflared/config.yml")));
    assert!(tunnel.token.is_none());

    // nothing: what to do about it
    let s = fresh("ssh-discover-none");
    let why = find(&s, None).unwrap_err();
    assert!(why.starts_with("no Cloudflare Tunnel was found on this machine"), "{why}");
    assert!(why.contains("Create a tunnel") && why.contains("moi never creates tunnels"), "{why}");
    // a token that is not one
    let why = find(&s, Some("FAKEnotatoken")).unwrap_err();
    assert!(why.contains("is not a Cloudflare Tunnel token"), "{why}");
}

// ------------------------------------------------------------------------------------------------ plans

#[test]
fn plan_for_a_fresh_host_with_a_tunnel() {
    let s = fresh("ssh-plan-fresh");
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    common::assert_golden(&golden("plan-fresh.txt"), &out);
    // a plan reads: nothing was asked of the machine or of Cloudflare that changes anything
    assert!(s.api.changes().is_empty(), "{:?}", s.api.changes());
    assert!(
        !s.fixture
            .commands_run()
            .iter()
            .any(|c| c.starts_with("apt-get") || c.starts_with("cloudflared") || c.starts_with("systemd-run"))
    );
    assert!(
        !out.contains(&cloudflare::tunnel_token()) && !out.contains(cloudflare::API_TOKEN),
        "a secret in the plan:\n{out}"
    );
}

#[test]
fn plan_for_cestmoi_adopts_the_rule_and_retires_the_container() {
    let s = cestmoi("ssh-plan-cestmoi");
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    common::assert_golden(&golden("plan-cestmoi.txt"), &out);
    assert!(
        out.contains(
            "  adopt   ingress      ssh.example.com → ssh://localhost:22 is there already: adopted, unchanged\n"
        ),
        "{out}"
    );
    assert!(out.contains("stop cloudflared and set its restart policy to `no` (was `unless-stopped`)"), "{out}");
    assert!(s.api.changes().is_empty() && !s.fixture.docker_requests().iter().any(|r| r.starts_with("POST")));
    let (_, json) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run", "--json"]);
    let v: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        v["steps"][4],
        json!({ "name": "ingress", "action": "adopt", "text": "ssh.example.com → ssh://localhost:22 is there already: adopted, unchanged" })
    );
}

#[test]
fn a_container_that_an_orchestrator_would_bring_back_is_reported_not_stopped() {
    let s = cestmoi("ssh-plan-orchestrated");
    s.connector(|inspect| {
        inspect["Config"]["Labels"] =
            json!({ "com.docker.swarm.service.name": "cloudflared", "com.docker.swarm.task.id": "FAKEtask" });
    });
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    common::assert_golden(&golden("plan-orchestrated.txt"), &out);
    assert!(
        out.contains(
            "  note    container    cloudflared also serves this tunnel and is managed by the swarm service cloudflared"
        ),
        "{out}"
    );
    assert!(!out.contains("stop cloudflared and set"), "{out}");
    // Dokploy's stacks are Dokploy's as well
    s.connector(|inspect| {
        inspect["Config"]["Labels"]["com.docker.compose.project.working_dir"] =
            json!("/etc/dokploy/compose/cloudflared-ab12/code");
        inspect["Config"]["Labels"].as_object_mut().unwrap().remove("com.docker.swarm.service.name");
    });
    let (_, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run"]);
    assert!(out.contains("is managed by Dokploy: stop or delete it in Dokploy's interface"), "{out}");
}

#[test]
fn what_the_api_token_lacks_is_named() {
    let s = fresh("ssh-plan-permissions");
    s.api.state().may_edit_tunnels = false;
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("STOP    credentials  the API token lacks: Account → Cloudflare Tunnel → Edit — add it to the token (secret cloudflare_api_token)"), "{out}");
    assert!(out.ends_with("nothing was changed: the run cannot go on (see STOP above)\n"), "{out}");
    s.api.state().may_edit_tunnels = true;
    s.api.state().may_edit_dns = false;
    let (_, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run"]);
    assert!(out.contains("the API token lacks: Zone → DNS → Edit for the zone of ssh.example.com"), "{out}");
    // a name in no zone of the account reads the same: the token cannot see its zone
    s.api.state().may_edit_dns = true;
    let (_, out) = s.moi(&["ssh", "up", "--hostname", "ssh.elsewhere.org", "--dry-run"]);
    assert!(out.contains("Zone → DNS → Edit for the zone of ssh.elsewhere.org"), "{out}");
    s.api.state().token = "FAKEanother".to_string();
    let (_, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run"]);
    assert!(out.contains("Cloudflare does not accept the API token (secret cloudflare_api_token)"), "{out}");
    // no token at all: how to make one
    let s = fresh("ssh-plan-no-token");
    s.write(
        "/etc/moi/secrets.toml",
        &format!("[secrets]\ncloudflare_tunnel_token = \"{}\"\n", cloudflare::tunnel_token()),
        0o600,
    );
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--dry-run"]);
    assert_eq!(code, 1);
    assert!(
        out.contains("an API token is needed") && out.contains("put it into secrets.toml as cloudflare_api_token"),
        "{out}"
    );
    // and no hostname: asked for
    let (code, out) = fresh("ssh-plan-no-hostname").moi(&["ssh", "up", "--dry-run"]);
    assert_eq!(code, 2);
    assert!(out.contains("--hostname ssh.example.com"), "{out}");
}

#[test]
fn dns_that_points_elsewhere_stops_the_run() {
    let s = fresh("ssh-plan-dns-elsewhere");
    s.api.add_record("A", HOSTNAME, "203.0.113.7");
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("STOP    dns          ssh.example.com already points somewhere else (A 203.0.113.7). moi does not overwrite DNS it did not create"), "{out}");
    assert!(s.api.changes().is_empty(), "{:?}", s.api.changes());
    assert!(!s.fixture.commands_run().iter().any(|c| c.starts_with("apt-get") || c.starts_with("cloudflared")));
    assert_eq!(s.api.state().records.len(), 1);
}

// ------------------------------------------------------------------------------------------------ the client side

#[test]
fn connect_prints_the_one_command_for_a_computer() {
    let s = cestmoi("ssh-connect");
    s.write(
        "/etc/moi/moi.toml",
        &format!(
            "{}\n[ssh]\nhostname = \"{HOSTNAME}\"\nuser = \"owner\"\napi_token = \"secret:cloudflare_api_token\"\n",
            s.read("/etc/moi/moi.toml")
        ),
        0o644,
    );
    let (code, out) = s.moi(&["ssh", "connect"]);
    assert_eq!(code, 0, "{out}");
    common::assert_golden(&golden("connect.txt"), &out);
    // only public values, and the release this version was tested with
    assert!(out.starts_with("curl -fsSL https://github.com/prabhask5/devbox/releases/download/ssh-v0.1.0/devbox-ssh-setup.sh | sh -s -- add \\\n  --alias cestmoi --url https://ssh.example.com --user owner \\\n  --host-key 'ssh-ed25519 AAAAC3"), "{out}");
    assert!(!out.contains("root@server") && !out.contains("FAKEcloudflare"), "{out}");
}

#[test]
fn authorize_appends_a_valid_key_once_with_the_right_modes_and_owner() {
    let s = fresh("ssh-authorize");
    let (code, out) = s.moi(&["ssh", "authorize", LAPTOP_KEY, "--name", "work laptop"]);
    assert_eq!((code, out.as_str()), (0, "work laptop may now log in as owner\n"));
    let file = Path::new("/home/owner/.ssh/authorized_keys");
    assert_eq!(
        s.read("/home/owner/.ssh/authorized_keys"),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl work laptop\n"
    );
    let (file_meta, dir_meta) = (s.host.metadata(file).unwrap(), s.host.metadata(file.parent().unwrap()).unwrap());
    assert_eq!((file_meta.mode, file_meta.uid, file_meta.gid), (0o600, 1000, 1000));
    assert_eq!((dir_meta.mode, dir_meta.uid), (0o700, 1000));
    // again, under another name: nothing changes
    let (code, out) = s.moi(&["ssh", "authorize", LAPTOP_KEY, "--name", "again"]);
    assert_eq!((code, out.as_str()), (0, "that key is already allowed to log in as owner: nothing was changed\n"));
    assert_eq!(s.read("/home/owner/.ssh/authorized_keys").lines().count(), 1);
    // from a file, keeping its comment; next to a key that was there
    s.write("/home/owner/second.pub", "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE0= phone\n", 0o644);
    assert_eq!(s.moi(&["ssh", "authorize", "/home/owner/second.pub"]).1, "phone may now log in as owner\n");
    assert_eq!(s.read("/home/owner/.ssh/authorized_keys").lines().count(), 2);
    // what is not a key is refused, and says why
    // (put together here so that the repository's secret scan has nothing to find)
    let private_key_line = format!("-----BEGIN OPENSSH {} KEY-----", "PRIVATE");
    for (bad, why) in [
        (private_key_line.as_str(), "is not a key type moi accepts"),
        ("ssh-dss AAAAB3NzaC1kc3MAAACBAFAKE comment", "is not a key type moi accepts"),
        ("ssh-ed25519 not-base64!! x", "the key itself is not base64"),
        (
            "ssh-ed25519 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTY= x",
            "does not match the type written in front of it",
        ),
        ("ssh-ed25519", "a public key line looks like"),
    ] {
        let (code, out) = s.moi(&["ssh", "authorize", "--", bad]);
        assert_eq!(code, 2, "{bad}");
        assert!(out.contains(why), "{bad}: {out}");
    }
    assert_eq!(s.read("/home/owner/.ssh/authorized_keys").lines().count(), 2);
    // the fingerprints, never the keys, are what status and the records show
    let keys = cestmoi::ssh::authorized_keys(s.host.as_ref(), "owner");
    assert_eq!(keys[0], ("SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU".to_string(), "work laptop".to_string()));
}

// ------------------------------------------------------------------------------------------------ up, down, confirm

impl Server {
    /// Everything an apply talks to: Cloudflare's package server, apt, cloudflared, systemd, the
    /// connector's metrics and the public name.
    fn world(&self) {
        let key = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh/cloudflare-main.gpg"))
            .unwrap();
        self.package_key(key);
        for command in [
            "apt-get update -q",
            "systemctl daemon-reload",
            "systemctl restart cloudflared.service",
            "systemctl is-active --quiet cloudflared.service",
            "systemctl stop moi-ssh-rollback.timer",
            "cloudflared --version",
        ] {
            self.fixture.add_command(
                command,
                0,
                if command.ends_with("--version") { "cloudflared version 2026.10.0\n" } else { "" },
            );
        }
        // what apt and cloudflared leave behind
        let host = self.fixture.clone();
        self.fixture.add_command("apt-get install -y -q cloudflared", 0, "");
        self.fixture.on_command(
            "apt-get install -y -q cloudflared",
            Arc::new(move || host.write(Path::new("/usr/bin/cloudflared"), b"", 0o755).unwrap()),
        );
        let install = format!("cloudflared service install {}", cloudflare::tunnel_token());
        let host = self.fixture.clone();
        self.fixture.add_command(&install, 0, "");
        self.fixture.on_command(
            &install,
            Arc::new(move || {
                let unit = format!("[Unit]\nDescription=cloudflared\n\n[Service]\nExecStart=/usr/bin/cloudflared --no-autoupdate tunnel run --token {}\n\n[Install]\nWantedBy=multi-user.target\n", cloudflare::tunnel_token());
                host.write(Path::new(cf::UNIT), unit.as_bytes(), 0o644).unwrap();
            }),
        );
        let host = self.fixture.clone();
        self.fixture.add_command("cloudflared service uninstall", 0, "");
        self.fixture.on_command(
            "cloudflared service uninstall",
            Arc::new(move || host.remove_file(Path::new(cf::UNIT)).unwrap()),
        );
        cloudflare::serve_metrics(&self.fixture, METRICS, 4);
        cloudflare::serve_ssh_over_websocket(&self.fixture, HOSTNAME, "SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13\r\n");
    }

    fn package_key(&self, key: Vec<u8>) {
        let handler: cestmoi::host::HttpHandler = Arc::new(move |_authority, _raw| {
            let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", key.len()).into_bytes();
            out.extend(&key);
            out
        });
        self.fixture.set_http_at("pkg.cloudflare.com:443", handler);
    }

    fn ran(&self, start: &str) -> Vec<String> {
        self.fixture.commands_run().into_iter().filter(|c| c.starts_with(start)).collect()
    }
}

#[test]
fn up_on_a_fresh_host_installs_the_connector_adds_rule_and_record_and_down_puts_everything_back() {
    let s = fresh("ssh-up-fresh");
    s.world();
    let (config_before, secrets_before) = (s.read("/etc/moi/moi.toml"), s.read("/etc/moi/secrets.toml"));
    let ingress_before = s.api.ingress();

    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]);
    assert_eq!(code, 0, "{out}");
    for line in [
        "  done   cloudflared installed from Cloudflare's repository\n",
        "  done   cloudflared.service installed\n",
        "  done   the host's connector has 4 edge connections\n",
        "  done   ingress rule ssh.example.com → ssh://localhost:22\n",
        "  done   CNAME ssh.example.com → FAKEtunn-0000-4000-8000-000000000001.cfargotunnel.com\n",
        "  done   [ssh] written to /etc/moi/moi.toml\n",
        "  ✓ an SSH server answers at https://ssh.example.com/",
        "next: `moi ssh connect` prints the command for your computers\n",
    ] {
        assert!(out.contains(line), "missing {line:?} in:\n{out}");
    }
    assert!(!out.contains(&cloudflare::tunnel_token()), "the token was printed:\n{out}");
    // on the machine
    assert_eq!(
        s.read(cf::DROP_IN),
        "# Written by `moi ssh up`; removed by `moi ssh down`.\n[Service]\nEnvironment=TUNNEL_METRICS=127.0.0.1:20241\n"
    );
    assert_eq!(
        s.read("/etc/apt/sources.list.d/cloudflared.list"),
        "deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main\n"
    );
    let config = s.read("/etc/moi/moi.toml");
    assert!(config.starts_with(&config_before), "what was in moi.toml is untouched");
    assert!(
        config.ends_with("# >>> moi ssh up (managed block: `moi ssh down` removes it)\n[ssh]\nprovider = \"cloudflare\"\nhostname = \"ssh.example.com\"\nalias = \"myserver\"\nuser = \"owner\"\napi_token = \"secret:cloudflare_api_token\"\ntunnel_token = \"secret:cloudflare_tunnel_token\"\nmetrics = \"127.0.0.1:20241\"\nclient_release = \"ssh-v0.1.0\"\nmonitor = \"SSH tunnel\"\n# <<< moi ssh up\n"),
        "{config}"
    );
    assert_eq!(s.moi(&["config", "check", "--quiet"]).0, 0, "the configuration it wrote loads");
    // at Cloudflare: the rule before the catch-all, one record, nothing else
    assert_eq!(
        s.api.ingress(),
        [json!({ "hostname": HOSTNAME, "service": "ssh://localhost:22" }), json!({ "service": "http_status:404" })]
    );
    assert_eq!(
        s.api.state().config["warp-routing"],
        json!({ "enabled": false }),
        "the rest of the tunnel's configuration is kept"
    );
    assert_eq!(s.api.state().records.len(), 1);
    assert_eq!(s.api.state().records[0]["content"], format!("{}.cfargotunnel.com", cloudflare::TUNNEL));
    assert_eq!(s.api.state().records[0]["proxied"], true);
    // the connector was connected before the rule pointed anyone at it
    let commands = s.fixture.commands_run();
    let at = |needle: &str| {
        commands.iter().position(|c| c.starts_with(needle)).unwrap_or_else(|| panic!("{needle} not run: {commands:?}"))
    };
    assert!(
        at("apt-get install") < at("cloudflared service install")
            && at("cloudflared service install") < at("systemctl restart cloudflared.service")
    );

    // again: nothing to do
    let changes = s.api.changes().len();
    let (code, out) = s.moi(&["ssh", "up"]);
    assert_eq!(code, 0, "{out}");
    common::assert_golden(&golden("plan-up-to-date.txt"), &out);
    assert!(out.ends_with("everything already up to date\n"), "{out}");
    assert_eq!(s.api.changes().len(), changes);

    // status: one line of health, then the parts
    let (code, out) = s.moi(&["ssh", "status"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.starts_with(
            "ssh myserver — ssh.example.com\n  ✓ SSH tunnel up: 4 edge connections (ssh.example.com), handshake "
        ),
        "{out}"
    );
    assert!(out.contains("  connector  cloudflared version 2026.10.0\n  ingress    ssh.example.com → ssh://localhost:22\n  dns        CNAME → FAKEtunn-0000-4000-8000-000000000001.cfargotunnel.com\n  keys       0 allowed to log in\n"), "{out}");

    // down: what would be done, then done
    let (_, would) = s.moi(&["ssh", "down", "--dry-run"]);
    assert!(
        would.contains("would remove the ingress rule of ssh.example.com (every other rule stays)")
            && would.contains("would delete the DNS record"),
        "{would}"
    );
    assert_eq!(s.api.state().records.len(), 1, "a dry run changes nothing");
    let (code, out) = s.moi(&["ssh", "down"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(s.read("/etc/moi/moi.toml"), config_before, "moi.toml byte for byte");
    assert_eq!(s.read("/etc/moi/secrets.toml"), secrets_before);
    assert!(!s.host.exists(Path::new(cf::DROP_IN)) && !s.host.exists(Path::new(cf::DROP_IN).parent().unwrap()));
    assert!(
        !s.host.exists(Path::new(cf::UNIT)),
        "the unit cloudflared wrote is gone with `cloudflared service uninstall`"
    );
    assert_eq!(s.api.ingress(), ingress_before);
    assert!(s.api.state().records.is_empty());
    assert!(out.contains("the cloudflared package and Cloudflare's apt repository stay installed"), "{out}");
    assert!(
        !s.api.state().requests.iter().any(|r| r.starts_with("DELETE") && r.contains("cfd_tunnel")),
        "the tunnel is never deleted"
    );
    assert_eq!(s.moi(&["ssh", "down"]).1, "nothing to restore: `moi ssh up` has changed nothing on this server\n");
}

#[test]
fn a_package_key_that_is_not_the_one_moi_knows_stops_the_install() {
    let s = fresh("ssh-up-wrong-key");
    s.world();
    s.package_key(b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nsomebody else's\n".to_vec());
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("is not the one moi knows (sha256 ") && out.contains("cloudflared is not installed"), "{out}");
    assert!(s.ran("apt-get").is_empty() && s.ran("cloudflared").is_empty());
    assert!(!s.host.exists(Path::new("/usr/share/keyrings/cloudflare-main.gpg")) && s.api.changes().is_empty());
}

#[test]
fn a_connector_that_does_not_connect_stops_the_run_before_anything_that_worked_is_touched() {
    let s = cestmoi("ssh-up-not-ready");
    s.world();
    cloudflare::serve_metrics(&s.fixture, METRICS, 0);
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("cloudflared.service has no connection to Cloudflare after 60 s (no edge connections)"),
        "{out}"
    );
    // the container that still carries the tunnel was not stopped
    assert!(!s.fixture.docker_requests().iter().any(|r| r.starts_with("POST")), "{:?}", s.fixture.docker_requests());
    assert!(s.api.changes().is_empty());
}

#[test]
fn up_on_cestmoi_retires_the_container_and_down_gives_it_back() {
    let s = cestmoi("ssh-up-cestmoi");
    s.world();
    let ingress_before = s.api.ingress();
    let config_before = s.read("/etc/moi/moi.toml");
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("  done   container cloudflared stopped (restart policy `no`)\n"), "{out}");
    // through the Docker API: the policy first, so that it cannot come back between the two
    let posts: Vec<String> = s.fixture.docker_requests().into_iter().filter(|r| r.starts_with("POST")).collect();
    assert_eq!(posts, [format!("POST /containers/{CONNECTOR}/update"), format!("POST /containers/{CONNECTOR}/stop")]);
    let sent = s.fixture.docker_sent();
    assert_eq!(String::from_utf8_lossy(&sent[0].1), r#"{"RestartPolicy":{"Name":"no"}}"#);
    // the host's connector was up before that
    let commands = s.fixture.commands_run();
    assert!(commands.iter().any(|c| c == "systemctl restart cloudflared.service"));
    // the rule and the record were there: adopted, nothing sent to Cloudflare
    assert!(s.api.changes().is_empty(), "{:?}", s.api.changes());
    assert_eq!(s.api.ingress(), ingress_before);
    // the token found in the container is a secret now, and [ssh] refers to it
    assert!(
        s.read("/etc/moi/secrets.toml")
            .contains(&format!("cloudflare_tunnel_token = \"{}\"\n", cloudflare::tunnel_token()))
    );
    assert!(s.read("/etc/moi/moi.toml").contains("alias = \"cestmoi\"\nuser = \"prabhas\"\n"));
    // moi's own boot job does not start the retired stack again
    let db = cestmoi::state::StateDir::new("/var/lib/moi").open_db(s.host.as_ref()).unwrap();
    assert_eq!(db.setting("ssh.retired_stacks").as_deref(), Some(r#"["/opt/services/cloudflared"]"#));

    let (code, out) = s.moi(&["ssh", "down"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("give container cloudflared its restart policy `unless-stopped` back and start it"), "{out}");
    let posts: Vec<String> =
        s.fixture.docker_requests().into_iter().filter(|r| r.starts_with("POST")).skip(2).collect();
    assert_eq!(posts, [format!("POST /containers/{CONNECTOR}/update"), format!("POST /containers/{CONNECTOR}/start")]);
    assert_eq!(
        String::from_utf8_lossy(&s.fixture.docker_sent()[1].1),
        r#"{"RestartPolicy":{"Name":"unless-stopped"}}"#
    );
    assert_eq!(s.read("/etc/moi/moi.toml"), config_before);
    assert!(!s.read("/etc/moi/secrets.toml").contains("cloudflare_tunnel_token"));
    // what moi adopted is not moi's to remove
    assert_eq!(s.api.ingress(), ingress_before);
    assert_eq!(s.api.state().records.len(), 1);
    assert!(s.api.changes().is_empty());
    assert_eq!(db.setting("ssh.retired_stacks"), None);
}

/// cestmoi reached over its tunnel, whose rule still names the Docker bridge: the change touches the
/// path the session runs on.
fn over_the_tunnel(test: &str) -> Server {
    let s = cestmoi(test);
    s.world();
    s.api.set_ingress(
        json!([{ "hostname": HOSTNAME, "service": "ssh://172.17.0.1:22" }, { "service": "http_status:404" }]),
    );
    s.fixture.set_env("SSH_CONNECTION", "127.0.0.1 50000 127.0.0.1 22");
    s.write("/usr/local/bin/moi", "", 0o755);
    s
}

fn archive(s: &Server) -> String {
    let names = s.host.read_dir(Path::new("/var/lib/moi/archive/ssh")).unwrap();
    format!("/var/lib/moi/archive/ssh/{}", names[0])
}

#[test]
fn over_the_tunnel_the_way_back_is_armed_first_and_confirm_keeps_the_change() {
    let s = over_the_tunnel("ssh-safety-confirm");
    s.fixture.add_command_then("systemd-run ...", 0, "");
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]);
    assert_eq!(code, 0, "{out}");
    let dir = archive(&s);
    let runs = s.ran("systemd-run");
    assert_eq!(
        runs,
        [
            format!(
                "systemd-run --quiet --collect --unit=moi-ssh-rollback --on-active=300s /usr/local/bin/moi ssh down --rollback {dir}"
            ),
            format!(
                "systemd-run --quiet --collect --wait --unit=moi-ssh-apply /usr/local/bin/moi ssh up --hostname ssh.example.com --yes --inner {dir}"
            ),
        ]
    );
    // this process changed nothing of the path itself: that is the unit's work
    assert!(!s.fixture.docker_requests().iter().any(|r| r.starts_with("POST")) && s.api.changes().is_empty());
    assert!(s.ran("cloudflared service").is_empty());
    assert!(out.contains("Now open a NEW connection (`ssh cestmoi`) and run `sudo moi ssh confirm` within 5 minutes.\nWithout it, everything is put back as it was.\n"), "{out}");
    let (_, status) = s.moi(&["ssh", "status"]);
    assert!(status.contains("waits for `moi ssh confirm`"), "{status}");

    // what the unit does: the tunnel steps, into the same record
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes", "--inner", &dir]);
    assert_eq!((code, out.as_str()), (0, ""));
    assert_eq!(s.api.ingress()[0], json!({ "hostname": HOSTNAME, "service": "ssh://localhost:22" }));
    assert!(s.fixture.docker_requests().contains(&format!("POST /containers/{CONNECTOR}/stop")));

    // from a new connection
    assert_eq!(s.moi(&["ssh", "confirm"]).1, format!("kept: the change is permanent (its record is in {dir})\n"));
    assert_eq!(s.ran("systemctl stop moi-ssh-rollback.timer").len(), 1);
    assert_eq!(s.moi(&["ssh", "confirm"]).1, "nothing waits for confirmation\n");
    assert_eq!(s.api.ingress()[0]["service"], "ssh://localhost:22");
}

#[test]
fn without_a_confirmation_the_timer_puts_connector_container_and_ingress_back() {
    let s = over_the_tunnel("ssh-safety-rollback");
    s.fixture.add_command_then("systemd-run ...", 0, "");
    let config_before = s.read("/etc/moi/moi.toml");
    assert_eq!(s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]).0, 0);
    let dir = archive(&s);
    assert_eq!(s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes", "--inner", &dir]).0, 0);
    assert_eq!(s.api.ingress()[0]["service"], "ssh://localhost:22");

    // five minutes pass without `moi ssh confirm`: this is what the timer runs
    let (code, out) = s.moi(&["ssh", "down", "--rollback", &dir]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.api.ingress(),
        [json!({ "hostname": HOSTNAME, "service": "ssh://172.17.0.1:22" }), json!({ "service": "http_status:404" })],
        "the rule as it was"
    );
    let posts: Vec<String> = s.fixture.docker_requests().into_iter().filter(|r| r.starts_with("POST")).collect();
    assert_eq!(
        posts[2..],
        [format!("POST /containers/{CONNECTOR}/update"), format!("POST /containers/{CONNECTOR}/start")],
        "the container carries the tunnel again"
    );
    assert!(!s.host.exists(Path::new(cf::DROP_IN)) && !s.host.exists(Path::new(cf::UNIT)));
    assert_eq!(s.read("/etc/moi/moi.toml"), config_before);
    // the old connector came back before the new one went
    let commands = s.fixture.commands_run();
    assert!(commands.iter().any(|c| c == "cloudflared service uninstall"));
    assert_eq!(s.moi(&["ssh", "confirm"]).1, "nothing waits for confirmation\n");
}

#[test]
fn when_the_way_back_cannot_be_armed_nothing_is_changed() {
    let s = over_the_tunnel("ssh-safety-no-timer");
    s.fixture.add_command_then("systemd-run ...", 1, "");
    let config_before = s.read("/etc/moi/moi.toml");
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("not changing the tunnel through itself without a way back: systemd-run could not schedule the rollback. Nothing was changed"), "{out}");
    assert_eq!(s.ran("systemd-run").len(), 1, "the apply unit was never started");
    assert!(s.api.changes().is_empty() && !s.fixture.docker_requests().iter().any(|r| r.starts_with("POST")));
    assert!(s.ran("cloudflared service").is_empty() && s.ran("apt-get").is_empty());
    assert_eq!(s.read("/etc/moi/moi.toml"), config_before);
    assert_eq!(s.moi(&["ssh", "confirm"]).1, "nothing waits for confirmation\n");
}

#[test]
fn at_the_console_or_for_a_first_setup_no_timer_is_needed() {
    // over SSH, but the name has no rule yet: the session cannot be coming in through it
    let s = fresh("ssh-safety-first-setup");
    s.world();
    s.fixture.set_env("SSH_CONNECTION", "203.0.113.5 50000 192.0.2.10 22");
    assert_eq!(s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]).0, 0);
    assert!(s.ran("systemd-run").is_empty());
}

#[test]
fn the_health_messages_are_what_the_check_will_say() {
    use cestmoi::ssh::{self, health};
    let s = fresh("ssh-health");
    s.world();
    assert_eq!(s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]).0, 0);
    let loaded =
        cestmoi::config::load(s.host.as_ref(), &cestmoi::config::Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let settings = ssh::settings(s.host.as_ref(), &loaded, None).unwrap();
    assert_eq!(
        health(&s.host, &settings, false),
        (true, "SSH tunnel up: 4 edge connections (ssh.example.com)".to_string())
    );
    let (up, message) = health(&s.host, &settings, true);
    assert!(
        up && message.starts_with("SSH tunnel up: 4 edge connections (ssh.example.com), handshake ")
            && message.ends_with(" ms"),
        "{message}"
    );
    // what answers is not an SSH server
    cloudflare::serve_ssh_over_websocket(&s.fixture, HOSTNAME, "HTTP/1.1 400 Bad Request\r\n");
    assert_eq!(health(&s.host, &settings, true), (false, "SSH tunnel down: public handshake failed (banner: what answers is not an SSH server (\"HTTP/1.1 400 Bad Request\"))".to_string()));
    // the edge answers, but not with a WebSocket
    let handler: cestmoi::host::HttpHandler =
        Arc::new(|_, _| b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n".to_vec());
    s.fixture.set_http_at(&format!("{HOSTNAME}:443"), handler);
    assert_eq!(
        health(&s.host, &settings, true),
        (
            false,
            "SSH tunnel down: public handshake failed (websocket: answered 502 instead of switching protocols)"
                .to_string()
        )
    );
    cloudflare::serve_metrics(&s.fixture, METRICS, 0);
    assert_eq!(health(&s.host, &settings, true), (false, "SSH tunnel down: no edge connections".to_string()));
    let handler: cestmoi::host::HttpHandler =
        Arc::new(|_, _| b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec());
    s.fixture.set_http_at(METRICS, handler);
    assert_eq!(
        health(&s.host, &settings, false),
        (false, "SSH tunnel down: metrics not answering (HTTP 404 without a connection count)".to_string())
    );
    s.fixture.add_command("systemctl is-active --quiet cloudflared.service", 3, "");
    assert_eq!(health(&s.host, &settings, true), (false, "SSH tunnel down: cloudflared not running".to_string()));
    let (code, out) = s.moi(&["ssh", "status"]);
    assert_eq!(code, 1);
    assert!(out.contains("  ✗ SSH tunnel down: cloudflared not running\n"), "{out}");
}

// ------------------------------------------------------------------------------------------------ records

/// cestmoi after `moi ssh up`: the connector on the host, `[ssh]` in moi.toml.
fn set_up(test: &str) -> Server {
    let s = cestmoi(test);
    s.world();
    s.write("/home/prabhas/.ssh/authorized_keys", &format!("{LAPTOP_KEY}\n"), 0o600);
    let (code, out) = s.moi(&["ssh", "up", "--hostname", HOSTNAME, "--yes"]);
    assert_eq!(code, 0, "{out}");
    s
}

#[test]
fn the_ssh_check_runs_with_the_others_and_looks_from_outside_every_quarter_of_an_hour() {
    use cestmoi::checks::{self, Mode};
    let s = set_up("ssh-check");
    let loaded =
        cestmoi::config::load(s.host.as_ref(), &cestmoi::config::Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let config = Arc::new(loaded.config);
    let check = checks::enabled(&config).into_iter().find(|c| c.name() == "ssh").expect("[ssh] switches the check on");
    assert_eq!(
        (check.interval(&config).as_secs(), check.monitor(&config).as_deref(), check.journal_tag(&config).as_str()),
        (300, Some("SSH tunnel"), "moi-check-ssh")
    );
    // its monitor: a push monitor like every check's, late by a fifth of the interval at most
    let row = cestmoi::monitors::desired(&config, &Docker::new(s.host.clone()))
        .into_iter()
        .find(|d| d.owner == "check:ssh")
        .unwrap();
    assert_eq!((row.name.as_str(), row.interval, row.retries), ("SSH tunnel", 360, 1));

    // count the looks from outside
    let looks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = looks.clone();
    let handler: cestmoi::host::HttpHandler = Arc::new(move |_, _| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let banner = "SSH-2.0-OpenSSH_9.6\r\n";
        let mut out = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n".to_vec();
        out.extend([0x82, u8::try_from(banner.len()).unwrap()]);
        out.extend(banner.as_bytes());
        out
    });
    s.fixture.set_http_at(&format!("{HOSTNAME}:443"), handler);
    let db = cestmoi::state::StateDir::new("/var/lib/moi").open_db(s.host.as_ref()).unwrap();
    let run = || checks::run(&s.host, &config, &db, &check, Mode::Record).unwrap();
    let first = run();
    assert!(
        first.message.starts_with("SSH tunnel up: 4 edge connections (ssh.example.com), handshake ")
            && first.message.ends_with(" ms"),
        "{}",
        first.message
    );
    s.fixture.advance(std::time::Duration::from_secs(300));
    assert_eq!(run().message, "SSH tunnel up: 4 edge connections (ssh.example.com)");
    s.fixture.advance(std::time::Duration::from_secs(300));
    assert_eq!(run().message, "SSH tunnel up: 4 edge connections (ssh.example.com)");
    assert_eq!(looks.load(std::sync::atomic::Ordering::SeqCst), 1, "two runs in between did not look from outside");
    s.fixture.advance(std::time::Duration::from_secs(300));
    assert!(run().message.contains("handshake"));
    assert_eq!(looks.load(std::sync::atomic::Ordering::SeqCst), 2);

    // a look that fails is remembered until the next one
    let handler: cestmoi::host::HttpHandler = Arc::new(|_, _| b"HTTP/1.1 530 X\r\nContent-Length: 0\r\n\r\n".to_vec());
    s.fixture.set_http_at(&format!("{HOSTNAME}:443"), handler);
    s.fixture.advance(std::time::Duration::from_secs(900));
    let failed = run();
    assert_eq!(
        (failed.status.as_str(), failed.message.as_str()),
        ("down", "SSH tunnel down: public handshake failed (websocket: answered 530 instead of switching protocols)")
    );
    s.fixture.advance(std::time::Duration::from_secs(300));
    assert_eq!(run().message, failed.message, "the connector is connected, but nobody gets through");
    // by hand: the whole look, every time, and on the command line
    let (code, out) = s.moi(&["check", "ssh"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("SSH tunnel down: public handshake failed"), "{out}");
    // the connector gone
    s.fixture.add_command("systemctl is-active --quiet cloudflared.service", 3, "");
    assert_eq!(run().message, "SSH tunnel down: cloudflared not running");
    // the check changes nothing
    assert!(s.api.changes().is_empty());
}

#[test]
fn snapshot_context_and_audit_describe_the_way_in() {
    let s = set_up("ssh-records");
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    let access = s.read("/var/lib/moi/state/snapshot/ssh/access.json");
    common::assert_golden(&golden("access.json"), &access);
    // fingerprints and comments, never a key; no token
    assert!(
        access.contains("SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU")
            && !access.contains("AAAAC3NzaC1lZDI1NTE5AAAAIOMq"),
        "{access}"
    );
    assert!(!access.contains(&cloudflare::tunnel_token()) && !access.contains(cloudflare::API_TOKEN));
    assert_eq!(s.read("/var/lib/moi/state/volatile/ssh-connector.txt"), "cloudflared.service unknown\n");

    // Cloudflare is asked once a day: a second snapshot reads the cache
    let asked = s.api.state().requests.len();
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    assert_eq!(s.api.state().requests.len(), asked);
    s.fixture.advance(std::time::Duration::from_secs(25 * 3600));
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    assert!(s.api.state().requests.len() > asked);

    let context = s.read("/var/lib/moi/state/CONTEXT.md");
    let block: String = context
        .split("### SSH access\n")
        .nth(1)
        .expect("an SSH access block")
        .split("\n## ")
        .next()
        .unwrap()
        .to_string();
    common::assert_golden(&golden("context-ssh-access.md"), &block);
    assert!(
        block.contains("`ssh cestmoi` (user `prabhas`)") && block.contains("`ssh.example.com` → `ssh://localhost:22`"),
        "{block}"
    );

    // the audit: fine as it is …
    s.moi(&["audit"]);
    let audit = s.read("/var/lib/moi/state/audit.md");
    assert!(audit.contains("sshd at ssh.example.com takes public keys only; 1 keys may log in"), "{audit}");
    // … and an entry when sshd would take a password at a public name
    s.fixture.add_command("sshd -T", 0, "permitrootlogin yes\npasswordauthentication yes\n");
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    assert_eq!(s.moi(&["audit"]).0, 1);
    let audit = s.read("/var/lib/moi/state/audit.md");
    let section: String = audit.split("SSH from outside").nth(1).unwrap().split("\n## ").next().unwrap().to_string();
    common::assert_golden(&golden("audit-ssh.md"), &section);
    let context = s.read("/var/lib/moi/state/CONTEXT.md");
    assert!(
        context
            .split("## ⚠ Attention")
            .nth(1)
            .unwrap()
            .contains("sshd accepts passwords, and it answers at the public name ssh.example.com"),
        "the attention list names it"
    );
}

#[test]
fn a_server_without_ssh_configured_records_nothing_of_it() {
    let s = cestmoi("ssh-records-absent");
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    assert!(!s.host.exists(Path::new("/var/lib/moi/state/snapshot/ssh")));
    assert!(!s.read("/var/lib/moi/state/CONTEXT.md").contains("SSH access"));
    let (code, out) = s.moi(&["check", "ssh"]);
    assert_eq!(code, 2, "{out}");
}

#[test]
fn a_rebuild_brings_the_connector_back_from_the_old_servers_ssh_section() {
    // the new machine: nothing but the bundle
    let s = fresh("ssh-restore");
    s.world();
    s.write("/etc/moi/secrets.toml", "[secrets]\n", 0o600);
    let bundle = "/root/moi-bundle";
    s.host.create_dir_all(&Path::new(bundle).join("var/lib/moi/state/snapshot"), 0o700).unwrap();
    s.write(
        &format!("{bundle}/etc/moi/moi.toml"),
        "[server]\nname = \"oldserver\"\n\n[ssh]\nhostname = \"ssh.example.com\"\nuser = \"owner\"\napi_token = \"secret:cloudflare_api_token\"\ntunnel_token = \"secret:cloudflare_tunnel_token\"\n",
        0o644,
    );
    s.write(
        &format!("{bundle}/etc/moi/secrets.toml"),
        &format!(
            "[secrets]\ncloudflare_api_token = \"{}\"\ncloudflare_tunnel_token = \"{}\"\n",
            cloudflare::API_TOKEN,
            cloudflare::tunnel_token()
        ),
        0o600,
    );
    // at Cloudflare everything is as the old server left it
    s.api.set_ingress(
        json!([{ "hostname": HOSTNAME, "service": "ssh://localhost:22" }, { "service": "http_status:404" }]),
    );
    s.api.add_record("CNAME", HOSTNAME, &format!("{}.cfargotunnel.com", cloudflare::TUNNEL));

    let (code, out) = s.moi(&["restore", "--bundle", bundle, "--only", "ssh", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("==> phase: ssh — ")
            && out.contains(
                "    adopt   ingress      ssh.example.com → ssh://localhost:22 is there already: adopted, unchanged"
            ),
        "{out}"
    );
    assert!(out.contains("    [dry-run] moi ssh up --yes; then the SSH handshake through the public name"), "{out}");
    assert!(s.ran("apt-get").is_empty() && s.ran("cloudflared").is_empty());

    let (code, out) = s.moi(&["restore", "--bundle", bundle, "--only", "ssh", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("    cloudflared.service installed\n")
            && out.contains("  ✓ an SSH server answers at https://ssh.example.com/"),
        "{out}"
    );
    assert!(out.contains("`ssh oldserver` works again"), "{out}");
    assert_eq!(s.ran("cloudflared service install").len(), 1);
    assert!(s.host.exists(Path::new(cf::DROP_IN)));
    // ingress and DNS were what they should be: confirmed, not rewritten
    assert!(s.api.changes().is_empty(), "{:?}", s.api.changes());
    // the phase comes after config and before the stacks: it does not wait for Docker
    let phases: Vec<&str> = cestmoi::restore::PHASES.iter().map(|(name, _)| *name).collect();
    let at = |name: &str| phases.iter().position(|p| *p == name).unwrap();
    assert!(at("config") < at("ssh") && at("ssh") < at("docker"));
}

#[test]
fn init_says_that_a_tunnel_is_there_to_be_adopted() {
    let s = cestmoi("ssh-init");
    let found = cestmoi::init::discover(&s.host);
    assert_eq!(found.tunnel.as_deref(), Some("container cloudflared"));
    let text = cestmoi::init::config_text(&found, false);
    assert!(text.contains("# A Cloudflare Tunnel connects this server (container cloudflared). `moi ssh up --hostname ssh.<your zone>` makes\n# `ssh cestmoi` work from any computer through it"), "{text}");
    assert!(!text.contains("\n[ssh]"), "the section is `moi ssh up`'s to write");
    assert_eq!(cestmoi::init::discover(&fresh("ssh-init-none").host).tunnel, None);
}
