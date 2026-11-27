//! Pushing a result to its Uptime Kuma push monitor: one status, one message.

use std::sync::Arc;
use std::time::Duration;

use super::CheckResult;
use crate::config::{Config, MonitorMode};
use crate::host::Host;
use crate::net::{self, Client};
use crate::state::db::Db;

/// Push a result. Returns `"ok"` or why nothing was pushed; never an error — a monitor that cannot be
/// reached must not stop the check from being reported locally.
pub fn push(host: &Arc<dyn Host>, config: &Config, db: &Db, result: &CheckResult) -> String {
    let kuma = &config.monitors.kuma;
    if config.monitors.mode == MonitorMode::Shadow {
        return "not pushed: monitors.mode is shadow".to_string();
    }
    let Some(monitor) = &result.monitor else { return "not pushed: the check has no monitor".to_string() };
    let base = if kuma.push_base.is_empty() { kuma.url.clone().unwrap_or_default() } else { kuma.push_base.clone() };
    if !kuma.enabled || base.is_empty() {
        return "not pushed: no [monitors.kuma] url".to_string();
    }
    let token: Option<String> = db
        .conn()
        .query_row(
            "SELECT push_token FROM monitor_bindings WHERE owner = ?1 AND push_token IS NOT NULL",
            [format!("check:{}", result.name)],
            |r| r.get(0),
        )
        .ok();
    let Some(token) = token else {
        return format!("not pushed: monitor \"{monitor}\" is not bound to this check yet (run `moi monitors sync`)");
    };
    let url = format!(
        "{}/api/push/{}?status={}&msg={}&ping=",
        base.trim_end_matches('/'),
        net::encode(&token),
        result.status.as_str(),
        net::encode(&result.message)
    );
    let client = Client::new(Arc::clone(host)).with_timeout(Duration::from_secs(15));
    let mut last = String::new();
    // one try and two retries, as `curl --retry 2` did
    for _ in 0..3 {
        match client.send("GET", &url, &[], None) {
            Ok(response) if response.is_success() => return "ok".to_string(),
            Ok(response) => last = format!("HTTP {}", response.status),
            Err(e) => last = e.to_string(),
        }
    }
    format!("push failed: {last}")
}
