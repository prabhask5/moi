//! SSH access (docs/spec/16-ssh.md § Records): with `[ssh]`, the tunnel connector, its ingress rules,
//! the DNS record, the settings of sshd that matter, and who may log in — fingerprints and comments,
//! never keys.

use std::path::Path;

use serde_json::{Value, json};

use super::{Collector, Ctx, Output, run_tool};
use crate::config::SshProvider;
use crate::ssh::cloudflare::{self, Api, Dns};

pub(crate) struct Ssh;

/// What Cloudflare said about the tunnel is asked again at most this often.
const API_EVERY: i64 = 86_400;
const CACHE: &str = "ssh.cloudflare_cache";

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

impl Collector for Ssh {
    fn name(&self) -> &'static str {
        "ssh"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let mut out = Output::default();
        let host = cx.host.as_ref();
        let Some(settings) = crate::ssh::settings_of(host, &cx.config) else { return Ok(out) };
        let mut access = json!({ "hostname": settings.hostname, "alias": settings.alias, "user": settings.user, "provider": cx.config.ssh.provider });

        // sshd's own settings, lower-cased as `sshd -T` prints them
        if let Some(effective) = run_tool(host, "sshd", &["-T"]) {
            let wanted = [
                "port",
                "passwordauthentication",
                "permitrootlogin",
                "pubkeyauthentication",
                "kbdinteractiveauthentication",
            ];
            let settings: serde_json::Map<String, Value> = effective
                .lines()
                .filter_map(|l| l.split_once(' '))
                .filter(|(key, _)| wanted.contains(&key.to_lowercase().as_str()))
                .map(|(key, value)| (key.to_lowercase(), json!(value.trim().to_lowercase())))
                .collect();
            access["sshd"] = Value::Object(settings);
        }
        let keys: Vec<Value> = crate::ssh::authorized_keys(host, &settings.user)
            .into_iter()
            .map(|(fingerprint, comment)| json!({ "fingerprint": fingerprint, "comment": comment }))
            .collect();
        access["authorized_keys"] = json!(keys);

        if settings.provider == SshProvider::Cloudflare {
            let version = run_tool(host, "cloudflared", &["--version"]).map(|v| v.trim().to_string());
            access["connector"] = json!({
                "unit": host.exists(Path::new(cloudflare::UNIT)).then_some(cloudflare::UNIT),
                "drop_in": host.exists(Path::new(cloudflare::DROP_IN)).then_some(cloudflare::DROP_IN),
                "version": version,
                "metrics": settings.metrics,
            });
            let active =
                run_tool(host, "systemctl", &["is-active", "cloudflared.service"]).map(|s| s.trim().to_string());
            out.volatile(
                "ssh-connector.txt",
                format!("cloudflared.service {}\n", active.unwrap_or_else(|| "unknown".to_string())),
            );
            if let Ok(tunnel) = cloudflare::discover(host, &cx.docker, settings.tunnel_token.as_ref()) {
                access["tunnel"] =
                    json!({ "id": tunnel.id, "account": tunnel.account, "found_in": tunnel.source.describe() });
                match tunnel.local_config() {
                    Some(path) => {
                        access["ingress"] =
                            json!(cloudflare::local_ingress(&host.read_to_string(Path::new(path)).unwrap_or_default()))
                    }
                    None => {
                        // Cloudflare is asked once a day; in between, what it said last
                        let state = crate::state::StateDir::new(cx.config.state.dir.clone());
                        let db = state.open_db(host).ok();
                        let now = crate::host::unix_seconds(host.now());
                        let cached: Option<Value> =
                            db.as_ref().and_then(|db| db.setting(CACHE)).and_then(|v| serde_json::from_str(&v).ok());
                        let fresh = cached.as_ref().is_some_and(|c| {
                            c["hostname"] == settings.hostname.as_str()
                                && now - c["at"].as_i64().unwrap_or(0) < API_EVERY
                        });
                        let answer = if fresh {
                            cached
                        } else if let Some(token) = settings.api_token.clone() {
                            let api = Api::new(&cx.host, token);
                            let ingress = api
                                .configuration(&tunnel)
                                .ok()
                                .map(|c| c["ingress"].as_array().cloned().unwrap_or_default());
                            let dns = api
                                .zone(&settings.hostname)
                                .ok()
                                .flatten()
                                .and_then(|zone| api.dns_records(&zone.0, &settings.hostname).ok())
                                .map(|records| match cloudflare::dns_action(&records, &tunnel.target()) {
                                    Dns::Adopt => format!("CNAME {} → {}", settings.hostname, tunnel.target()),
                                    Dns::Create => "no record".to_string(),
                                    Dns::Elsewhere(what) => format!("points elsewhere: {what}"),
                                });
                            // an answer that did not come keeps the last one
                            match (ingress, dns) {
                                (Some(ingress), Some(dns)) => {
                                    let answer = json!({ "at": now, "hostname": settings.hostname, "ingress": ingress, "dns": dns });
                                    if let Some(db) = &db {
                                        db.set_setting(CACHE, &answer.to_string());
                                    }
                                    Some(answer)
                                }
                                _ => cached,
                            }
                        } else {
                            cached
                        };
                        if let Some(answer) = answer {
                            // hostname and service of each rule: what a reader needs, in the tunnel's order
                            let rules: Vec<Value> = answer["ingress"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|r| json!({ "hostname": r["hostname"], "service": r["service"] }))
                                .collect();
                            access["ingress"] = json!(rules);
                            access["dns"] = answer["dns"].clone();
                        }
                    }
                }
            }
        }
        out.stable("ssh/access.json", pretty(&access));
        Ok(out)
    }
}
