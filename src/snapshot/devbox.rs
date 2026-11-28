//! devbox (docs/spec/15-devbox.md § Snapshot): its host configuration, the routing file it names,
//! the gateway container, and what `devbox status` and `doctor` say — the parts that stay the same
//! in `snapshot/`, the parts that change by the hour in `volatile/`. The secrets are never read.

use std::path::Path;

use serde_json::{Value, json};

use super::{Collector, Ctx, Output};

pub(crate) struct Devbox;

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

impl Collector for Devbox {
    fn name(&self) -> &'static str {
        "devbox"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let mut out = Output::default();
        let host = cx.host.as_ref();
        if !crate::devbox::enabled(host, &cx.config) {
            return Ok(out);
        }
        let Ok(config_text) = host.read_to_string(Path::new(crate::devbox::CONFIG)) else { return Ok(out) };
        out.stable("devbox/devbox.toml", config_text.clone());
        let config: toml::Table = config_text.parse().unwrap_or_default();
        let text = |table: &str, key: &str| {
            config.get(table).and_then(|t| t.get(key)).and_then(toml::Value::as_str).map(str::to_string)
        };
        if let Some(routing) = text("routing", "file")
            && let Ok(content) = host.read_to_string(Path::new(&routing))
        {
            out.stable("devbox/routing.yml", content);
        }
        // the secrets: that they are there and who may read them, nothing of what they say
        let secrets = Path::new(crate::devbox::CONFIG).with_file_name("secrets.toml");
        let secrets = host
            .metadata(&secrets)
            .ok()
            .map(|m| json!({ "mode": format!("{:04o}", m.mode), "uid": m.uid, "gid": m.gid }));
        // the gateway, as Docker describes it
        let gateway = cx.docker.containers(true).unwrap_or_default().into_iter().find(|c| {
            c["Names"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|n| n.trim_start_matches('/').starts_with("devbox-gateway"))
        });
        let gateway = gateway.map(|c| {
            json!({
                "container": c["Names"][0].as_str().unwrap_or_default().trim_start_matches('/'),
                "image": c["Image"],
                "compose_project": c["Labels"]["com.docker.compose.project"],
                "working_dir": c["Labels"]["com.docker.compose.project.working_dir"],
            })
        });

        // what devbox itself says: asked once, split into what stays and what changes
        let status = crate::devbox::status_value(host);
        let field = |path: &[&str]| -> Value {
            let mut at = status.as_ref().unwrap_or(&Value::Null);
            for key in path {
                at = &at[*key];
            }
            at.clone()
        };
        let number =
            |table: &str, key: &str| config.get(table).and_then(|t| t.get(key)).and_then(toml::Value::as_integer);
        let stable = json!({
            "instance": config.get("instance").and_then(toml::Value::as_str),
            "domain": config.get("domain").and_then(toml::Value::as_str),
            "hosts": { "code": text("hosts", "code"), "agents": text("hosts", "agents") },
            "vm": { "address": text("vm", "address"), "user": field(&["vm", "user"]), "code_port": number("vm", "code_port"), "hub_port": number("vm", "hub_port"), "ssh_port": number("vm", "ssh_port") },
            "sleep": { "idle": text("sleep", "idle"), "mode": text("sleep", "mode"), "min_up": text("sleep", "min_up") },
            "routing": { "provider": text("routing", "provider"), "file": text("routing", "file") },
            "firewall": text("firewall", "manager"),
            "gateway": gateway,
            "gateway_ssh": text("gateway", "ssh_listen"),
            "gateway_version": field(&["gateway", "version"]),
            "dotfiles": field(&["dotfiles"]),
            "secrets": secrets,
        });
        out.stable("devbox/status.json", pretty(&stable));

        let system = crate::devbox::DevboxHost(host);
        let paths = devbox_core::setup::Paths::from_env(&system);
        let doctor = devbox_core::setup::doctor(&system, &devbox_core::setup::StatusOptions { paths });
        let failing: Vec<Value> = doctor.0["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["result"] == "fail")
            .map(|c| json!({ "id": c["id"], "title": c["title"], "detail": c["detail"], "fix": c["fix"] }))
            .collect();
        let volatile = json!({
            "asked": status.is_some(),
            "vm_state": field(&["vm", "state"]),
            "memory_saved": field(&["vm", "memorySaved"]),
            "phase": field(&["gateway", "phase"]),
            "health": field(&["gateway", "health"]),
            "problems": field(&["gateway", "problems"]),
            "gateway_container": field(&["gateway", "container"]),
            "last_stop": field(&["gateway", "lastStop"]),
            "last_wake": field(&["gateway", "lastWake"]),
            "doctor_failures": failing,
        });
        out.volatile("devbox/state.json", pretty(&volatile));
        Ok(out)
    }
}
