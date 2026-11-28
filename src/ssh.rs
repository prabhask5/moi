//! `moi ssh`: reaching the server with `ssh <name>` from any computer, through a path that does not
//! depend on Docker or the reverse proxy (docs/spec/16-ssh.md).
//!
//! The way in is a [`Provider`]: `cloudflare` (a tunnel connector as a host service, its ingress rule
//! and DNS record) or `none` (the server is reachable already). Everything that is not the provider's
//! — the client command, authorizing keys, the look at sshd — is here.

pub mod apply;
pub mod cloudflare;

use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::config::{Loaded, Secret, SshProvider};
use crate::docker::Docker;
use crate::error::Error;
use crate::host::{Cmd, Host};

use cloudflare::{Api, ApiError, Connector, Dns, Merge, Tunnel};

/// The secrets `moi ssh up` looks for before `[ssh]` exists, and writes references to.
pub const API_TOKEN_SECRET: &str = "cloudflare_api_token";
pub const TUNNEL_TOKEN_SECRET: &str = "cloudflare_tunnel_token";
const CLIENT_RELEASES: &str = "https://github.com/prabhask5/devbox/releases/download";

/// What `[ssh]` says, with every default filled in.
#[derive(Debug, Clone)]
pub struct Settings {
    pub provider: SshProvider,
    pub hostname: String,
    pub alias: String,
    pub user: String,
    pub api_token: Option<Secret>,
    pub tunnel_token: Option<Secret>,
    pub metrics: String,
    pub client_release: String,
    pub monitor: Option<String>,
    /// Whether `[ssh]` in `moi.toml` already says all of this.
    pub recorded: bool,
}

/// What `[ssh]` says, for the parts of moi that only read (the check, the snapshot, the context).
/// `None` when the section is absent, switched off or has no hostname.
pub fn settings_of(host: &dyn Host, config: &crate::config::Config) -> Option<Settings> {
    let ssh = &config.ssh;
    let hostname = ssh.hostname.clone().filter(|_| ssh.enabled)?;
    Some(Settings {
        provider: ssh.provider,
        hostname,
        alias: ssh.alias.clone().unwrap_or_else(|| config.server.name.clone()),
        user: ssh
            .user
            .clone()
            .or_else(|| host.env("SUDO_USER"))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "root".to_string()),
        api_token: ssh.api_token.clone(),
        tunnel_token: ssh.tunnel_token.clone(),
        metrics: ssh.metrics.clone(),
        client_release: ssh.client_release.clone(),
        monitor: ssh.monitor.clone(),
        recorded: true,
    })
}

/// The settings of this run: the command line, then `[ssh]`, then what can be found out.
pub fn settings(host: &dyn Host, loaded: &Loaded, hostname: Option<&str>) -> Result<Settings, Error> {
    let ssh = &loaded.config.ssh;
    let Some(name) = hostname.map(str::to_string).or_else(|| ssh.hostname.clone()) else {
        return Err(Error::Usage("which host name should answer SSH? `moi ssh up --hostname ssh.example.com` (a name in a zone of your Cloudflare account)".to_string()));
    };
    let secret = |configured: &Option<Secret>, name: &str| {
        configured.clone().or_else(|| crate::config::secret_named(host, &loaded.paths, name))
    };
    Ok(Settings {
        provider: ssh.provider,
        recorded: ssh.enabled && ssh.hostname.as_deref() == Some(name.as_str()),
        hostname: name,
        alias: ssh.alias.clone().unwrap_or_else(|| loaded.config.server.name.clone()),
        user: ssh
            .user
            .clone()
            .or_else(|| host.env("SUDO_USER"))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "root".to_string()),
        api_token: secret(&ssh.api_token, API_TOKEN_SECRET),
        tunnel_token: secret(&ssh.tunnel_token, TUNNEL_TOKEN_SECRET),
        metrics: ssh.metrics.clone(),
        client_release: ssh.client_release.clone(),
        monitor: if ssh.enabled { ssh.monitor.clone() } else { Some(crate::config::SSH_MONITOR.to_string()) },
    })
}

/// What a step of the plan amounts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// As it should be.
    Ok,
    /// Exists and becomes moi's to look after; nothing changes.
    Adopt,
    /// Will be created or changed.
    Change,
    /// Said, not done: something for the owner.
    Note,
    /// The run cannot go on.
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub name: &'static str,
    pub action: Action,
    pub text: String,
}

/// What `moi ssh up` would do, and everything the apply needs to do it.
#[derive(Debug, Clone)]
pub struct Plan {
    pub settings: Settings,
    pub steps: Vec<Step>,
    pub tunnel: Option<Tunnel>,
    /// The zone of the hostname: id and name.
    pub zone: Option<(String, String)>,
    /// What has to happen on the host: install the binary, the service, the drop-in.
    pub install_binary: bool,
    pub install_service: bool,
    pub write_drop_in: bool,
    /// Containers to stop.
    pub retire: Vec<Connector>,
    /// The ingress to put (remote), or the new `config.yml` (local), when it changes.
    pub ingress: Option<Vec<Value>>,
    pub local_config: Option<String>,
    pub create_dns: bool,
}

impl Plan {
    fn new(settings: Settings) -> Self {
        Self {
            settings,
            steps: Vec::new(),
            tunnel: None,
            zone: None,
            install_binary: false,
            install_service: false,
            write_drop_in: false,
            retire: Vec::new(),
            ingress: None,
            local_config: None,
            create_dns: false,
        }
    }

    fn step(&mut self, name: &'static str, action: Action, text: impl Into<String>) {
        self.steps.push(Step { name, action, text: text.into() });
    }

    /// Why nothing can be applied, if so.
    pub fn stopped(&self) -> Option<&Step> {
        self.steps.iter().find(|s| s.action == Action::Stop)
    }

    pub fn changes(&self) -> usize {
        self.steps.iter().filter(|s| s.action == Action::Change).count()
    }

    /// Whether applying it touches the path a session over the tunnel runs on.
    pub fn touches_the_path(&self) -> bool {
        self.install_service
            || self.write_drop_in
            || !self.retire.is_empty()
            || self.ingress.is_some()
            || self.local_config.is_some()
    }
}

/// A way into the server. Another one (Tailscale, a plain public port) is another implementation;
/// nothing else in `moi ssh` knows which it is.
pub trait Provider {
    /// Look at everything and say what `up` would do. Reads only.
    fn plan(&self, host: &Arc<dyn Host>, settings: Settings) -> Plan;

    /// Do the provider's part of a plan, every change recorded in `archive` before it is made.
    fn apply(&self, cx: &apply::Cx<'_>, plan: &Plan, archive: &Path) -> Result<Vec<String>, String>;

    /// Put back what the record in `archive` says was changed.
    fn down(
        &self,
        cx: &apply::Cx<'_>,
        archive: &Path,
        api_token: Option<Secret>,
        dry_run: bool,
    ) -> Result<Vec<String>, String>;

    /// Whether the way in works right now, in the `ssh` check's words.
    fn status(&self, host: &Arc<dyn Host>, settings: &Settings, probe: bool) -> (bool, String);
}

pub fn provider(kind: SshProvider) -> Box<dyn Provider> {
    match kind {
        SshProvider::Cloudflare => Box::new(Cloudflare),
        SshProvider::None => Box::new(Reachable),
    }
}

/// What moi's drop-in for cloudflared's unit says: the metrics address, through the variable
/// cloudflared reads for `--metrics`, so that the unit cloudflared wrote stays as it is.
pub fn drop_in(metrics: &str) -> String {
    format!("# Written by `moi ssh up`; removed by `moi ssh down`.\n[Service]\nEnvironment=TUNNEL_METRICS={metrics}\n")
}

/// sshd's own settings that matter once it has a public name. `None`: sshd could not be asked.
pub fn sshd_findings(host: &dyn Host) -> Option<Vec<String>> {
    let out = host.run(&Cmd::new("sshd").arg("-T")).ok().filter(|o| o.status == 0)?;
    let settings = out.stdout_str().to_lowercase();
    let value = |key: &str| {
        settings.lines().find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string())).unwrap_or_default()
    };
    let mut found = Vec::new();
    if value("passwordauthentication ") != "no" {
        found.push("sshd accepts passwords (PasswordAuthentication is not `no`)".to_string());
    }
    if value("permitrootlogin ") == "yes" {
        found.push("sshd lets root log in with a password (PermitRootLogin yes)".to_string());
    }
    Some(found)
}

fn sshd_step(host: &dyn Host, plan: &mut Plan) {
    match sshd_findings(host) {
        None => plan.step(
            "sshd",
            Action::Note,
            "could not be asked (`sshd -T`): check that passwords and root logins are off",
        ),
        Some(found) if found.is_empty() => {
            plan.step("sshd", Action::Ok, "public keys only, no root login with a password")
        }
        // an Attention entry of the audit, not a change: moi does not edit sshd
        Some(found) => plan.step(
            "sshd",
            Action::Note,
            format!("{} — moi does not change sshd; with a public name this is worth fixing", found.join("; ")),
        ),
    }
}

fn records_step(plan: &mut Plan) {
    let monitor = plan
        .settings
        .monitor
        .clone()
        .map(|m| format!(", the `ssh` check and its monitor \"{m}\""))
        .unwrap_or_else(|| ", the `ssh` check".to_string());
    if plan.settings.recorded {
        plan.step("records", Action::Ok, format!("[ssh] is in moi.toml{monitor}"));
    } else {
        plan.step("records", Action::Change, format!("write [ssh] to moi.toml; snapshot, CONTEXT.md{monitor}"));
    }
}

/// The server is reachable already: nothing to set up but the records.
struct Reachable;

impl Provider for Reachable {
    fn plan(&self, host: &Arc<dyn Host>, settings: Settings) -> Plan {
        let mut plan = Plan::new(settings);
        plan.step(
            "path",
            Action::Ok,
            format!("provider `none`: {} is reachable as it is; moi sets up nothing", plan.settings.hostname),
        );
        sshd_step(host.as_ref(), &mut plan);
        records_step(&mut plan);
        plan
    }

    fn apply(&self, _cx: &apply::Cx<'_>, _plan: &Plan, _archive: &Path) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    fn down(
        &self,
        cx: &apply::Cx<'_>,
        archive: &Path,
        _api_token: Option<Secret>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // only the records were written
        apply::down(cx, archive, None, dry_run)
    }

    fn status(&self, _host: &Arc<dyn Host>, _settings: &Settings, _probe: bool) -> (bool, String) {
        (true, "provider `none`: nothing of moi's is in the path".to_string())
    }
}

struct Cloudflare;

impl Provider for Cloudflare {
    fn apply(&self, cx: &apply::Cx<'_>, plan: &Plan, archive: &Path) -> Result<Vec<String>, String> {
        apply::apply(cx, plan, archive)
    }

    fn down(
        &self,
        cx: &apply::Cx<'_>,
        archive: &Path,
        api_token: Option<Secret>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        apply::down(cx, archive, api_token, dry_run)
    }

    fn status(&self, host: &Arc<dyn Host>, settings: &Settings, probe: bool) -> (bool, String) {
        health(host, settings, probe)
    }

    fn plan(&self, host: &Arc<dyn Host>, settings: Settings) -> Plan {
        let docker = Docker::new(Arc::clone(host));
        let mut plan = Plan::new(settings);
        let hostname = plan.settings.hostname.clone();

        // 1. the tunnel
        let tunnel = match cloudflare::discover(host.as_ref(), &docker, plan.settings.tunnel_token.as_ref()) {
            Ok(tunnel) => tunnel,
            Err(why) => {
                plan.step("tunnel", Action::Stop, why);
                return plan;
            }
        };
        let stored = if tunnel.token.is_some() && plan.settings.tunnel_token.is_none() {
            format!("; its token is stored as secret {TUNNEL_TOKEN_SECRET}")
        } else {
            String::new()
        };
        plan.step(
            "tunnel",
            Action::Ok,
            format!("{} of account {}, found in {}{stored}", tunnel.id, tunnel.account, tunnel.source.describe()),
        );
        plan.tunnel = Some(tunnel.clone());

        // 2. what the API token may do
        let Some(token) = plan.settings.api_token.clone() else {
            plan.step(
                "credentials",
                Action::Stop,
                format!("an API token is needed: create one in the Cloudflare dashboard (My Profile → API Tokens) with Account → Cloudflare Tunnel → Edit and Zone → DNS → Edit for the zone of {hostname}, and put it into secrets.toml as {API_TOKEN_SECRET}"),
            );
            return plan;
        };
        let api = Api::new(host, token);
        let mut missing: Vec<String> = Vec::new();
        let mut trouble: Option<String> = None;
        match api.tunnel(&tunnel) {
            Ok(_) => {}
            Err(ApiError::Forbidden) => missing.push("Account → Cloudflare Tunnel → Edit".to_string()),
            Err(ApiError::Unauthorized) => {
                trouble = Some(format!(
                    "Cloudflare does not accept the API token (secret {API_TOKEN_SECRET}): create a new one"
                ))
            }
            Err(ApiError::NotFound) => {
                trouble = Some(format!(
                    "Cloudflare knows no tunnel {} in account {}: was it deleted? (moi never creates tunnels)",
                    tunnel.id, tunnel.account
                ))
            }
            Err(e) => trouble = Some(e.to_string()),
        }
        if trouble.is_none() {
            match api.zone(&hostname) {
                Ok(Some(zone)) => plan.zone = Some(zone),
                Ok(None) | Err(ApiError::Forbidden) => {
                    missing.push(format!("Zone → DNS → Edit for the zone of {hostname}"))
                }
                Err(e) => trouble = Some(e.to_string()),
            }
        }
        if let Some(why) = trouble {
            plan.step("credentials", Action::Stop, why);
            return plan;
        }
        if !missing.is_empty() {
            plan.step(
                "credentials",
                Action::Stop,
                format!(
                    "the API token lacks: {} — add {} to the token (secret {API_TOKEN_SECRET})",
                    missing.join(" and "),
                    if missing.len() == 1 { "it" } else { "them" }
                ),
            );
            return plan;
        }
        let zone = plan.zone.clone().unwrap_or_default();
        plan.step("credentials", Action::Ok, format!("the API token may edit the tunnel and the DNS of {}", zone.1));

        // 3. the connector as a service of the host
        let have = |tool: &str| {
            ["/usr/bin", "/usr/local/bin", "/usr/sbin", "/bin"]
                .iter()
                .any(|dir| host.exists(&Path::new(dir).join(tool)))
        };
        let mut todo: Vec<String> = Vec::new();
        if !have("cloudflared") {
            if !have("apt-get") {
                plan.step("connector", Action::Stop, "cloudflared is not installed and this system has no apt: install it (https://pkg.cloudflare.com), then run `moi ssh up` again");
                return plan;
            }
            plan.install_binary = true;
            todo.push(
                "install cloudflared from Cloudflare's apt repository (signing key checked against the one moi knows)"
                    .to_string(),
            );
        }
        let unit = host.read_to_string(Path::new(cloudflare::UNIT)).unwrap_or_default();
        let serves_this = match tunnel.local_config() {
            Some(_) => !unit.is_empty(),
            None => unit.lines().filter_map(|l| l.trim().strip_prefix("ExecStart=")).any(|exec| {
                exec.split_whitespace().filter_map(cloudflare::decode_token).any(|(_, id)| id == tunnel.id)
            }),
        };
        let active = serves_this
            && host
                .run(&Cmd::new("systemctl").args(["is-active", "--quiet", "cloudflared.service"]))
                .is_ok_and(|o| o.status == 0);
        if !active {
            plan.install_service = true;
            todo.push(if serves_this {
                "start cloudflared.service".to_string()
            } else {
                "`cloudflared service install` (its own unit, started at boot)".to_string()
            });
        }
        if host.read_to_string(Path::new(cloudflare::DROP_IN)).ok().as_deref()
            != Some(drop_in(&plan.settings.metrics).as_str())
        {
            plan.write_drop_in = true;
            todo.push(format!("drop-in {} (metrics on {})", cloudflare::DROP_IN, plan.settings.metrics));
        }
        if todo.is_empty() {
            plan.step(
                "connector",
                Action::Ok,
                format!("cloudflared.service runs this tunnel on the host, metrics on {}", plan.settings.metrics),
            );
        } else {
            plan.step("connector", Action::Change, format!("{}; then wait for an edge connection", todo.join("; ")));
        }

        // 4. a container that serves the same tunnel
        for connector in cloudflare::connectors(&docker) {
            let same =
                connector.token.as_deref().and_then(cloudflare::decode_token).is_some_and(|(_, id)| id == tunnel.id);
            if !same || (!connector.running && connector.restart == "no") {
                continue;
            }
            match &connector.orchestrator {
                // it would be brought back: said, never stopped
                Some(how) => plan.step("container", Action::Note, format!("{} also serves this tunnel and is managed by {how} — moi does not stop it; `moi ssh status` shows the second connector until it is gone", connector.name)),
                None => {
                    let boot = connector.compose_dir.as_ref().map(|dir| format!("; its stack {dir} is left out of moi's boot.compose_up from now on")).unwrap_or_default();
                    plan.step("container", Action::Change, format!("stop {} and set its restart policy to `no` (was `{}`){boot}. Nothing is deleted", connector.name, connector.restart));
                    plan.retire.push(connector);
                }
            }
        }

        // 5. the ingress rule
        let merged = match tunnel.local_config() {
            Some(path) => {
                let yaml = host.read_to_string(Path::new(path)).unwrap_or_default();
                let (new, merge) = cloudflare::local_config_with(&yaml, &hostname, cloudflare::SERVICE);
                if merge != Merge::Unchanged {
                    plan.local_config = Some(new);
                }
                Ok(merge)
            }
            None => api.configuration(&tunnel).map(|config| {
                let rules = config["ingress"].as_array().cloned().unwrap_or_default();
                let (new, merge) = cloudflare::merge_ingress(&rules, &hostname, cloudflare::SERVICE);
                if merge != Merge::Unchanged {
                    plan.ingress = Some(new);
                }
                merge
            }),
        };
        match merged {
            Ok(Merge::Unchanged) => plan.step("ingress", Action::Adopt, format!("{hostname} → {} is there already: adopted, unchanged", cloudflare::SERVICE)),
            Ok(Merge::Service(old)) => plan.step("ingress", Action::Change, format!("{hostname} exists: adopted, its service changes from {old} to {} (every other rule stays as it is)", cloudflare::SERVICE)),
            Ok(Merge::Added) => plan.step("ingress", Action::Change, format!("add {hostname} → {} (every other rule stays, the catch-all stays last)", cloudflare::SERVICE)),
            Err(e) => {
                plan.step("ingress", Action::Stop, format!("the tunnel's configuration cannot be read: {e}"));
                return plan;
            }
        }

        // 6. DNS
        match api.dns_records(&zone.0, &hostname).map(|records| cloudflare::dns_action(&records, &tunnel.target())) {
            Ok(Dns::Adopt) => plan.step(
                "dns",
                Action::Adopt,
                format!("CNAME {hostname} → {} is there already: adopted", tunnel.target()),
            ),
            Ok(Dns::Create) => {
                plan.create_dns = true;
                plan.step("dns", Action::Change, format!("create a proxied CNAME {hostname} → {}", tunnel.target()));
            }
            Ok(Dns::Elsewhere(what)) => {
                plan.step("dns", Action::Stop, format!("{hostname} already points somewhere else ({what}). moi does not overwrite DNS it did not create: remove that record or choose another name"));
                return plan;
            }
            Err(e) => {
                plan.step("dns", Action::Stop, format!("the DNS records of {} cannot be read: {e}", zone.1));
                return plan;
            }
        }

        // 7–9
        sshd_step(host.as_ref(), &mut plan);
        plan.step("verify", Action::Note, format!("afterwards: an SSH handshake through https://{hostname}/"));
        records_step(&mut plan);
        plan
    }
}

/// The plan as people read it.
pub fn render(plan: &Plan) -> String {
    let mut out = format!(
        "moi ssh up — {} ({})\n",
        plan.settings.hostname,
        if plan.settings.provider == SshProvider::None { "provider none" } else { "Cloudflare Tunnel" }
    );
    for step in &plan.steps {
        let mark = match step.action {
            Action::Ok => "ok    ",
            Action::Adopt => "adopt ",
            Action::Change => "change",
            Action::Note => "note  ",
            Action::Stop => "STOP  ",
        };
        out.push_str(&format!("  {mark}  {:<12} {}\n", step.name, step.text));
    }
    if plan.stopped().is_some() {
        out.push_str("nothing was changed: the run cannot go on (see STOP above)\n");
    } else if plan.changes() == 0 {
        out.push_str("everything already up to date\n");
    }
    out
}

// ------------------------------------------------------------------------------------------------ the client side

/// Quote for a POSIX shell.
fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The one command for a computer, and what to say below it.
pub fn connect(host: &dyn Host, settings: &Settings) -> Result<String, Error> {
    let key_file = "/etc/ssh/ssh_host_ed25519_key.pub";
    let key = host
        .read_to_string(Path::new(key_file))
        .map_err(|e| Error::host(format!("cannot read the host key {key_file} (is sshd installed?)"), e))?;
    // type and key, without the comment (it names the machine, not the key)
    let host_key: Vec<&str> = key.split_whitespace().take(2).collect();
    if host_key.len() != 2 {
        return Err(Error::Usage(format!("{key_file} is not a public key")));
    }
    let release = &settings.client_release;
    let alias = &settings.alias;
    Ok(format!(
        "curl -fsSL {CLIENT_RELEASES}/{release}/devbox-ssh-setup.sh | sh -s -- add \\\n  --alias {alias} --url https://{} --user {} \\\n  --host-key {} --authorize-hint {}\n\n\
         Run this on each computer (macOS or Linux) that should reach the server with `ssh {alias}`.\n\
         It installs devbox's client `devbox-ssh` and writes one block of SSH configuration; both come from\n\
         devbox's release {release}. The same script with `remove --alias {alias}` undoes it.\n\
         If the computer's key is not allowed yet, the script prints it: on the server, run `sudo moi ssh authorize '<that line>'`.\n",
        settings.hostname,
        settings.user,
        quoted(&host_key.join(" ")),
        quoted("On the server, run: sudo moi ssh authorize"),
    ))
}

/// What happened to a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorized {
    Added(String),
    AlreadyThere(String),
}

/// A public key line as `authorized_keys` takes it: type, key, optional comment. The same types and
/// RSA minimum as devbox's pairing: Ed25519, ECDSA (NIST curves), their security-key forms, RSA from
/// 3072 bits.
pub fn valid_key(line: &str) -> Result<(String, String, String), String> {
    let mut parts = line.split_whitespace();
    let (Some(kind), Some(blob)) = (parts.next(), parts.next()) else {
        return Err("a public key line looks like `ssh-ed25519 AAAA… comment` (the content of a .pub file)".to_string());
    };
    let comment = parts.collect::<Vec<_>>().join(" ");
    let allowed = [
        "ssh-ed25519",
        "sk-ssh-ed25519@openssh.com",
        "ecdsa-sha2-nistp256",
        "ecdsa-sha2-nistp384",
        "ecdsa-sha2-nistp521",
        "sk-ecdsa-sha2-nistp256@openssh.com",
        "ssh-rsa",
    ];
    if !allowed.contains(&kind) {
        return Err(format!(
            "`{kind}` is not a key type moi accepts (Ed25519, ECDSA or RSA of 3072 bits and more); is this a private key or an options line?"
        ));
    }
    let raw = cloudflare::base64_decode(blob)
        .filter(|raw| blob.len() >= 16 && raw.len() > 4)
        .ok_or_else(|| "the key itself is not base64: copy the whole line of the .pub file".to_string())?;
    // the blob begins with its own type: length, then the name
    let field = |at: usize| -> Option<(&[u8], usize)> {
        let len = usize::try_from(u32::from_be_bytes(raw.get(at..at + 4)?.try_into().ok()?)).ok()?;
        Some((raw.get(at + 4..at + 4 + len)?, at + 4 + len))
    };
    let (inner, mut at) = field(0).ok_or_else(|| "the key is cut off".to_string())?;
    if inner != kind.as_bytes() {
        return Err("the key does not match the type written in front of it".to_string());
    }
    if kind == "ssh-rsa" {
        // exponent, then the modulus: its length is the key's size
        let modulus = field(at).and_then(|(_, next)| {
            at = next;
            field(at)
        });
        let bits = modulus.map_or(0, |(m, _)| m.iter().skip_while(|b| **b == 0).count() * 8);
        if bits < 3072 {
            return Err(format!(
                "this RSA key has {bits} bits; moi accepts RSA from 3072 bits (better: `ssh-keygen -t ed25519`)"
            ));
        }
    }
    Ok((kind.to_string(), blob.to_string(), comment))
}

/// Allow a key to log in as `user`: appended to the user's `authorized_keys`, once.
pub fn authorize(host: &dyn Host, user: &str, key: &str, name: Option<&str>) -> Result<Authorized, Error> {
    // a file with the key in it, or the line itself
    let line = match host.read_to_string(Path::new(key.trim())) {
        Ok(content) if !key.trim().contains(' ') => content,
        _ => key.to_string(),
    };
    let line = line.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or_default();
    let (kind, blob, comment) = valid_key(line).map_err(Error::Usage)?;
    let passwd = host.read_to_string(Path::new("/etc/passwd")).unwrap_or_default();
    let account: Vec<&str> =
        passwd.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.first() == Some(&user)).unwrap_or_default();
    let (Some(uid), Some(gid), Some(home)) = (
        account.get(2).and_then(|v| v.parse::<u32>().ok()),
        account.get(3).and_then(|v| v.parse::<u32>().ok()),
        account.get(5),
    ) else {
        return Err(Error::Usage(format!(
            "there is no user {user} on this machine ([ssh] user in moi.toml says who logs in)"
        )));
    };
    let dir = Path::new(home).join(".ssh");
    let file = dir.join("authorized_keys");
    let label = name.map(str::to_string).filter(|n| !n.is_empty()).unwrap_or(comment);
    let existing = host.read_to_string(&file).unwrap_or_default();
    if existing.lines().any(|l| l.split_whitespace().any(|word| word == blob)) {
        return Ok(Authorized::AlreadyThere(label));
    }
    host.create_dir_all(&dir, 0o700).map_err(|e| Error::host(format!("cannot create {}", dir.display()), e))?;
    let _ = host.set_mode(&dir, 0o700);
    let _ = host.set_owner(&dir, uid, gid);
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(format!("{kind} {blob} {label}").trim_end());
    text.push('\n');
    host.write(&file, text.as_bytes(), 0o600)
        .map_err(|e| Error::host(format!("cannot write {}", file.display()), e))?;
    let _ = host.set_mode(&file, 0o600);
    let _ = host.set_owner(&file, uid, gid);
    Ok(Authorized::Added(label))
}

// ------------------------------------------------------------------------------------------------ health and status

/// The tunnel's health as the `ssh` check reports it: up or down, and the message (stable: monitors
/// and people read it). `probe` adds the handshake through the public name.
pub fn health(host: &Arc<dyn Host>, settings: &Settings, probe: bool) -> (bool, String) {
    let running = host
        .run(&Cmd::new("systemctl").args(["is-active", "--quiet", "cloudflared.service"]))
        .is_ok_and(|o| o.status == 0);
    if !running {
        return (false, "SSH tunnel down: cloudflared not running".to_string());
    }
    let connections = match apply::ready(host, &settings.metrics) {
        Ok(0) => return (false, "SSH tunnel down: no edge connections".to_string()),
        Ok(n) => n,
        Err(error) => return (false, format!("SSH tunnel down: metrics not answering ({error})")),
    };
    let hostname = &settings.hostname;
    if !probe {
        return (true, format!("SSH tunnel up: {connections} edge connections ({hostname})"));
    }
    match apply::handshake(host, hostname) {
        Ok(ms) => (true, format!("SSH tunnel up: {connections} edge connections ({hostname}), handshake {ms} ms")),
        Err(e) => (false, format!("SSH tunnel down: public handshake failed ({}: {})", e.stage, e.error)),
    }
}

/// `SHA256:…` of a public key, as `ssh-keygen -l` prints it.
pub fn fingerprint(blob: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let raw = cloudflare::base64_decode(blob)?;
    Some(format!("SHA256:{}", crate::context::share::base64(&Sha256::digest(&raw)).trim_end_matches('=')))
}

/// The keys that may log in as `user`: fingerprint and comment, never the key.
pub fn authorized_keys(host: &dyn Host, user: &str) -> Vec<(String, String)> {
    let passwd = host.read_to_string(Path::new("/etc/passwd")).unwrap_or_default();
    let Some(home) = passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.first() == Some(&user))
        .and_then(|f| f.get(5).map(|h| (*h).to_string()))
    else {
        return Vec::new();
    };
    let file = host.read_to_string(&Path::new(&home).join(".ssh/authorized_keys")).unwrap_or_default();
    file.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|line| {
            // options may come first: the key is the word after its type
            let words: Vec<&str> = line.split_whitespace().collect();
            let at =
                words.iter().position(|w| w.starts_with("ssh-") || w.starts_with("ecdsa-") || w.starts_with("sk-"))?;
            Some((fingerprint(words.get(at + 1)?)?, words.get(at + 2..).unwrap_or_default().join(" ")))
        })
        .collect()
}

/// Everything `moi ssh status` shows.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub hostname: String,
    pub alias: String,
    pub provider: SshProvider,
    /// The tunnel's health: the check's message, with the public handshake.
    pub up: bool,
    pub message: String,
    pub connector_version: Option<String>,
    /// Containers that connect the same tunnel besides the host's service.
    pub other_connectors: Vec<String>,
    /// The ingress rule's service, when Cloudflare could be asked.
    pub ingress: Option<String>,
    pub dns: Option<String>,
    pub sshd: Vec<String>,
    pub keys: Vec<(String, String)>,
    /// A change made over the tunnel waits for `moi ssh confirm`.
    pub awaiting_confirmation: bool,
}

pub fn status(host: &Arc<dyn Host>, settings: &Settings, awaiting_confirmation: bool) -> Status {
    let docker = Docker::new(Arc::clone(host));
    let mut status = Status {
        hostname: settings.hostname.clone(),
        alias: settings.alias.clone(),
        provider: settings.provider,
        up: true,
        message: "provider `none`: nothing of moi's is in the path".to_string(),
        connector_version: None,
        other_connectors: Vec::new(),
        ingress: None,
        dns: None,
        sshd: sshd_findings(host.as_ref()).unwrap_or_default(),
        keys: authorized_keys(host.as_ref(), &settings.user),
        awaiting_confirmation,
    };
    (status.up, status.message) = provider(settings.provider).status(host, settings, true);
    if settings.provider == SshProvider::None {
        return status;
    }
    status.connector_version = host
        .run(&Cmd::new("cloudflared").arg("--version"))
        .ok()
        .filter(|o| o.status == 0)
        .map(|o| o.stdout_str().trim().to_string());
    if let Ok(tunnel) = cloudflare::discover(host.as_ref(), &docker, settings.tunnel_token.as_ref()) {
        status.other_connectors = cloudflare::connectors(&docker)
            .into_iter()
            .filter(|c| {
                c.running
                    && c.token.as_deref().and_then(cloudflare::decode_token).is_some_and(|(_, id)| id == tunnel.id)
            })
            .map(|c| c.name)
            .collect();
        if let Some(token) = settings.api_token.clone() {
            let api = Api::new(host, token);
            let rules = match tunnel.local_config() {
                Some(path) => {
                    Some(cloudflare::local_ingress(&host.read_to_string(Path::new(path)).unwrap_or_default()))
                }
                None => api.configuration(&tunnel).ok().map(|c| c["ingress"].as_array().cloned().unwrap_or_default()),
            };
            status.ingress = rules.map(|rules| {
                rules.iter().find(|r| r["hostname"] == settings.hostname.as_str()).map_or_else(
                    || "no rule for this name".to_string(),
                    |r| r["service"].as_str().unwrap_or_default().to_string(),
                )
            });
            status.dns = api
                .zone(&settings.hostname)
                .ok()
                .flatten()
                .and_then(|zone| api.dns_records(&zone.0, &settings.hostname).ok())
                .map(|records| match cloudflare::dns_action(&records, &tunnel.target()) {
                    Dns::Adopt => format!("CNAME → {}", tunnel.target()),
                    Dns::Create => "no record".to_string(),
                    Dns::Elsewhere(what) => format!("points elsewhere: {what}"),
                });
        }
    }
    status
}

pub fn render_status(status: &Status) -> String {
    let mut out = format!(
        "ssh {} — {}\n  {} {}\n",
        status.alias,
        status.hostname,
        if status.up { "✓" } else { "✗" },
        status.message
    );
    if let Some(version) = &status.connector_version {
        out.push_str(&format!("  connector  {version}\n"));
    }
    for name in &status.other_connectors {
        out.push_str(&format!("  ⚠ container {name} still connects the same tunnel: stop it where it is managed\n"));
    }
    if let Some(ingress) = &status.ingress {
        out.push_str(&format!("  ingress    {} → {ingress}\n", status.hostname));
    }
    if let Some(dns) = &status.dns {
        out.push_str(&format!("  dns        {dns}\n"));
    }
    for finding in &status.sshd {
        out.push_str(&format!("  ⚠ {finding}\n"));
    }
    out.push_str(&format!("  keys       {} allowed to log in\n", status.keys.len()));
    for (fingerprint, comment) in &status.keys {
        out.push_str(&format!("             {fingerprint} {comment}\n"));
    }
    if status.awaiting_confirmation {
        out.push_str("  ⚠ a change made over the tunnel waits for `moi ssh confirm` (from a new connection); without it, it is undone\n");
    }
    out
}
