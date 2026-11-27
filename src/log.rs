//! Logging: structured entries to the systemd journal when running under systemd, plain lines on stderr
//! otherwise. Every message and field value is masked for known secrets before it leaves the process.
//!
//! Spec: docs/spec/10-daemon.md § Logging, docs/spec/14-security.md.

use std::sync::Arc;

use crate::host::Host;
use crate::redact::Masker;

/// syslog priorities (the journal's `PRIORITY` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 3,
    Warning = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
}

/// Where entries go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Native journal protocol with structured fields.
    Journal,
    /// Human-readable lines on stderr.
    Stderr,
}

#[derive(Debug, Clone)]
pub struct Logger {
    host: Arc<dyn Host>,
    identifier: String,
    target: Target,
    min_level: Level,
    masker: Masker,
}

impl Logger {
    /// A logger tagged `identifier` (`SYSLOG_IDENTIFIER`). It writes to the journal when systemd started
    /// the process (`JOURNAL_STREAM` is set, as for `moi.service`), to stderr otherwise.
    pub fn new(host: Arc<dyn Host>, identifier: &str) -> Self {
        let target = if host.env("JOURNAL_STREAM").is_some() { Target::Journal } else { Target::Stderr };
        Self { host, identifier: identifier.to_string(), target, min_level: Level::Info, masker: Masker::default() }
    }

    #[must_use]
    pub fn with_target(mut self, target: Target) -> Self {
        self.target = target;
        self
    }

    /// Drop entries less severe than this (`--quiet` uses [`Level::Warning`]).
    #[must_use]
    pub fn with_min_level(mut self, level: Level) -> Self {
        self.min_level = level;
        self
    }

    /// Mask these secret values in everything logged.
    #[must_use]
    pub fn with_masker(mut self, masker: Masker) -> Self {
        self.masker = masker;
        self
    }

    pub fn target(&self) -> Target {
        self.target
    }

    /// Log a message with extra fields. In the journal a field `late-by` becomes `MOI_LATE_BY`.
    pub fn log(&self, level: Level, message: &str, fields: &[(&str, &str)]) {
        if level > self.min_level {
            return;
        }
        let message = self.masker.mask(message);
        let fields: Vec<(&str, String)> = fields.iter().map(|(k, v)| (*k, self.masker.mask(v))).collect();
        if self.target == Target::Journal {
            let mut entry = vec![
                ("MESSAGE".to_string(), message.clone()),
                ("PRIORITY".to_string(), (level as u8).to_string()),
                ("SYSLOG_IDENTIFIER".to_string(), self.identifier.clone()),
            ];
            entry.extend(fields.iter().map(|(k, v)| (journal_field_name(k), v.clone())));
            if self.host.journal(&entry).is_ok() {
                return;
            }
            // No journal after all (socket missing): the entry must not be lost.
        }
        let borrowed: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        eprintln!("{}", format_line(level, &message, &borrowed));
    }

    pub fn error(&self, message: &str, fields: &[(&str, &str)]) {
        self.log(Level::Error, message, fields);
    }

    pub fn warn(&self, message: &str, fields: &[(&str, &str)]) {
        self.log(Level::Warning, message, fields);
    }

    pub fn info(&self, message: &str, fields: &[(&str, &str)]) {
        self.log(Level::Info, message, fields);
    }

    pub fn debug(&self, message: &str, fields: &[(&str, &str)]) {
        self.log(Level::Debug, message, fields);
    }
}

/// A journal field name: upper-case letters, digits and `_`, prefixed `MOI_`.
fn journal_field_name(name: &str) -> String {
    let upper: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    if upper.starts_with("MOI_") { upper } else { format!("MOI_{upper}") }
}

/// The stderr form of an entry: `moi: warning: message (key=value, …)`.
pub fn format_line(level: Level, message: &str, fields: &[(&str, &str)]) -> String {
    let mut line = String::from(match level {
        Level::Error => "moi: error: ",
        Level::Warning => "moi: warning: ",
        Level::Notice | Level::Info | Level::Debug => "moi: ",
    });
    line.push_str(message);
    if !fields.is_empty() {
        let shown: Vec<String> = fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
        line.push_str(&format!(" ({})", shown.join(", ")));
    }
    line
}

/// One datagram of the journal's native protocol: `KEY=value\n`, or for values containing a newline
/// `KEY\n` + 64-bit little-endian length + value + `\n`.
pub fn encode_journal(fields: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in fields {
        out.extend_from_slice(key.as_bytes());
        if value.contains('\n') {
            out.push(b'\n');
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        } else {
            out.push(b'=');
        }
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out
}
