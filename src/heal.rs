//! Heal rules (`[[heal]]`, docs/spec/10-daemon.md § Heal rules): what a rule does, its cooldown and
//! the report it adds to the services message. The services check runs the rules it triggers itself
//! (a container off its network, named results failing); the daemon runs the event-triggered ones.

use std::sync::Arc;
use std::time::Duration;

use crate::config::{Action, HealRule};
use crate::docker::Docker;
use crate::host::{self, Cmd, Host};
use crate::log::Logger;
use crate::state::StateDir;

/// How often a container's health is looked at while waiting for it.
const HEALTH_POLL: Duration = Duration::from_secs(5);
/// Longest a `run` action may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// What a rule needs to act.
#[derive(Debug, Clone)]
pub struct Healer {
    pub host: Arc<dyn Host>,
    pub docker: Docker,
    pub state: StateDir,
    /// `false` for a manual `moi check`: say what would be done, do nothing.
    pub act: bool,
}

impl Healer {
    /// A rule's trigger fired. Returns what to add to the services message, if anything.
    pub fn fire(&self, rule: &HealRule, why: &str) -> Option<String> {
        let name = &rule.name;
        if !self.act {
            return Some(format!("[{name}: not healed by a manual check]"));
        }
        let now = host::unix_seconds(self.host.now());
        // The run is noted before acting: a heal that takes the check past its timeout must still count.
        match self.note_run(name, now, rule.cooldown.map(|c| c.as_secs())) {
            Ok(true) => {}
            Ok(false) => {
                let cooldown = rule.cooldown.map(|c| c.to_string()).unwrap_or_default();
                return Some(
                    rule.report_cooldown
                        .clone()
                        .unwrap_or_else(|| format!("[{name}: already tried in the last {cooldown}, needs a look]")),
                );
            }
            Err(e) => return Some(self.failed(rule, &format!("cannot record the run: {e}"))),
        }
        let log = Logger::new(Arc::clone(&self.host), "moi");
        log.info(&format!("heal {name}: {why}"), &[("rule", name)]);
        match self.run(&rule.action) {
            Ok(stdout) if rule.report_stdout => {
                let said = stdout.trim();
                (!said.is_empty()).then(|| format!("[{said}; next check confirms]"))
            }
            Ok(_) => Some(rule.report.clone().unwrap_or_else(|| format!("[{name}: healed; next check confirms]"))),
            Err(e) => {
                log.warn(&format!("heal {name} failed: {e}"), &[("rule", name)]);
                Some(self.failed(rule, &e))
            }
        }
    }

    fn failed(&self, rule: &HealRule, error: &str) -> String {
        rule.report_failed.clone().unwrap_or_else(|| format!("[{}: heal failed: {error}]", rule.name))
    }

    /// Record that the rule runs now — unless it ran within its cooldown, in which case nothing is
    /// recorded and `false` comes back.
    fn note_run(&self, rule: &str, now: i64, cooldown: Option<u64>) -> Result<bool, String> {
        let db = self.state.open_db(self.host.as_ref()).map_err(|e| e.to_string())?;
        let last: Option<i64> =
            db.conn().query_row("SELECT last_run FROM heal_cooldowns WHERE rule = ?1", [rule], |r| r.get(0)).ok();
        if let (Some(last), Some(cooldown)) = (last, cooldown)
            && now.saturating_sub(last) < i64::try_from(cooldown).unwrap_or(i64::MAX)
        {
            return Ok(false);
        }
        db.conn()
            .execute("INSERT INTO heal_cooldowns (rule, last_run) VALUES (?1, ?2) ON CONFLICT(rule) DO UPDATE SET last_run = excluded.last_run", (rule, now))
            .map_err(|e| e.to_string())?;
        Ok(true)
    }

    /// Do what the rule says. Returns the stdout of a `run` action.
    fn run(&self, action: &Action) -> Result<String, String> {
        match action {
            Action::NetworkConnect { network_connect: target } => {
                self.docker.network_connect(&target.network, &target.container).map_err(|e| e.to_string())?;
                Ok(String::new())
            }
            Action::Restart { restart, wait_healthy, then_restart } => {
                for container in restart {
                    self.docker.restart(container).map_err(|e| e.to_string())?;
                }
                if let Some(wait) = wait_healthy {
                    // as the legacy script did, the dependants are restarted even if it never gets healthy
                    let mut waited = Duration::ZERO;
                    while !self.healthy(&wait.container) && waited < wait.timeout.as_duration() {
                        self.host.sleep(HEALTH_POLL);
                        waited += HEALTH_POLL;
                    }
                }
                for container in then_restart {
                    self.docker.restart(container).map_err(|e| e.to_string())?;
                }
                Ok(String::new())
            }
            Action::Run { run } => {
                let mut words = run.split_whitespace();
                let program = words.next().unwrap_or_default();
                let out = self
                    .host
                    .run(&Cmd::new(program).args(words).timeout(RUN_TIMEOUT))
                    .map_err(|e| format!("cannot run {program}: {e}"))?;
                if out.timed_out {
                    return Err(format!("{program} timed out after {}s", RUN_TIMEOUT.as_secs()));
                }
                if out.status != 0 {
                    return Err(format!("{program} exited with status {}", out.status));
                }
                Ok(out.stdout_str().into_owned())
            }
            Action::ComposeUp { .. } => Err("compose_up actions are run by the daemon".to_string()),
        }
    }

    /// Healthy by its own health check; a container without one is healthy when it runs.
    fn healthy(&self, container: &str) -> bool {
        let Ok(c) = self.docker.container(container) else { return false };
        match c["State"]["Health"]["Status"].as_str() {
            Some(status) => status == "healthy",
            None => c["State"]["Running"].as_bool().unwrap_or(false),
        }
    }
}
