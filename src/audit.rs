//! Audit: what would be lost or forgotten — on a reboot, in a rebuild, by the backup.
//!
//! Spec: docs/spec/04-audit.md. Read-only. Works from the last snapshot plus a few live facts a
//! snapshot does not hold (directory sizes, bytes written inside containers, image digests, package
//! ownership). Findings are data ([`Audit`]); `audit.md`, the context file's attention list and
//! `--json` are renderings of it.

mod config;
mod coverage;
mod misc;
mod placement;
mod reboot;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::backup::Identity;
use crate::config::Config;
use crate::docker::Docker;
use crate::error::Error;
use crate::host::{self, Cmd, Host};
use crate::snapshot::View;
use crate::state::StateDir;

/// What a finding means for the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Something would be lost or not come back.
    Gap,
    /// Probably fine; the owner should decide.
    Review,
    /// Checked and fine — shown so the owner sees why.
    Ok,
    /// Declared fungible: never backed up, never a gap.
    Fungible,
    /// A fact worth knowing.
    Info,
}

impl Level {
    fn mark(self) -> &'static str {
        match self {
            Level::Gap => "✗ ",
            Level::Review => "review ",
            Level::Ok => "✓ ",
            Level::Fungible => "fungible ",
            Level::Info => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Finding { level: Level, text: String },
    Text { text: String },
    Code { text: String },
    Table { headers: Vec<String>, rows: Vec<Vec<String>> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Section {
    /// Stable identifier: `reboot`, `large`, `docker`, `config`, `state`, `placement`, `secrets`,
    /// `listeners`, `monitoring`.
    pub id: &'static str,
    pub title: &'static str,
    pub blocks: Vec<Block>,
}

impl Section {
    fn new(id: &'static str, title: &'static str) -> Self {
        Self { id, title, blocks: Vec::new() }
    }

    fn finding(&mut self, level: Level, text: impl Into<String>) {
        self.blocks.push(Block::Finding { level, text: text.into() });
    }

    fn gap(&mut self, text: impl Into<String>) {
        self.finding(Level::Gap, text);
    }

    fn review(&mut self, text: impl Into<String>) {
        self.finding(Level::Review, text);
    }

    fn ok(&mut self, text: impl Into<String>) {
        self.finding(Level::Ok, text);
    }

    fn info(&mut self, text: impl Into<String>) {
        self.finding(Level::Info, text);
    }

    fn text(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::Text { text: text.into() });
    }

    /// A code block, cut after `cap` lines.
    fn code(&mut self, lines: &[String], cap: usize) {
        let mut text = lines.iter().take(cap).cloned().collect::<Vec<_>>().join("\n");
        if lines.len() > cap {
            text.push_str(&format!("\n… {} more", lines.len() - cap));
        }
        self.blocks.push(Block::Code { text });
    }

    fn table(&mut self, headers: &[&str], rows: Vec<Vec<String>>) {
        if !rows.is_empty() {
            self.blocks.push(Block::Table { headers: headers.iter().map(ToString::to_string).collect(), rows });
        }
    }

    /// A line for sections that found nothing.
    fn or_nothing(mut self, text: &str) -> Self {
        if self.blocks.is_empty() {
            self.ok(text);
        }
        self
    }
}

/// The result of an audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Audit {
    pub hostname: String,
    /// RFC 3339.
    pub generated: String,
    pub sections: Vec<Section>,
}

impl Audit {
    /// Every finding of a level, with the section it belongs to.
    pub fn findings(&self, level: Level) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        for section in &self.sections {
            for block in &section.blocks {
                if let Block::Finding { level: l, text } = block
                    && *l == level
                {
                    out.push((section.id, text.as_str()));
                }
            }
        }
        out
    }

    /// Number of gaps (✗).
    pub fn gaps(&self) -> usize {
        self.findings(Level::Gap).len()
    }

    /// `audit.md`. `heading` is the level of the title (1 standalone, 2 inside the context file).
    pub fn markdown(&self, heading: usize) -> String {
        let h = "#".repeat(heading);
        let mut out = format!(
            "{h} Coverage & reboot audit — {}\n_Generated {}. Read-only findings: ✗ = gap, review = decide, ✓ = checked and fine, fungible = declared not worth backing up. \
             Close gaps in `/etc/moi/moi.toml` (`[backup]`, `[data] fungible`, `[restore]`) or explain them with `moi notes`._\n",
            self.hostname,
            crate::state::history::display_time(&self.generated) + " UTC"
        );
        for section in &self.sections {
            out.push_str(&format!("\n{h}# {}\n", section.title));
            for block in &section.blocks {
                match block {
                    Block::Finding { level, text } => out.push_str(&format!("- {}{text}\n", level.mark())),
                    Block::Text { text } => out.push_str(&format!("{text}\n")),
                    Block::Code { text } => out.push_str(&format!("```\n{text}\n```\n")),
                    Block::Table { headers, rows } => {
                        out.push_str(&format!("| {} |\n|{}\n", headers.join(" | "), "---|".repeat(headers.len())));
                        for row in rows {
                            out.push_str(&format!("| {} |\n", row.join(" | ")));
                        }
                    }
                }
            }
        }
        out
    }
}

/// Live facts an audit needs beyond the snapshot.
#[derive(Debug, Default)]
pub(crate) struct Live {
    /// Directory → bytes, from `du` at idle IO priority.
    pub sizes: BTreeMap<String, u64>,
    /// Why sizes are missing, if they are.
    pub sizing_note: Option<String>,
    /// Container → bytes written inside it (not in a volume).
    pub size_rw: BTreeMap<String, u64>,
    /// Image tags that have no registry digest (built on this machine).
    pub local_images: Vec<String>,
    /// Every path a package installed.
    pub package_owned: BTreeSet<String>,
    /// Package conffile → md5 the package shipped.
    pub conffiles: Vec<(String, String)>,
    /// When the OS was installed, as far as files can tell.
    pub installed: Option<SystemTime>,
}

/// Human-readable size: `1.5 GB`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Roots whose directories are sized.
fn sizing_roots(view: &View<'_>) -> Vec<String> {
    let mut roots: Vec<String> =
        ["/opt", "/srv", "/var/lib", "/root", "/home"].iter().map(ToString::to_string).collect();
    roots.extend(view.lines("storage/data-roots.txt"));
    let mut seen = BTreeSet::new();
    roots.retain(|r| {
        seen.insert(r.clone()) && view.host().metadata(Path::new(r)).is_ok_and(|m| m.kind == host::FileKind::Dir)
    });
    roots
}

fn gather(host: &Arc<dyn Host>, view: &View<'_>) -> Live {
    let mut live = Live::default();
    // Sizes: only ever at the lowest CPU and IO priority. Without `nice`/`ionice` there is no sizing.
    for root in sizing_roots(view) {
        let cmd = Cmd::new("nice")
            .args(["-n", "19", "ionice", "-c3", "du", "-x", "-B1", "-d", "2", root.as_str()])
            .timeout(Duration::from_secs(900));
        match host.run(&cmd) {
            // du exits non-zero when it could not read something; what it printed is still right
            Ok(out) => {
                for line in out.stdout_str().lines() {
                    if let Some((size, path)) = line.split_once('\t')
                        && let Ok(size) = size.parse()
                    {
                        live.sizes.insert(path.to_string(), size);
                    }
                }
            }
            Err(e) => {
                live.sizing_note = Some(format!("directory sizes were not measured ({e})"));
                break;
            }
        }
    }
    let docker = Docker::new(Arc::clone(host));
    if let Ok(containers) = docker.containers_with_sizes() {
        for c in containers {
            let name = c["Names"][0].as_str().unwrap_or_default().trim_start_matches('/').to_string();
            // task containers are the service's: `name.1.<task id>`
            let name = match c["Labels"]["com.docker.swarm.service.name"].as_str() {
                Some(service) => service.to_string(),
                None => name,
            };
            live.size_rw.insert(name, c["SizeRw"].as_u64().unwrap_or(0));
        }
    }
    if let Ok(images) = docker.images() {
        for image in images {
            if image["RepoDigests"].as_array().is_none_or(Vec::is_empty) {
                live.local_images.extend(
                    image["RepoTags"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).map(str::to_string),
                );
            }
        }
        live.local_images.sort();
    }
    let info = Path::new("/var/lib/dpkg/info");
    for name in host.read_dir(info).unwrap_or_default().into_iter().filter(|n| n.ends_with(".list")) {
        live.package_owned
            .extend(host.read_to_string(&info.join(name)).unwrap_or_default().lines().map(str::to_string));
    }
    let status = host.read_to_string(Path::new("/var/lib/dpkg/status")).unwrap_or_default();
    let mut in_conffiles = false;
    for line in status.lines() {
        if line.starts_with("Conffiles:") {
            in_conffiles = true;
        } else if in_conffiles && line.starts_with(' ') {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() == 2 {
                live.conffiles.push((fields[0].to_string(), fields[1].to_string()));
            }
        } else {
            in_conffiles = false;
        }
    }
    live.installed = ["/var/log/installer", "/etc/machine-id"]
        .iter()
        .filter_map(|p| host.metadata(Path::new(p)).ok())
        .map(|m| m.mtime)
        .min();
    live
}

/// Audit the server from its last snapshot and write `state/audit.md`.
pub fn run(host: &Arc<dyn Host>, config: &Config, state: &StateDir) -> Result<Audit, Error> {
    let view = View::new(host.as_ref(), state);
    if !view.exists() {
        return Err(Error::Usage("no snapshot yet: run `moi snapshot` first (the audit works from it)".into()));
    }
    let live = gather(host, &view);
    let identity = Identity::compute(config, &view);
    let audit = Audit {
        hostname: view.hostname(),
        generated: host::format_rfc3339(host.now()),
        sections: vec![
            reboot::section(&view, config),
            coverage::large(&view, &identity, &live),
            coverage::docker(&view, &identity, &live),
            config::section(&view, config, &live),
            coverage::app_state(&view, &identity, &live),
            placement::section(&view, config, &live),
            misc::secrets(&view),
            misc::listeners(&view),
            misc::monitoring(&view, config),
        ],
    };
    let path = state.audit();
    host.write(&path, audit.markdown(1).as_bytes(), 0o600)
        .map_err(|e| Error::host(format!("cannot write {}", path.display()), e))?;
    Ok(audit)
}
