//! `[[checks.custom]]`: the owner's own check script. Exit 0 is up, anything else down; the first line
//! it prints is the message.

use std::time::Duration;

use super::{Check, Cx, Memory, Outcome};
use crate::config::{Config, CustomCheck};
use crate::host::Cmd;
use crate::log::Logger;

pub(crate) struct Custom {
    /// Position in `checks.custom`.
    pub index: usize,
    pub name: String,
}

impl Custom {
    fn settings<'a>(&self, config: &'a Config) -> &'a CustomCheck {
        &config.checks.custom[self.index]
    }
}

impl Check for Custom {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn timeout(&self, config: &Config) -> Duration {
        // the script's own timeout is enforced when it is run; the engine allows a little more
        self.settings(config).common.timeout.as_duration() + Duration::from_secs(5)
    }

    fn monitor(&self, config: &Config) -> Option<String> {
        self.settings(config).common.monitor.clone()
    }

    fn journal_tag(&self, config: &Config) -> String {
        self.settings(config).common.journal_tag.clone()
    }

    fn run(&self, cx: &Cx, _memory: &mut Memory) -> Outcome {
        let settings = self.settings(&cx.config);
        let mut words = settings.run.split_whitespace();
        let program = words.next().unwrap_or_default();
        let timeout = settings.common.timeout.as_duration();
        let out = match cx.host.run(&Cmd::new(program).args(words).timeout(timeout)) {
            Ok(out) => out,
            Err(e) => return Outcome::down(format!("cannot run {program}: {e}")),
        };
        let stderr = out.stderr_str();
        if !stderr.trim().is_empty() {
            Logger::new(cx.host.clone(), &settings.common.journal_tag)
                .warn(stderr.trim(), &[("check", &settings.name)]);
        }
        if out.timed_out {
            return Outcome::down(format!("timed out after {}s", timeout.as_secs()));
        }
        let first_line = out.stdout_str().lines().next().map(str::trim).unwrap_or_default().to_string();
        let message = if first_line.is_empty() {
            if out.status == 0 { "OK".to_string() } else { format!("exit status {}", out.status) }
        } else {
            first_line
        };
        if out.status == 0 { Outcome::up(message) } else { Outcome::down(message) }
    }
}
