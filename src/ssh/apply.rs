//! Doing what a plan says, undoing it, and the safeguard for doing either through the tunnel itself.
//!
//! Everything `up` changes is written into a record (`<state>/archive/ssh/<time>/record.json`, with
//! copies of the files it replaces) *before* it is changed, so that `down` — by hand, or by the
//! rollback timer when a change made over the tunnel is not confirmed — puts back exactly that.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cloudflare::{self, Api, Merge, Tunnel};
use super::{Plan, Settings};
use crate::config::{Loaded, Secret};
use crate::docker::Docker;
use crate::error::Error;
use crate::host::{self, Cmd, Host};
use crate::net::Client;
use crate::state::StateDir;
use crate::state::db::Db;

/// How long the owner has to confirm a change made over the tunnel.
pub const CONFIRM: Duration = Duration::from_secs(300);
pub const ROLLBACK_UNIT: &str = "moi-ssh-rollback";
pub const APPLY_UNIT: &str = "moi-ssh-apply";
const PENDING: &str = "ssh.pending";
/// Compose stacks whose only job was the connector moi retired: the boot job leaves them alone.
pub const RETIRED_STACKS: &str = "ssh.retired_stacks";

/// Cloudflare's package signing key, as published at `KEY_URL`: the SHA-256 of the file. A different
/// file stops the installation (docs/spec/14-security.md).
pub const KEY_URL: &str = "https://pkg.cloudflare.com/cloudflare-main.gpg";
pub const KEY_SHA256: &str = "1bd95f4082b320d541bee351560fc2765aa9f9cd8efa4c9e32135e63f252721d";
const KEYRING: &str = "/usr/share/keyrings/cloudflare-main.gpg";
const APT_LIST: &str = "/etc/apt/sources.list.d/cloudflared.list";
const BLOCK_BEGIN: &str = "# >>> moi ssh up (managed block: `moi ssh down` removes it)";
const BLOCK_END: &str = "# <<< moi ssh up";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Retired {
    pub id: String,
    pub name: String,
    pub restart: String,
    pub running: bool,
    pub compose_dir: Option<String>,
}

/// What an `up` changed, for putting it back.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub hostname: String,
    pub account: String,
    pub tunnel: String,
    /// The locally managed tunnel's config file, when it was edited (its old content is in `files/`).
    pub local_config: Option<String>,
    /// The ingress was changed; `rule_before` is the rule for the hostname as it was (`None`: there was none).
    pub ingress_changed: bool,
    pub rule_before: Option<Value>,
    /// The DNS record moi created: zone id and record id.
    pub created_dns: Option<(String, String)>,
    pub retired: Vec<Retired>,
    pub drop_in_written: bool,
    pub drop_in_before: Option<String>,
    pub service_installed: bool,
    pub service_started: bool,
    pub binary_installed: bool,
    pub wrote_config: bool,
    pub secret_added: bool,
    pub undone: bool,
}

#[derive(Debug)]
pub struct Cx<'a> {
    pub host: &'a Arc<dyn Host>,
    pub loaded: &'a Loaded,
    pub state: &'a StateDir,
    pub db: &'a Db,
}

impl Cx<'_> {
    fn run(&self, program: &str, args: &[&str]) -> Result<(), String> {
        let line = format!("{program} {}", args.join(" "));
        match self.host.run(&Cmd::new(program).args(args.iter().copied()).timeout(Duration::from_secs(600))) {
            Ok(out) if out.status == 0 => Ok(()),
            Ok(out) => Err(format!(
                "`{}` failed: {}",
                self.loaded.masker.mask(&line),
                self.loaded.masker.mask(out.stderr_str().trim())
            )),
            Err(e) => Err(format!("`{}` could not run: {e}", self.loaded.masker.mask(&line))),
        }
    }

    fn api(&self, settings: &Settings) -> Result<Api, String> {
        settings.api_token.clone().map(|token| Api::new(self.host, token)).ok_or_else(|| "no API token".to_string())
    }
}

pub fn archive_root(state: &StateDir) -> PathBuf {
    state.archive().join("ssh")
}

fn save(host: &dyn Host, archive: &Path, record: &Record) -> Result<(), String> {
    host.create_dir_all(archive, 0o700).map_err(|e| format!("cannot create {}: {e}", archive.display()))?;
    let text = serde_json::to_string_pretty(record).unwrap_or_default();
    host.write(&archive.join("record.json"), text.as_bytes(), 0o600)
        .map_err(|e| format!("cannot write the record in {}: {e}", archive.display()))
}

pub fn load(host: &dyn Host, archive: &Path) -> Option<Record> {
    serde_json::from_slice(&host.read(&archive.join("record.json")).ok()?).ok()
}

/// The newest record that has not been undone.
pub fn latest(host: &dyn Host, state: &StateDir) -> Option<(PathBuf, Record)> {
    let root = archive_root(state);
    let mut names = host.read_dir(&root).unwrap_or_default();
    names.sort();
    names
        .into_iter()
        .rev()
        .map(|n| root.join(n))
        .find_map(|dir| load(host, &dir).filter(|r| !r.undone).map(|r| (dir, r)))
}

/// A new archive directory, named by the time.
pub fn new_archive(host: &dyn Host, state: &StateDir) -> PathBuf {
    let stamp: String = host::format_rfc3339(host.now()).chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    archive_root(state).join(stamp)
}

fn keep_file(host: &dyn Host, archive: &Path, path: &str) -> Result<(), String> {
    let Ok(content) = host.read(Path::new(path)) else { return Ok(()) };
    let copy = archive.join("files").join(path.trim_start_matches('/'));
    if host.exists(&copy) {
        return Ok(());
    }
    if let Some(parent) = copy.parent() {
        host.create_dir_all(parent, 0o700).map_err(|e| format!("cannot archive {path}: {e}"))?;
    }
    host.write(&copy, &content, 0o600).map_err(|e| format!("cannot archive {path}: {e}"))
}

/// cloudflared's own word on whether the tunnel is connected: the number of edge connections.
pub fn ready(host: &Arc<dyn Host>, metrics: &str) -> Result<u64, String> {
    let response = Client::new(Arc::clone(host))
        .with_timeout(Duration::from_secs(5))
        .send("GET", &format!("http://{metrics}/ready"), &[], None)
        .map_err(|e| e.to_string())?;
    let answer: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    match answer["readyConnections"].as_u64() {
        Some(n) => Ok(n),
        None => Err(format!("HTTP {} without a connection count", response.status)),
    }
}

/// The SSH handshake through the public name: how long it took, in milliseconds.
pub fn handshake(host: &Arc<dyn Host>, hostname: &str) -> Result<u64, crate::net::HandshakeError> {
    let started = host.monotonic();
    let first = Client::new(Arc::clone(host))
        .with_timeout(Duration::from_secs(15))
        .websocket_first_bytes(&format!("https://{hostname}/"), 8)?;
    if !first.starts_with(b"SSH-2.0-") {
        let shown: String = String::from_utf8_lossy(&first).chars().take(40).filter(|c| !c.is_control()).collect();
        return Err(crate::net::HandshakeError {
            stage: "banner",
            error: format!("what answers is not an SSH server (\"{shown}\")"),
        });
    }
    Ok(u64::try_from(host.monotonic().saturating_sub(started).as_millis()).unwrap_or(u64::MAX))
}

/// Steps 3 to 6 of the plan: the connector on the host, the old container, the ingress, the DNS.
/// Each change is in the record before it is made. Returns what was done, in words.
pub fn apply(cx: &Cx<'_>, plan: &Plan, archive: &Path) -> Result<Vec<String>, String> {
    let host = cx.host.as_ref();
    let Some(tunnel) = &plan.tunnel else { return Ok(Vec::new()) };
    let settings = &plan.settings;
    let mut record = load(host, archive).unwrap_or_default();
    record.hostname.clone_from(&settings.hostname);
    record.account.clone_from(&tunnel.account);
    record.tunnel.clone_from(&tunnel.id);
    save(host, archive, &record)?;
    let mut done = Vec::new();

    // 3. the connector
    if plan.install_binary {
        let key = Client::new(Arc::clone(cx.host))
            .with_timeout(Duration::from_secs(60))
            .send("GET", KEY_URL, &[], None)
            .map_err(|e| format!("cannot download Cloudflare's package key: {e}"))?;
        let have: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&key.body).iter().map(|b| format!("{b:02x}")).collect()
        };
        if !key.is_success() || have != KEY_SHA256 {
            return Err(format!(
                "the key at {KEY_URL} is not the one moi knows (sha256 {have}): cloudflared is not installed. If Cloudflare changed its key, install cloudflared yourself and run `moi ssh up` again"
            ));
        }
        for file in [KEYRING, APT_LIST] {
            if let Some(dir) = Path::new(file).parent() {
                host.create_dir_all(dir, 0o755).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
        }
        host.write(Path::new(KEYRING), &key.body, 0o644).map_err(|e| format!("cannot write {KEYRING}: {e}"))?;
        host.write(
            Path::new(APT_LIST),
            format!("deb [signed-by={KEYRING}] https://pkg.cloudflare.com/cloudflared any main\n").as_bytes(),
            0o644,
        )
        .map_err(|e| format!("cannot write {APT_LIST}: {e}"))?;
        record.binary_installed = true;
        save(host, archive, &record)?;
        cx.run("apt-get", &["update", "-q"])?;
        cx.run("apt-get", &["install", "-y", "-q", "cloudflared"])?;
        done.push("cloudflared installed from Cloudflare's repository".to_string());
    }
    if plan.install_service {
        let unit = host.read_to_string(Path::new(cloudflare::UNIT)).unwrap_or_default();
        if unit.is_empty() {
            record.service_installed = true;
            save(host, archive, &record)?;
            match &tunnel.token {
                Some(token) => cx.run("cloudflared", &["service", "install", token.expose()])?,
                None => cx.run("cloudflared", &["service", "install"])?,
            }
            done.push("cloudflared.service installed".to_string());
        } else if plan.steps.iter().any(|s| s.name == "connector" && s.text.contains("start cloudflared.service")) {
            record.service_started = true;
            save(host, archive, &record)?;
            cx.run("systemctl", &["enable", "--now", "cloudflared.service"])?;
            done.push("cloudflared.service started".to_string());
        } else {
            return Err(format!(
                "{} belongs to another tunnel: moi does not replace it. Remove it (`cloudflared service uninstall`) or use that tunnel",
                cloudflare::UNIT
            ));
        }
    }
    if plan.write_drop_in {
        record.drop_in_before = host.read_to_string(Path::new(cloudflare::DROP_IN)).ok();
        record.drop_in_written = true;
        save(host, archive, &record)?;
        let dir = Path::new(cloudflare::DROP_IN).parent().unwrap_or(Path::new("/"));
        host.create_dir_all(dir, 0o755).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        host.write(Path::new(cloudflare::DROP_IN), super::drop_in(&settings.metrics).as_bytes(), 0o644)
            .map_err(|e| format!("cannot write {}: {e}", cloudflare::DROP_IN))?;
        cx.run("systemctl", &["daemon-reload"])?;
        cx.run("systemctl", &["restart", "cloudflared.service"])?;
        done.push(format!("metrics on {}", settings.metrics));
    }
    if plan.install_binary || plan.install_service || plan.write_drop_in {
        // an edge connection of the host's connector, before anything that worked is taken away
        let mut waited = 0;
        let connections = loop {
            match ready(cx.host, &settings.metrics) {
                Ok(n) if n > 0 => break n,
                last if waited >= 60 => {
                    return Err(format!(
                        "cloudflared.service has no connection to Cloudflare after 60 s ({}): journalctl -u cloudflared. Nothing that worked before was changed; `moi ssh down` removes what was installed",
                        last.map_or_else(|e| e, |_| "no edge connections".to_string())
                    ));
                }
                _ => {}
            }
            cx.host.sleep(Duration::from_secs(2));
            waited += 2;
        };
        done.push(format!("the host's connector has {connections} edge connections"));
    }

    // 4. the container that did this before
    if !plan.retire.is_empty() {
        let docker = Docker::new(Arc::clone(cx.host));
        for connector in &plan.retire {
            record.retired.push(Retired {
                id: connector.id.clone(),
                name: connector.name.clone(),
                restart: connector.restart.clone(),
                running: connector.running,
                compose_dir: connector.compose_dir.clone(),
            });
            save(host, archive, &record)?;
            docker
                .set_restart_policy(&connector.id, "no")
                .map_err(|e| format!("cannot change the restart policy of {}: {e}", connector.name))?;
            docker.stop(&connector.id).map_err(|e| format!("cannot stop {}: {e}", connector.name))?;
            done.push(format!("container {} stopped (restart policy `no`)", connector.name));
        }
        let stacks: Vec<&String> = record.retired.iter().filter_map(|r| r.compose_dir.as_ref()).collect();
        cx.db.set_setting(RETIRED_STACKS, &serde_json::to_string(&stacks).unwrap_or_default());
    }

    // 5. the ingress rule
    if let (Some(new), Some(path)) = (&plan.local_config, tunnel.local_config()) {
        keep_file(host, archive, path)?;
        record.local_config = Some(path.to_string());
        record.ingress_changed = true;
        save(host, archive, &record)?;
        host.write(Path::new(path), new.as_bytes(), 0o644).map_err(|e| format!("cannot write {path}: {e}"))?;
        cx.run("systemctl", &["restart", "cloudflared.service"])?;
        done.push(format!("ingress rule for {} in {path}", settings.hostname));
    } else if plan.ingress.is_some() {
        let api = cx.api(settings)?;
        // merged into what the tunnel has *now*, not what it had when the plan was made
        let mut config =
            api.configuration(tunnel).map_err(|e| format!("cannot read the tunnel's configuration: {e}"))?;
        let rules = config["ingress"].as_array().cloned().unwrap_or_default();
        record.rule_before = rules.iter().find(|r| r["hostname"] == settings.hostname.as_str()).cloned();
        record.ingress_changed = true;
        save(host, archive, &record)?;
        let (merged, change) = cloudflare::merge_ingress(&rules, &settings.hostname, cloudflare::SERVICE);
        if change != Merge::Unchanged {
            if !config.is_object() {
                config = serde_json::json!({});
            }
            config["ingress"] = Value::Array(merged);
            api.put_configuration(tunnel, &config).map_err(|e| format!("cannot change the tunnel's ingress: {e}"))?;
        }
        done.push(format!("ingress rule {} → {}", settings.hostname, cloudflare::SERVICE));
    }

    // 6. DNS
    if plan.create_dns {
        let (api, zone) = (cx.api(settings)?, plan.zone.clone().unwrap_or_default());
        let id = api
            .create_cname(&zone.0, &settings.hostname, &tunnel.target())
            .map_err(|e| format!("cannot create the DNS record: {e}"))?;
        record.created_dns = Some((zone.0, id));
        save(host, archive, &record)?;
        done.push(format!("CNAME {} → {}", settings.hostname, tunnel.target()));
    }
    Ok(done)
}

/// Step 9: `[ssh]` in `moi.toml` as a marked block, and the tunnel's token among the secrets.
pub fn write_records(cx: &Cx<'_>, plan: &Plan, archive: &Path) -> Result<Vec<String>, String> {
    let host = cx.host.as_ref();
    let settings = &plan.settings;
    let mut record = load(host, archive).unwrap_or_default();
    let mut done = Vec::new();
    let paths = &cx.loaded.paths;
    // the token found on the machine becomes a secret, so that a rebuild has it
    let token = plan.tunnel.as_ref().and_then(|t| t.token.clone());
    let have_secret = crate::config::secret_named(host, paths, super::TUNNEL_TOKEN_SECRET).is_some();
    if let (Some(token), false) = (&token, have_secret) {
        let mut text = host.read_to_string(&paths.secrets).unwrap_or_default();
        if !text.lines().any(|l| l.trim() == "[secrets]") {
            text.push_str("[secrets]\n");
        } else if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "{} = {}\n",
            super::TUNNEL_TOKEN_SECRET,
            toml::Value::String(token.expose().to_string())
        ));
        record.secret_added = true;
        save(host, archive, &record)?;
        host.write(&paths.secrets, text.as_bytes(), 0o600)
            .map_err(|e| format!("cannot write {}: {e}", paths.secrets.display()))?;
        done.push(format!("the tunnel's token is in {} as {}", paths.secrets.display(), super::TUNNEL_TOKEN_SECRET));
    }
    if !settings.recorded {
        let config_path = paths.config.to_string_lossy().into_owned();
        keep_file(host, archive, &config_path)?;
        let existing = host.read_to_string(&paths.config).unwrap_or_default();
        let mut text = remove_block(&existing);
        if cx.loaded.config.ssh.enabled {
            return Err(format!(
                "{config_path} has an [ssh] section of its own that says something else: change `hostname` there (moi does not rewrite a section it did not write)"
            ));
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let quote = |s: &str| toml::Value::String(s.to_string()).to_string();
        let mut block = format!(
            "\n{BLOCK_BEGIN}\n[ssh]\nprovider = {}\nhostname = {}\nalias = {}\nuser = {}\n",
            quote(if plan.tunnel.is_some() { "cloudflare" } else { "none" }),
            quote(&settings.hostname),
            quote(&settings.alias),
            quote(&settings.user)
        );
        if plan.tunnel.is_some() {
            block.push_str(&format!("api_token = \"secret:{}\"\n", super::API_TOKEN_SECRET));
            if token.is_some() {
                block.push_str(&format!("tunnel_token = \"secret:{}\"\n", super::TUNNEL_TOKEN_SECRET));
            }
            block.push_str(&format!("metrics = {}\n", quote(&settings.metrics)));
        }
        block.push_str(&format!(
            "client_release = {}\nmonitor = {}\n{BLOCK_END}\n",
            quote(&settings.client_release),
            quote(settings.monitor.as_deref().unwrap_or_default())
        ));
        text.push_str(&block);
        record.wrote_config = true;
        save(host, archive, &record)?;
        host.write(&paths.config, text.as_bytes(), 0o644).map_err(|e| format!("cannot write {config_path}: {e}"))?;
        done.push(format!("[ssh] written to {config_path}"));
    }
    Ok(done)
}

/// `moi.toml` without moi's `[ssh]` block.
fn remove_block(text: &str) -> String {
    let (Some(begin), Some(end)) = (text.find(BLOCK_BEGIN), text.find(BLOCK_END)) else { return text.to_string() };
    if end < begin {
        return text.to_string();
    }
    let before = text[..begin].trim_end_matches('\n');
    let after = text[end + BLOCK_END.len()..].trim_start_matches('\n');
    match (before.is_empty(), after.is_empty()) {
        (true, _) => after.to_string(),
        (false, true) => format!("{before}\n"),
        (false, false) => format!("{before}\n\n{after}"),
    }
}

/// What `down` would do / did, in words.
pub fn down(cx: &Cx<'_>, archive: &Path, settings_token: Option<Secret>, dry_run: bool) -> Result<Vec<String>, String> {
    let host = cx.host.as_ref();
    let Some(mut record) = load(host, archive) else {
        return Err(format!("{} holds no record of an `up`", archive.display()));
    };
    let mut said = Vec::new();
    let mut act = |what: String, doing: &mut dyn FnMut() -> Result<(), String>| -> Result<(), String> {
        if dry_run {
            said.push(format!("would {what}"));
            Ok(())
        } else {
            doing()?;
            said.push(what);
            Ok(())
        }
    };
    let tunnel = Tunnel {
        account: record.account.clone(),
        id: record.tunnel.clone(),
        token: None,
        source: cloudflare::Source::Config,
    };
    let api = settings_token.map(|token| Api::new(cx.host, token));

    // the old connector first: a way in exists again before the new one is taken away
    let docker = Docker::new(Arc::clone(cx.host));
    for retired in &record.retired {
        act(
            format!(
                "give container {} its restart policy `{}` back{}",
                retired.name,
                retired.restart,
                if retired.running { " and start it" } else { "" }
            ),
            &mut || {
                docker
                    .set_restart_policy(&retired.id, &retired.restart)
                    .map_err(|e| format!("cannot restore the restart policy of {}: {e}", retired.name))?;
                if retired.running {
                    docker.start(&retired.id).map_err(|e| format!("cannot start {}: {e}", retired.name))?;
                }
                Ok(())
            },
        )?;
    }
    if let Some(path) = &record.local_config {
        let copy = archive.join("files").join(path.trim_start_matches('/'));
        act(format!("put {path} back as it was"), &mut || {
            let old = host.read(&copy).map_err(|e| format!("the archived copy of {path} cannot be read: {e}"))?;
            host.write(Path::new(path), &old, 0o644).map_err(|e| format!("cannot write {path}: {e}"))
        })?;
    } else if record.ingress_changed {
        let what = match &record.rule_before {
            Some(rule) => format!(
                "set the ingress rule of {} back to {}",
                record.hostname,
                rule["service"].as_str().unwrap_or_default()
            ),
            None => format!("remove the ingress rule of {} (every other rule stays)", record.hostname),
        };
        act(what, &mut || {
            let api = api.as_ref().ok_or_else(|| {
                format!("the API token is needed to put the ingress back (secret {})", super::API_TOKEN_SECRET)
            })?;
            let mut config =
                api.configuration(&tunnel).map_err(|e| format!("cannot read the tunnel's configuration: {e}"))?;
            let mut rules = config["ingress"].as_array().cloned().unwrap_or_default();
            match &record.rule_before {
                Some(before) => {
                    for rule in rules.iter_mut().filter(|r| r["hostname"] == record.hostname.as_str()) {
                        *rule = before.clone();
                    }
                }
                None => rules.retain(|r| r["hostname"] != record.hostname.as_str()),
            }
            config["ingress"] = Value::Array(rules);
            api.put_configuration(&tunnel, &config).map_err(|e| format!("cannot change the tunnel's ingress: {e}"))
        })?;
    }
    if let Some((zone, id)) = &record.created_dns {
        act(format!("delete the DNS record of {} (moi created it)", record.hostname), &mut || {
            let api = api.as_ref().ok_or_else(|| {
                format!("the API token is needed to delete the DNS record (secret {})", super::API_TOKEN_SECRET)
            })?;
            match api.delete_record(zone, id) {
                Ok(()) | Err(cloudflare::ApiError::NotFound) => Ok(()),
                Err(e) => Err(format!("cannot delete the DNS record: {e}")),
            }
        })?;
    }
    if record.drop_in_written {
        let before = record.drop_in_before.clone();
        act(format!("{} {}", if before.is_some() { "put back" } else { "remove" }, cloudflare::DROP_IN), &mut || {
            match &before {
                Some(old) => {
                    host.write(Path::new(cloudflare::DROP_IN), old.as_bytes(), 0o644).map_err(|e| e.to_string())?
                }
                None => {
                    host.remove_file(Path::new(cloudflare::DROP_IN)).map_err(|e| e.to_string())?;
                    // the directory was moi's too if nothing else is in it
                    if let Some(dir) = Path::new(cloudflare::DROP_IN).parent()
                        && host.read_dir(dir).is_ok_and(|names| names.is_empty())
                    {
                        let _ = host.remove_dir(dir);
                    }
                }
            }
            cx.run("systemctl", &["daemon-reload"])
        })?;
    }
    if record.service_installed {
        act("uninstall cloudflared.service (`cloudflared service uninstall`)".to_string(), &mut || {
            cx.run("cloudflared", &["service", "uninstall"])
        })?;
    } else if record.service_started {
        act("stop and disable cloudflared.service (it was not running before)".to_string(), &mut || {
            cx.run("systemctl", &["disable", "--now", "cloudflared.service"])
        })?;
    } else if record.drop_in_written || record.local_config.is_some() {
        act("restart cloudflared.service".to_string(), &mut || {
            cx.run("systemctl", &["restart", "cloudflared.service"])
        })?;
    }
    if record.wrote_config {
        let path = cx.loaded.paths.config.clone();
        act(format!("remove moi's [ssh] block from {}", path.display()), &mut || {
            let text = host.read_to_string(&path).map_err(|e| e.to_string())?;
            host.write(&path, remove_block(&text).as_bytes(), 0o644).map_err(|e| e.to_string())
        })?;
    }
    if record.secret_added {
        let path = cx.loaded.paths.secrets.clone();
        act(format!("remove {} from {}", super::TUNNEL_TOKEN_SECRET, path.display()), &mut || {
            let text = host.read_to_string(&path).map_err(|e| e.to_string())?;
            let kept: Vec<&str> = text
                .lines()
                .filter(|l| l.split('=').next().map(str::trim) != Some(super::TUNNEL_TOKEN_SECRET))
                .collect();
            host.write(&path, (kept.join("\n") + "\n").as_bytes(), 0o600).map_err(|e| e.to_string())
        })?;
    }
    if record.binary_installed {
        said.push("the cloudflared package and Cloudflare's apt repository stay installed (`apt-get remove cloudflared` if you do not want them)".to_string());
    }
    if !dry_run {
        cx.db.unset_setting(RETIRED_STACKS);
        cx.db.unset_setting(PENDING);
        record.undone = true;
        save(host, archive, &record)?;
    }
    Ok(said)
}

/// How an `up` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Done and kept.
    Applied(Vec<String>),
    /// Done over the tunnel: kept only after `moi ssh confirm` from a new connection.
    AwaitingConfirmation(Vec<String>),
}

/// Apply a plan: directly, or — when the session runs through the path being changed — behind the
/// rollback timer and in a unit of its own.
pub fn up(cx: &Cx<'_>, plan: &Plan) -> Result<(Outcome, PathBuf), Error> {
    let host = cx.host.as_ref();
    let archive = new_archive(host, cx.state);
    let usage = Error::Usage;
    // through the tunnel: the rule the session came in by exists and is about to be touched
    let rule_exists = plan.steps.iter().any(|s| s.name == "ingress" && !s.text.starts_with("add "));
    let over_tunnel = host.env("SSH_CONNECTION").is_some() && rule_exists && plan.touches_the_path();
    if !over_tunnel {
        let done = super::provider(plan.settings.provider)
            .apply(cx, plan, &archive)
            .map_err(|why| usage(format!("{why}\n`moi ssh down` puts back what was changed so far")))?;
        return Ok((Outcome::Applied(done), archive));
    }
    let moi = crate::init::binary(host)?;
    save(host, &archive, &Record { hostname: plan.settings.hostname.clone(), ..Record::default() }).map_err(usage)?;
    let archive_text = archive.to_string_lossy().into_owned();
    // the way back first, outside this session
    let seconds = format!("--on-active={}s", CONFIRM.as_secs());
    let timer = Cmd::new("systemd-run").args([
        "--quiet",
        "--collect",
        &format!("--unit={ROLLBACK_UNIT}"),
        seconds.as_str(),
        moi,
        "ssh",
        "down",
        "--rollback",
        archive_text.as_str(),
    ]);
    if !host.run(&timer).is_ok_and(|o| o.status == 0) {
        return Err(usage("not changing the tunnel through itself without a way back: systemd-run could not schedule the rollback. Nothing was changed; run this at the console instead".to_string()));
    }
    // the change in a unit of its own: a session that drops does not stop it halfway
    let inner = Cmd::new("systemd-run")
        .args([
            "--quiet",
            "--collect",
            "--wait",
            &format!("--unit={APPLY_UNIT}"),
            moi,
            "ssh",
            "up",
            "--hostname",
            plan.settings.hostname.as_str(),
            "--yes",
            "--inner",
            archive_text.as_str(),
        ])
        .timeout(Duration::from_secs(900));
    if !host.run(&inner).is_ok_and(|o| o.status == 0) {
        let put_back =
            super::provider(plan.settings.provider).down(cx, &archive, plan.settings.api_token.clone(), false);
        let _ = host.run(&Cmd::new("systemctl").args(["stop", &format!("{ROLLBACK_UNIT}.timer")]));
        return Err(usage(format!(
            "the change failed (journalctl -u {APPLY_UNIT}); {}",
            match put_back {
                Ok(_) => "everything is as it was".to_string(),
                Err(e) => format!(
                    "putting it back failed too: {e} — the rollback timer may already have been stopped: run `moi ssh down`"
                ),
            }
        )));
    }
    cx.db.set_setting(PENDING, &archive_text);
    let done = vec![format!(
        "the tunnel was changed in the unit {APPLY_UNIT}; the rollback is armed ({ROLLBACK_UNIT}, {} minutes)",
        CONFIRM.as_secs() / 60
    )];
    Ok((Outcome::AwaitingConfirmation(done), archive))
}

/// The archive of a change that still waits for `moi ssh confirm`.
pub fn pending(db: &Db) -> Option<PathBuf> {
    db.setting(PENDING).map(PathBuf::from)
}

/// Keep a change made over the tunnel: the rollback is called off.
pub fn confirm(cx: &Cx<'_>) -> Result<String, Error> {
    let Some(archive) = pending(cx.db) else {
        return Ok("nothing waits for confirmation".to_string());
    };
    if !cx
        .host
        .run(&Cmd::new("systemctl").args(["stop", &format!("{ROLLBACK_UNIT}.timer")]))
        .is_ok_and(|o| o.status == 0)
    {
        return Err(Error::Usage(format!(
            "the rollback timer could not be stopped: run `systemctl stop {ROLLBACK_UNIT}.timer` now, or the change is undone when it fires"
        )));
    }
    cx.db.unset_setting(PENDING);
    Ok(format!("kept: the change is permanent (its record is in {})", archive.display()))
}
