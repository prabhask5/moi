//! `CONTEXT.md`: the server as observed, in one Markdown file an AI assistant can act on.
//!
//! Spec: docs/spec/03-context.md. Everything here is rendered from the last snapshot (`snapshot::View`),
//! the owner's notes, the last audit and the configuration — nothing is probed live, so generating is
//! cheap and can follow every snapshot. Secrets are masked twice: by shape where values are printed
//! (`redact`), and by value over the whole document (`Masker`).

mod docker;
mod pointer;
mod sections;

use std::collections::BTreeSet;
use std::path::Path;

pub use pointer::{remove_pointer, write_pointer};

use crate::config::Config;
use crate::error::Error;
use crate::host::{self, Host};
use crate::redact::{self, Masker};
use crate::snapshot::View;
use crate::state::{StateDir, notes};

/// Rows a table may have before it is cut.
const TABLE_CAP: usize = 200;
/// Lines a code block may have before it is cut.
const CODE_CAP: usize = 150;

/// The document being written.
pub(crate) struct Doc<'a> {
    pub view: View<'a>,
    pub config: &'a Config,
    out: String,
    warnings: BTreeSet<String>,
    notes: Vec<(String, String)>,
    used_notes: BTreeSet<String>,
}

impl Doc<'_> {
    pub fn line(&mut self, text: impl AsRef<str>) {
        self.out.push_str(text.as_ref());
        self.out.push('\n');
    }

    pub fn blank(&mut self) {
        if !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    pub fn heading(&mut self, level: usize, title: &str) {
        self.blank();
        self.line(format!("{} {title}", "#".repeat(level)));
    }

    /// Something the owner should look at: collected and shown near the top.
    pub fn warn(&mut self, text: impl Into<String>) {
        self.warnings.insert(text.into());
    }

    /// A Markdown table. Cells are escaped; more than [`TABLE_CAP`] rows are cut with a count.
    pub fn table(&mut self, headers: &[&str], rows: &[Vec<String>]) {
        if rows.is_empty() {
            return;
        }
        let escape = |cell: &str| cell.replace('|', "\\|").replace('\n', " ");
        self.line(format!("| {} |", headers.join(" | ")));
        self.line(format!("|{}", "---|".repeat(headers.len())));
        for row in rows.iter().take(TABLE_CAP) {
            self.line(format!("| {} |", row.iter().map(|c| escape(c)).collect::<Vec<_>>().join(" | ")));
        }
        if rows.len() > TABLE_CAP {
            self.line(format!("… {} more rows", rows.len() - TABLE_CAP));
        }
    }

    /// A fenced code block, cut after `cap` lines.
    pub fn code(&mut self, text: &str, cap: usize) {
        let lines: Vec<&str> = text.trim_end_matches('\n').lines().collect();
        self.line("```");
        for l in lines.iter().take(cap.min(CODE_CAP)) {
            self.line(l);
        }
        if lines.len() > cap.min(CODE_CAP) {
            self.line(format!("… ({} more lines)", lines.len() - cap.min(CODE_CAP)));
        }
        self.line("```");
    }

    /// The owner's note under a heading, as a quote block; marks the note as placed.
    pub fn note(&mut self, key: &str) -> Option<String> {
        self.used_notes.insert(key.to_string());
        let body = &self.notes.iter().find(|(heading, _)| heading == key)?.1;
        Some(body.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n"))
    }

    /// The owner's note under a heading as plain text (for table cells); marks it as placed.
    pub fn note_text(&mut self, key: &str) -> Option<String> {
        self.used_notes.insert(key.to_string());
        self.notes.iter().find(|(heading, _)| heading == key).map(|(_, body)| body.replace('\n', " "))
    }

    pub fn write_note(&mut self, key: &str) {
        if let Some(note) = self.note(key) {
            self.line(note);
        }
    }
}

/// Mask a command line or free text the way the context file shows it: by shape, plus long opaque
/// tokens (40+ characters of `[A-Za-z0-9_+=-]`, which no path or word is).
pub(crate) fn shown(text: &str) -> String {
    let masked = redact::line(text);
    let mut out = String::with_capacity(masked.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if token.chars().count() >= 40 {
            out.push_str(redact::MASK)
        } else {
            out.push_str(token)
        }
        token.clear();
    };
    for c in masked.chars() {
        if c.is_ascii_alphanumeric() || "_+=-".contains(c) {
            token.push(c);
        } else {
            flush(&mut token, &mut out);
            out.push(c);
        }
    }
    flush(&mut token, &mut out);
    out
}

/// Cut a string to at most `max` characters.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

/// Render `CONTEXT.md` from the state on disk.
pub fn generate(host: &dyn Host, config: &Config, state: &StateDir, masker: &Masker) -> String {
    let view = View::new(host, state);
    let mut doc = Doc {
        view,
        config,
        out: String::new(),
        warnings: BTreeSet::new(),
        notes: notes::read(host, state),
        used_notes: BTreeSet::new(),
    };
    let captured = view.volatile("captured-at").trim().to_string();
    let hostname = view.hostname();

    doc.line(format!("# Server context: {hostname}"));
    doc.blank();
    doc.line(format!(
        "_Generated by **moi** from the live machine, state as of **{}**. Regenerated after every snapshot (within minutes of a change, and daily) and after every audit. Do not edit this file: describe things with `sudo moi notes`._",
        if captured.is_empty() { "an unknown time".to_string() } else { captured.clone() }
    ));
    doc.heading(2, "How to use this document (for AI assistants)");
    for rule in [
        "This is the ground truth for what runs on this server as of the timestamp above. If something matters, verify it live (`docker ps`, `systemctl status`, `crontab -l`).".to_string(),
        "Before proposing a change, check it against what is here: **ports in use, Docker network names and subnets, bind-mount paths, volume names, schedules, firewall rules, disk space**.".to_string(),
        "Keep changes reproducible so they are captured and can be rebuilt: containers go in a compose file inside an existing stack directory (see Docker), scripts in a captured script directory, services as units in `/etc/systemd/system`, jobs via `crontab`, and **health checks, backups and monitors in `/etc/moi/moi.toml`** (validate with `moi config check`). Avoid one-off `docker run` and edits outside these places.".to_string(),
        "After making changes: `sudo moi snapshot` (it also happens by itself within minutes), then record the *why* with `sudo moi notes`.".to_string(),
        "Secrets are redacted (`***`, push tokens as `…last4`). The real values are in the files named here, on the server, readable by root only.".to_string(),
        format!("Raw data: `{}/` (configs, compose files, normalized Docker state). History: `sudo moi log`, `sudo moi diff`.", state.snapshot().display()),
    ] {
        doc.line(format!("- {rule}"));
    }
    let attention_at = doc.out.len();

    sections::owner_notes(&mut doc);
    sections::system(&mut doc);
    sections::storage(&mut doc);
    sections::network(&mut doc);
    sections::routes(&mut doc);
    docker::docker(&mut doc);
    docker::incus(&mut doc);
    docker::monitoring(&mut doc);
    sections::moi(&mut doc);
    sections::firewall(&mut doc);
    sections::jobs(&mut doc);
    sections::units(&mut doc);
    sections::scripts(&mut doc);
    sections::software(&mut doc);
    sections::users(&mut doc);
    sections::backups(&mut doc);
    sections::audit(&mut doc);
    sections::changes(&mut doc);
    sections::old_notes(&mut doc);

    // a snapshot that stopped being refreshed is itself worth a warning
    if let (Some(at), now) = (host::parse_rfc3339(&captured), host.now())
        && now.duration_since(at).is_ok_and(|age| age.as_secs() > 2 * 86_400)
    {
        doc.warn(format!(
            "this description is {} days old: the last snapshot was taken at {captured}",
            now.duration_since(at).map_or(0, |a| a.as_secs() / 86_400)
        ));
    }
    let Doc { mut out, warnings, .. } = doc;
    if !warnings.is_empty() {
        let mut block = String::from("\n## ⚠ Attention\n");
        for w in &warnings {
            block.push_str(&format!("- ⚠ {w}\n"));
        }
        out.insert_str(attention_at, &block);
    }
    // value-based masking over everything, whatever the sections printed
    let mut text = masker.mask(&out);
    while text.ends_with("\n\n") {
        text.pop();
    }
    text
}

/// What writing the context file did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Written {
    pub path: std::path::PathBuf,
    pub bytes: usize,
    /// Copies written (`context.copies`).
    pub copies: Vec<String>,
    /// Copies that could not be written, with the reason.
    pub failed_copies: Vec<String>,
    /// Items in the attention list.
    pub warnings: usize,
}

/// Regenerate `CONTEXT.md`, its copies and the Claude Code pointer blocks.
pub fn write(host: &dyn Host, config: &Config, state: &StateDir, masker: &Masker) -> Result<Written, Error> {
    notes::update(host, state)?;
    let text = generate(host, config, state, masker);
    let path = state.context();
    host.write(&path, text.as_bytes(), 0o600)
        .map_err(|e| Error::host(format!("cannot write {}", path.display()), e))?;
    let mut written = Written {
        path: path.clone(),
        bytes: text.len(),
        copies: Vec::new(),
        failed_copies: Vec::new(),
        warnings: text.lines().filter(|l| l.starts_with("- ⚠ ")).count(),
    };
    // (owner uid, where that owner reads the context)
    let mut readers: Vec<(u32, String)> = vec![(0, path.display().to_string())];
    for copy in &config.context.copies {
        let target = Path::new(copy);
        // a copy belongs to whoever owns the directory it is in; a missing directory is not created
        let result = target
            .parent()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no directory"))
            .and_then(|dir| host.metadata(dir))
            .and_then(|dir| {
                host.write(target, text.as_bytes(), 0o644)?;
                host.set_owner(target, dir.uid, dir.gid)?;
                Ok(dir.uid)
            });
        match result {
            Ok(uid) => {
                written.copies.push(copy.clone());
                if uid != 0 {
                    readers.push((uid, copy.clone()));
                }
            }
            Err(e) => written.failed_copies.push(format!("{copy}: {e}")),
        }
    }
    if config.context.claude_pointer {
        for (uid, context_path) in readers {
            // best effort: a home that does not exist simply gets no pointer
            let _ = write_pointer(host, uid, &context_path);
        }
    }
    Ok(written)
}
