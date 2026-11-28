//! The `ssh` check: is the tunnel that `ssh <server>` comes in by connected (docs/spec/16-ssh.md §
//! Checks). On when `[ssh]` is present with the `cloudflare` provider. It never changes anything.

use std::time::Duration;

use super::{Check, Cx, Memory, Mode, Outcome};
use crate::config::Config;
use crate::host;

/// The handshake through the public name is asked for at most this often; in between, the
/// connector's own word is the check.
const PROBE_EVERY: i64 = 15 * 60;

pub(crate) struct Ssh;

impl Check for Ssh {
    fn name(&self) -> String {
        "ssh".to_string()
    }

    fn timeout(&self, _config: &Config) -> Duration {
        Duration::from_secs(40)
    }

    fn interval(&self, config: &Config) -> Duration {
        config.checks.default_interval.as_duration()
    }

    fn monitor(&self, config: &Config) -> Option<String> {
        config.ssh.monitor.clone()
    }

    fn journal_tag(&self, _config: &Config) -> String {
        "moi-check-ssh".to_string()
    }

    fn run(&self, cx: &Cx, memory: &mut Memory) -> Outcome {
        let Some(settings) = crate::ssh::settings_of(cx.host.as_ref(), &cx.config) else {
            return Outcome::down("SSH tunnel down: [ssh] has no hostname");
        };
        let now = host::unix_seconds(cx.host.now());
        let last = memory.values.get("probed_at").and_then(|v| v.parse::<i64>().ok());
        // a run by hand always looks all the way; the daemon's runs every quarter of an hour
        let probe = cx.mode == Mode::Dry || last.is_none_or(|at| now - at >= PROBE_EVERY);
        let (up, mut message) = crate::ssh::provider(settings.provider).status(&cx.host, &settings, probe);
        if probe {
            memory.values.insert("probed_at".to_string(), now.to_string());
            // remembered, so that the runs in between keep saying what the last look found
            let failed =
                if !up && message.contains("public handshake failed") { message.clone() } else { String::new() };
            memory.values.insert("probe_failed".to_string(), failed);
        } else if up && let Some(failed) = memory.values.get("probe_failed").filter(|m| !m.is_empty()) {
            // the connector is connected, but the last look from outside did not get through
            return Outcome::down(failed.clone());
        }
        if up { Outcome::up(std::mem::take(&mut message)) } else { Outcome::down(message) }
    }
}
