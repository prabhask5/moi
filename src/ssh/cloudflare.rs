//! The `cloudflare` provider: a Cloudflare Tunnel connector on the host, its ingress rule for the SSH
//! hostname and the DNS record that names the tunnel (docs/spec/16-ssh.md).
//!
//! moi never creates a tunnel and never touches a rule or a record that is not its own: ingress
//! rules are merged by hostname, DNS that points elsewhere is refused.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Secret;
use crate::docker::Docker;
use crate::host::Host;
use crate::net::Client;

pub const API: &str = "https://api.cloudflare.com/client/v4";
/// Where an SSH rule sends connections: the host's own sshd.
pub const SERVICE: &str = "ssh://localhost:22";
pub const UNIT: &str = "/etc/systemd/system/cloudflared.service";
pub const DROP_IN: &str = "/etc/systemd/system/cloudflared.service.d/moi.conf";
const LOCAL_CONFIGS: [&str; 3] =
    ["/etc/cloudflared/config.yml", "/etc/cloudflared/config.yaml", "/root/.cloudflared/config.yml"];

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// Standard or URL-safe base64, padded or not.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut buffer, mut bits) = (0_u32, 0_u8);
    for c in input.bytes().filter(|c| *c != b'=') {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

/// Where a tunnel was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `[ssh] tunnel_token`.
    Config,
    /// A cloudflared container's environment or arguments.
    Container(String),
    /// The `--token` of a cloudflared unit.
    Unit(String),
    /// A locally managed tunnel: `config.yml` with `tunnel:` and `credentials-file:`.
    LocalConfig(String),
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Config => "[ssh] tunnel_token".to_string(),
            Source::Container(name) => format!("container {name}"),
            Source::Unit(path) => format!("the unit {path}"),
            Source::LocalConfig(path) => format!("{path} (locally managed)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tunnel {
    pub account: String,
    pub id: String,
    /// The connector's token; a locally managed tunnel has a credentials file instead.
    pub token: Option<Secret>,
    pub source: Source,
}

impl Tunnel {
    /// A locally managed tunnel keeps its ingress in a file on this machine.
    pub fn local_config(&self) -> Option<&str> {
        match &self.source {
            Source::LocalConfig(path) => Some(path),
            _ => None,
        }
    }

    pub fn target(&self) -> String {
        format!("{}.cfargotunnel.com", self.id)
    }
}

/// A token's payload: base64 JSON `{a, t, s}` — account, tunnel, secret.
pub fn decode_token(token: &str) -> Option<(String, String)> {
    let payload: Value = serde_json::from_slice(&base64_decode(token.trim())?).ok()?;
    let (account, tunnel) = (text(&payload["a"]), text(&payload["t"]));
    (!account.is_empty() && !tunnel.is_empty()).then(|| (account.to_string(), tunnel.to_string()))
}

/// A cloudflared container on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    pub id: String,
    pub name: String,
    pub running: bool,
    /// Its restart policy, to put back.
    pub restart: String,
    pub token: Option<String>,
    /// What would start it again if moi stopped it, in words; `None` when nothing would.
    pub orchestrator: Option<String>,
    /// The directory of the plain Compose project it belongs to (moi's own boot job would `up` it).
    pub compose_dir: Option<String>,
}

/// `--token X` or `--token=X` among arguments.
fn token_argument<'a>(args: impl Iterator<Item = &'a str>) -> Option<String> {
    let args: Vec<&str> = args.collect();
    args.iter().enumerate().find_map(|(i, arg)| match arg.strip_prefix("--token") {
        Some("") => args.get(i + 1).map(|t| (*t).to_string()),
        Some(rest) => rest.strip_prefix('=').map(str::to_string),
        None => None,
    })
}

/// The token a cloudflared unit runs with. Since cloudflared 2026.7.2 `service install` keeps it in a
/// file (`--token-file /etc/cloudflared/token`); older installs have it on the command line.
pub fn unit_token(host: &dyn Host, unit: &str) -> Option<String> {
    unit.lines().filter_map(|l| l.trim().strip_prefix("ExecStart=")).find_map(|exec| {
        let args: Vec<&str> = exec.split_whitespace().collect();
        token_argument(args.iter().copied()).or_else(|| {
            let file = args.iter().enumerate().find_map(|(i, arg)| match arg.strip_prefix("--token-file") {
                Some("") => args.get(i + 1).map(|f| (*f).to_string()),
                Some(rest) => rest.strip_prefix('=').map(str::to_string),
                None => None,
            })?;
            host.read_to_string(Path::new(&file)).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
        })
    })
}

/// Every cloudflared container, running or not.
pub fn connectors(docker: &Docker) -> Vec<Connector> {
    let mut out = Vec::new();
    for summary in docker.containers(true).unwrap_or_default() {
        if !text(&summary["Image"]).contains("cloudflared") {
            continue;
        }
        let Some(inspect) = summary["Id"].as_str().and_then(|id| docker.container(id).ok()) else { continue };
        let labels = &inspect["Config"]["Labels"];
        let env_token = inspect["Config"]["Env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find_map(|e| e.strip_prefix("TUNNEL_TOKEN=").map(str::to_string));
        let arguments = ["Entrypoint", "Cmd"]
            .iter()
            .flat_map(|key| inspect["Config"][*key].as_array().into_iter().flatten().filter_map(Value::as_str));
        let workdir = text(&labels["com.docker.compose.project.working_dir"]);
        // Something that would bring a stopped container back by itself: swarm reconciles its
        // services, Dokploy redeploys its stacks. A plain Compose project has nobody watching it.
        let orchestrator = if let Some(service) = labels["com.docker.swarm.service.name"].as_str() {
            Some(format!("the swarm service {service}: `docker service scale {service}=0`"))
        } else if workdir.starts_with("/etc/dokploy")
            || labels.as_object().is_some_and(|l| l.keys().any(|k| k.starts_with("dokploy.")))
        {
            Some("Dokploy: stop or delete it in Dokploy's interface".to_string())
        } else {
            None
        };
        out.push(Connector {
            id: text(&inspect["Id"]).to_string(),
            name: text(&inspect["Name"]).trim_start_matches('/').to_string(),
            running: inspect["State"]["Running"].as_bool().unwrap_or(false),
            restart: text(&inspect["HostConfig"]["RestartPolicy"]["Name"]).to_string(),
            token: env_token.or_else(|| token_argument(arguments)),
            compose_dir: (orchestrator.is_none() && !workdir.is_empty()).then(|| workdir.to_string()),
            orchestrator,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `key: value` at the top level of a small YAML file.
fn yaml_value(yaml: &str, key: &str) -> Option<String> {
    yaml.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix(':')?.trim();
        (!value.is_empty()).then(|| value.trim_matches(['"', '\'']).to_string())
    })
}

/// Find the tunnel, in the order of the spec. `Err` says what was looked at.
pub fn discover(host: &dyn Host, docker: &Docker, configured: Option<&Secret>) -> Result<Tunnel, String> {
    let from_token = |token: &str, source: Source| -> Result<Tunnel, String> {
        let (account, id) = decode_token(token).ok_or_else(|| format!("the token found in {} is not a Cloudflare Tunnel token (it should be the long string from the tunnel's install command)", source.describe()))?;
        Ok(Tunnel { account, id, token: Some(Secret::new(token.trim().to_string())), source })
    };
    if let Some(token) = configured {
        return from_token(token.expose(), Source::Config);
    }
    if let Some((connector, token)) = connectors(docker).into_iter().find_map(|c| c.token.clone().map(|t| (c, t))) {
        return from_token(&token, Source::Container(connector.name));
    }
    if let Ok(unit) = host.read_to_string(Path::new(UNIT))
        && let Some(token) = unit_token(host, &unit)
    {
        return from_token(&token, Source::Unit(UNIT.to_string()));
    }
    for path in LOCAL_CONFIGS {
        let Ok(yaml) = host.read_to_string(Path::new(path)) else { continue };
        let (Some(_), Some(credentials)) = (yaml_value(&yaml, "tunnel"), yaml_value(&yaml, "credentials-file")) else {
            continue;
        };
        let file: Value = host
            .read(Path::new(&credentials))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or(Value::Null);
        let (account, id) = (text(&file["AccountTag"]), text(&file["TunnelID"]));
        if account.is_empty() || id.is_empty() {
            return Err(format!(
                "{path} names the credentials file {credentials}, which cannot be read as a tunnel's credentials"
            ));
        }
        return Ok(Tunnel {
            account: account.to_string(),
            id: id.to_string(),
            token: None,
            source: Source::LocalConfig(path.to_string()),
        });
    }
    Err("no Cloudflare Tunnel was found on this machine (no [ssh] tunnel_token, no cloudflared container or service, no /etc/cloudflared/config.yml).\nCreate one in the Cloudflare dashboard — Zero Trust → Networks → Tunnels → Create a tunnel → Cloudflared — and put the token of its install command into secrets.toml as cloudflare_tunnel_token (moi never creates tunnels)".to_string())
}

// ------------------------------------------------------------------------------------------------ ingress

/// What merging the SSH rule into a tunnel's ingress did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// The rule is there and says what it should.
    Unchanged,
    /// The rule is there; only its service changes (from what it was).
    Service(String),
    Added,
}

/// The ingress with the rule for `hostname` in it: every other rule kept, in its order, the catch-all
/// last. An existing rule for the hostname keeps everything but its `service`.
pub fn merge_ingress(rules: &[Value], hostname: &str, service: &str) -> (Vec<Value>, Merge) {
    let mut out: Vec<Value> = rules.to_vec();
    if let Some(rule) = out.iter_mut().find(|r| text(&r["hostname"]) == hostname) {
        let old = text(&rule["service"]).to_string();
        if old == service {
            return (out, Merge::Unchanged);
        }
        rule["service"] = json!(service);
        return (out, Merge::Service(old));
    }
    let rule = json!({ "hostname": hostname, "service": service });
    // before the catch-all (the rule without a hostname, which must stay last); one is added if the
    // tunnel had none, because Cloudflare refuses an ingress without it
    match out.iter().rposition(|r| text(&r["hostname"]).is_empty()) {
        Some(last) if last + 1 == out.len() => out.insert(last, rule),
        _ => {
            out.push(rule);
            out.push(json!({ "service": "http_status:404" }));
        }
    }
    (out, Merge::Added)
}

/// The `ingress:` rules of a locally managed tunnel's `config.yml` (hostname and service of each).
pub fn local_ingress(yaml: &str) -> Vec<Value> {
    let mut rules: Vec<Value> = Vec::new();
    let mut inside = false;
    for line in yaml.lines() {
        if !line.starts_with([' ', '\t', '-']) && !line.trim().is_empty() {
            inside = line.trim_end() == "ingress:";
            continue;
        }
        if !inside {
            continue;
        }
        let trimmed = line.trim();
        let (starts, body) = match trimmed.strip_prefix("- ") {
            Some(body) => (true, body),
            None => (false, trimmed),
        };
        if starts {
            rules.push(json!({}));
        }
        if let (Some((key, value)), Some(rule)) = (body.split_once(':'), rules.last_mut())
            && ["hostname", "service", "path"].contains(&key.trim())
        {
            rule[key.trim()] = json!(value.trim().trim_matches(['"', '\'']));
        }
    }
    rules
}

/// `config.yml` with the SSH rule merged into its `ingress:` block; everything else as it was.
pub fn local_config_with(yaml: &str, hostname: &str, service: &str) -> (String, Merge) {
    let (_, merge) = merge_ingress(&local_ingress(yaml), hostname, service);
    let lines: Vec<&str> = yaml.lines().collect();
    let mut out: Vec<String> = Vec::new();
    match &merge {
        Merge::Unchanged => return (yaml.to_string(), merge),
        Merge::Service(_) => {
            // the `service:` line of the rule that names the hostname
            let mut in_rule = false;
            for line in &lines {
                let trimmed = line.trim().trim_start_matches("- ").trim();
                if line.trim().starts_with("- ") {
                    in_rule = trimmed
                        .strip_prefix("hostname:")
                        .is_some_and(|h| h.trim().trim_matches(['"', '\'']) == hostname);
                } else if trimmed
                    .strip_prefix("hostname:")
                    .is_some_and(|h| h.trim().trim_matches(['"', '\'']) == hostname)
                {
                    in_rule = true;
                }
                if in_rule && trimmed.starts_with("service:") {
                    let indent = &line[..line.len() - line.trim_start().len()];
                    let dash = if line.trim().starts_with("- ") { "- " } else { "" };
                    out.push(format!("{indent}{dash}service: {service}"));
                    in_rule = false;
                } else {
                    out.push((*line).to_string());
                }
            }
        }
        Merge::Added => {
            // before the catch-all: the last `- service:` item that has no hostname
            let ingress = lines.iter().position(|l| l.trim_end() == "ingress:");
            let catch_all = ingress.and_then(|start| {
                lines
                    .iter()
                    .enumerate()
                    .skip(start + 1)
                    .filter(|(_, l)| l.trim().starts_with("- service:"))
                    .map(|(i, _)| i)
                    .next_back()
            });
            for (i, line) in lines.iter().enumerate() {
                if Some(i) == catch_all {
                    let indent = &line[..line.len() - line.trim_start().len()];
                    out.push(format!("{indent}- hostname: {hostname}"));
                    out.push(format!("{indent}  service: {service}"));
                }
                out.push((*line).to_string());
            }
            if catch_all.is_none() {
                if ingress.is_none() {
                    out.push("ingress:".to_string());
                }
                out.push(format!("  - hostname: {hostname}"));
                out.push(format!("    service: {service}"));
                out.push("  - service: http_status:404".to_string());
            }
        }
    }
    (out.join("\n") + "\n", merge)
}

// ------------------------------------------------------------------------------------------------ the API

/// Why a call to Cloudflare did not give an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The token may not do this.
    Forbidden,
    /// The token itself is not accepted.
    Unauthorized,
    NotFound,
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Forbidden => f.write_str("the API token is not allowed to do this"),
            ApiError::Unauthorized => f.write_str("the API token is not valid"),
            ApiError::NotFound => f.write_str("not found"),
            ApiError::Other(why) => f.write_str(why),
        }
    }
}

/// Cloudflare's API, with one token.
#[derive(Debug, Clone)]
pub struct Api {
    client: Client,
    token: Secret,
    base: String,
}

impl Api {
    pub fn new(host: &Arc<dyn Host>, token: Secret) -> Self {
        let base = host.env("MOI_CF_API_URL").unwrap_or_else(|| API.to_string());
        Self { client: Client::new(Arc::clone(host)).with_timeout(Duration::from_secs(30)), token, base }
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {}", self.token.expose()))];
        let raw = body.map(Value::to_string);
        if raw.is_some() {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }
        let response = self
            .client
            .send(method, &format!("{}{path}", self.base), &headers, raw.as_deref().map(str::as_bytes))
            .map_err(|e| ApiError::Other(format!("Cloudflare's API cannot be reached: {e}")))?;
        let answer: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        match response.status {
            200..=299 if answer["success"] != false => Ok(answer["result"].clone()),
            401 => Err(ApiError::Unauthorized),
            403 => Err(ApiError::Forbidden),
            404 => Err(ApiError::NotFound),
            status => {
                let said: Vec<&str> =
                    answer["errors"].as_array().into_iter().flatten().map(|e| text(&e["message"])).collect();
                // 10000 is Cloudflare's "Authentication error": a token without the permission
                if answer["errors"].as_array().into_iter().flatten().any(|e| e["code"] == 10000) {
                    return Err(ApiError::Forbidden);
                }
                Err(ApiError::Other(format!(
                    "Cloudflare answered {status}: {}",
                    if said.is_empty() { "no explanation".to_string() } else { said.join("; ") }
                )))
            }
        }
    }

    pub fn tunnel(&self, tunnel: &Tunnel) -> Result<Value, ApiError> {
        self.call("GET", &format!("/accounts/{}/cfd_tunnel/{}", tunnel.account, tunnel.id), None)
    }

    /// The whole remote configuration of a tunnel (`config.ingress` among it).
    pub fn configuration(&self, tunnel: &Tunnel) -> Result<Value, ApiError> {
        self.call("GET", &format!("/accounts/{}/cfd_tunnel/{}/configurations", tunnel.account, tunnel.id), None)
            .map(|r| r["config"].clone())
    }

    /// Replace the tunnel's configuration: a full replace, so `config` is everything that was read
    /// (keys moi does not know included) with the one change made.
    pub fn put_configuration(&self, tunnel: &Tunnel, config: &Value) -> Result<(), ApiError> {
        self.call(
            "PUT",
            &format!("/accounts/{}/cfd_tunnel/{}/configurations", tunnel.account, tunnel.id),
            Some(&json!({ "config": config })),
        )
        .map(|_| ())
    }

    /// The zone a hostname is in: its id and name. The longest suffix that is a zone of this token.
    pub fn zone(&self, hostname: &str) -> Result<Option<(String, String)>, ApiError> {
        let labels: Vec<&str> = hostname.split('.').collect();
        for start in 0..labels.len().saturating_sub(1) {
            let name = labels[start..].join(".");
            let zones = self.call("GET", &format!("/zones?name={}", crate::net::encode(&name)), None)?;
            if let Some(zone) = zones.as_array().and_then(|z| z.first()) {
                return Ok(Some((text(&zone["id"]).to_string(), text(&zone["name"]).to_string())));
            }
        }
        Ok(None)
    }

    pub fn dns_records(&self, zone: &str, hostname: &str) -> Result<Vec<Value>, ApiError> {
        self.call("GET", &format!("/zones/{zone}/dns_records?name={}", crate::net::encode(hostname)), None)
            .map(|r| r.as_array().cloned().unwrap_or_default())
    }

    /// A proxied CNAME to the tunnel. Returns the record's id.
    pub fn create_cname(&self, zone: &str, hostname: &str, target: &str) -> Result<String, ApiError> {
        // ttl 1 is Cloudflare's "automatic", the only value a proxied record takes
        let record = json!({ "type": "CNAME", "name": hostname, "content": target, "proxied": true, "ttl": 1, "comment": "moi ssh" });
        self.call("POST", &format!("/zones/{zone}/dns_records"), Some(&record)).map(|r| text(&r["id"]).to_string())
    }

    pub fn delete_record(&self, zone: &str, id: &str) -> Result<(), ApiError> {
        self.call("DELETE", &format!("/zones/{zone}/dns_records/{id}"), None).map(|_| ())
    }
}

/// What to do about the DNS record of the hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dns {
    /// A CNAME to this tunnel exists.
    Adopt,
    Create,
    /// Something else answers for the name: shown, never overwritten.
    Elsewhere(String),
}

pub fn dns_action(records: &[Value], target: &str) -> Dns {
    match records.first() {
        None => Dns::Create,
        Some(_)
            if records
                .iter()
                .any(|r| text(&r["type"]) == "CNAME" && text(&r["content"]).trim_end_matches('.') == target) =>
        {
            Dns::Adopt
        }
        Some(_) => Dns::Elsewhere(
            records
                .iter()
                .map(|r| format!("{} {}", text(&r["type"]), text(&r["content"])))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    }
}
