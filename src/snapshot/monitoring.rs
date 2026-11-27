//! What reports to whom: Uptime Kuma's own monitor list (read-only from its SQLite database) and every
//! captured file or container environment that pushes to Uptime Kuma or pings healthchecks.io.
//!
//! Runs after the other collectors because it scans what they captured. Push tokens are kept as found:
//! adopting an existing monitor (06) needs them, and the state dir is root-only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::collect::is_text;
use super::{Collector, Ctx, Output};
use crate::host::{FileKind, Host};

pub(crate) struct Monitoring;

/// Files larger than this are not scripts or configuration worth scanning.
const SCAN_LIMIT: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Endpoint {
    pub kind: &'static str,
    pub base: String,
    pub token: String,
}

/// Uptime Kuma push URLs (`…/api/push/<token>`) and healthchecks.io ping URLs in a text.
pub(crate) fn endpoints(text: &str) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("http") {
        let candidate = &rest[i..];
        rest = &rest[i + 4..];
        let Some(after_scheme) = candidate.strip_prefix("https://").or_else(|| candidate.strip_prefix("http://"))
        else {
            continue;
        };
        let host_len =
            after_scheme.find(|c: char| c.is_whitespace() || "\"'/".contains(c)).unwrap_or(after_scheme.len());
        let host = &after_scheme[..host_len];
        let base = &candidate[..candidate.len() - after_scheme.len() + host_len];
        let path = &after_scheme[host_len..];
        if host.is_empty() {
            continue;
        }
        if let Some(token) = path.strip_prefix("/api/push/") {
            let token: String = token.chars().take_while(char::is_ascii_alphanumeric).collect();
            if token.len() >= 6 {
                out.push(Endpoint { kind: "uptime-kuma-push", base: base.to_string(), token });
            }
        } else if host == "hc-ping.com" || host.contains("healthchecks") {
            let Some(path) = path.strip_prefix('/') else { continue };
            let end = path.find(|c: char| c.is_whitespace() || "\"'?".contains(c)).unwrap_or(path.len());
            let token = &path[..end];
            let uuid = token.len() >= 20 && token.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            let slug = token.split('/').count() == 2 && token.split('/').all(|p| !p.is_empty());
            if uuid || slug {
                out.push(Endpoint { kind: "healthchecks", base: base.to_string(), token: token.to_string() });
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A copy someone left behind: it may still hold a live token, and nothing runs it.
fn stale_copy(path: &str) -> bool {
    [".bak", ".old", ".orig", "~"].iter().any(|s| path.ends_with(s))
}

fn integrations(cx: &Ctx) -> Vec<Value> {
    let host = cx.host.as_ref();
    let read_text = |p: &Path| -> Option<String> {
        let md = host.symlink_metadata(p).ok()?;
        if md.kind != FileKind::File || md.size > SCAN_LIMIT {
            return None;
        }
        let data = host.read(p).ok()?;
        is_text(&data).then(|| String::from_utf8_lossy(&data).into_owned())
    };
    // what schedules or starts things: where "called by" comes from
    let callers: Vec<(String, String)> = cx
        .captured
        .iter()
        .filter(|p| {
            let s = p.to_string_lossy();
            s == "/etc/crontab"
                || s.starts_with("/etc/cron.d/")
                || s.starts_with("/var/spool/cron/")
                || (s.starts_with("/etc/systemd/system/") && s.ends_with(".service"))
        })
        .filter_map(|p| Some((p.display().to_string(), read_text(p)?)))
        .collect();

    let mut found = Vec::new();
    let files: BTreeSet<&PathBuf> = cx.captured.iter().collect();
    for file in files {
        let Some(text) = read_text(file) else { continue };
        if !(text.contains("/api/push/") || text.contains("hc-ping") || text.contains("healthchecks")) {
            continue;
        }
        let path = file.display().to_string();
        let mut called_by = Vec::new();
        for (source, content) in &callers {
            for line in content.lines().filter(|l| l.contains(&path) && !l.trim_start().starts_with('#')) {
                called_by.push(format!("{source}: {}", line.trim().chars().take(160).collect::<String>()));
            }
        }
        for e in endpoints(&text) {
            let mut item =
                json!({ "file": path, "kind": e.kind, "base": e.base, "token": e.token, "called_by": called_by });
            if stale_copy(&path) {
                item["stale_copy"] = json!(true);
            }
            found.push(item);
        }
    }
    // containers that push by themselves (a DDNS updater with its own push URL, …)
    let containers: Value =
        cx.generated.get("docker/containers.json").and_then(|b| serde_json::from_slice(b).ok()).unwrap_or(Value::Null);
    for c in containers.as_array().into_iter().flatten() {
        for env in c["env"].as_array().into_iter().flatten().filter_map(|e| e.as_str()) {
            let (key, value) = env.split_once('=').unwrap_or((env, ""));
            for e in endpoints(value) {
                found.push(
                    json!({ "container": c["name"], "env": key, "kind": e.kind, "base": e.base, "token": e.token }),
                );
            }
        }
    }
    found
}

fn sql_value(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
    }
}

/// (stable monitor list, volatile last heartbeat per monitor name).
fn kuma_inventory(host: &dyn Host, db_host_path: &str) -> Option<(Value, Value)> {
    let real = host.real_path(Path::new(db_host_path)).ok()?;
    let conn = crate::monitors::inventory::open_read_only(&real)?;
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(monitor)")
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(1)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    let wanted: Vec<&str> = [
        "id",
        "name",
        "type",
        "active",
        "interval",
        "url",
        "hostname",
        "port",
        "keyword",
        "push_token",
        "docker_container",
        "docker_host",
        "parent",
        "description",
        "maxretries",
        "retry_interval",
        "upside_down",
        "method",
    ]
    .into_iter()
    .filter(|c| columns.iter().any(|have| have == c))
    .collect();
    let mut monitors: Vec<Map<String, Value>> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&format!("SELECT {} FROM monitor ORDER BY id", wanted.join(", "))) {
        let rows = stmt.query_map([], |r| {
            let mut m = Map::new();
            for (i, column) in wanted.iter().enumerate() {
                let value = sql_value(r.get_ref(i)?);
                let empty = value.is_null() || value == json!("") || value == json!(0);
                // `active = 0` (paused) is information; other zeros and blanks are not
                if !empty || *column == "active" {
                    m.insert((*column).to_string(), value);
                }
            }
            Ok(m)
        });
        monitors.extend(rows.into_iter().flatten().flatten());
    }
    let pairs = |sql: &str| -> Vec<(i64, String)> {
        conn.prepare(sql)
            .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()))
            .unwrap_or_default()
    };
    let notifications = pairs(
        "SELECT mn.monitor_id, n.name FROM monitor_notification mn JOIN notification n ON n.id = mn.notification_id ORDER BY n.name",
    );
    let tags = pairs(
        "SELECT mt.monitor_id, t.name || CASE WHEN coalesce(mt.value, '') = '' THEN '' ELSE ':' || mt.value END
         FROM monitor_tag mt JOIN tag t ON t.id = mt.tag_id ORDER BY 2",
    );
    let names: Vec<(i64, String)> =
        monitors.iter().filter_map(|m| Some((m.get("id")?.as_i64()?, m.get("name")?.as_str()?.to_string()))).collect();
    let mut last = Map::new();
    for m in &mut monitors {
        let id = m.get("id").and_then(Value::as_i64).unwrap_or_default();
        let of = |list: &[(i64, String)]| -> Vec<String> {
            list.iter().filter(|(mid, _)| *mid == id).map(|(_, v)| v.clone()).collect()
        };
        if !of(&notifications).is_empty() {
            m.insert("notifications".into(), json!(of(&notifications)));
        }
        if !of(&tags).is_empty() {
            m.insert("tags".into(), json!(of(&tags)));
        }
        if let Some(parent) = m.remove("parent").and_then(|p| p.as_i64())
            && let Some((_, group)) = names.iter().find(|(pid, _)| *pid == parent)
        {
            m.insert("group".into(), json!(group));
        }
        let heartbeat = conn.query_row(
            "SELECT status, msg, time FROM heartbeat WHERE monitor_id = ?1 ORDER BY time DESC LIMIT 1",
            [id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)),
        );
        if let (Ok((status, msg, time)), Some(name)) = (heartbeat, m.get("name").and_then(Value::as_str)) {
            let status = match status {
                0 => "down",
                1 => "up",
                2 => "pending",
                3 => "maintenance",
                _ => "unknown",
            };
            let msg: String = msg.unwrap_or_default().chars().take(300).collect();
            last.insert(name.to_string(), json!({ "status": status, "msg": msg, "time": time }));
        }
    }
    Some((json!({ "db": db_host_path, "monitors": monitors }), Value::Object(last)))
}

impl Collector for Monitoring {
    fn name(&self) -> &'static str {
        "monitoring"
    }

    fn after_capture(&self) -> bool {
        true
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        // Without this run's container list the result would silently lose the Kuma inventory and the
        // container integrations: keep the previous capture instead.
        if cx.failed.iter().any(|c| c == "docker") {
            return Err("docker state unavailable in this run".to_string());
        }
        let mut out = Output::default();
        let pretty = |v: &Value| format!("{}\n", serde_json::to_string_pretty(v).unwrap_or_default());
        let found = integrations(cx);
        if !found.is_empty() {
            out.stable("monitoring/integrations.json", pretty(&json!(found)));
        }

        let containers: Value = cx
            .generated
            .get("docker/containers.json")
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null);
        let kuma_db = containers
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["image"].as_str().is_some_and(|i| i.contains("uptime-kuma")))
            .flat_map(|c| c["mounts"].as_array().into_iter().flatten())
            .filter(|m| m["destination"] == "/app/data")
            .filter_map(|m| m["source"].as_str())
            .map(|source| format!("{}/kuma.db", source.trim_end_matches('/')))
            .find(|db| cx.host.exists(Path::new(db)));
        if let Some(db) = kuma_db {
            match kuma_inventory(cx.host.as_ref(), &db) {
                Some((monitors, last)) => {
                    out.stable("monitoring/kuma-monitors.json", pretty(&monitors));
                    out.volatile("kuma-last.json", pretty(&last));
                }
                None => return Err(format!("cannot read {db} (read-only)")),
            }
        }
        Ok(out)
    }
}
