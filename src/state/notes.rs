//! The owner's notes (`NOTES.md`): free text under stable `## ` headings that moi merges into the
//! context file next to the thing each heading names.
//!
//! Spec: docs/spec/03-context.md § Owner notes. moi only ever *appends* a stub heading for something
//! new; what the owner wrote is never modified, reordered or deleted.

use crate::error::Error;
use crate::host::Host;
use crate::snapshot::View;
use crate::state::StateDir;

/// How every generated stub starts. A section that still only holds its stub is not a note.
const PLACEHOLDER: &str = "_Not described yet — ";

fn header(hostname: &str) -> String {
    format!(
        "# Owner notes — {hostname}\n\n\
         Free-form notes that moi merges into CONTEXT.md (the file AI assistants read).\n\
         Write *why* things exist, who uses them, URLs, gotchas, plans. Keep the `## ` headings:\n\
         moi adds one for every new stack, container, service, VM, unit and scheduled script,\n\
         and never deletes or rewrites what you wrote. A section that still only holds its\n\
         placeholder line is ignored.\n\n\
         ## Overview\n{PLACEHOLDER}what this server is for, who uses it, how it is reached (domain, VPN, LAN IP…)_\n\n\
         ## Gotchas / do not touch\n{PLACEHOLDER}things that must not be changed, and why_\n\n\
         ## Planned changes / ideas\n{PLACEHOLDER}what you intend to change_\n"
    )
}

/// The general sections every notes file has; everything else describes one thing on the server.
pub const GENERAL: [&str; 3] = ["Overview", "Gotchas / do not touch", "Planned changes / ideas"];

/// `(heading, body)` of every `## ` section, in file order. Bodies are trimmed.
pub fn parse(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            sections.push((heading.trim().to_string(), Vec::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push(line);
        }
    }
    sections.into_iter().map(|(heading, body)| (heading, body.join("\n").trim().to_string())).collect()
}

/// True if a section says something (not empty, not just its placeholder line).
pub fn meaningful(body: &str) -> bool {
    let body = body.trim();
    !(body.is_empty() || (body.starts_with(PLACEHOLDER) && !body.contains('\n')))
}

/// The notes that say something: heading → text.
pub fn read(host: &dyn Host, state: &StateDir) -> Vec<(String, String)> {
    let text = host.read_to_string(&state.notes()).unwrap_or_default();
    parse(&text).into_iter().filter(|(_, body)| meaningful(body)).collect()
}

/// The heading each thing on the server gets, with the hint its stub shows.
pub fn wanted(view: &View<'_>) -> Vec<(String, &'static str)> {
    let mut want = Vec::new();
    let name = |v: &serde_json::Value, key: &str| v[key].as_str().unwrap_or_default().to_string();
    for project in view.compose_projects() {
        want.push((
            format!("Stack: {}", name(&project, "project")),
            "what it does, its URL, who uses it, anything non-obvious",
        ));
    }
    for c in view.containers() {
        if c["labels"].get("com.docker.compose.project").is_none() {
            want.push((
                format!("Container: {}", name(&c, "name")),
                "what this standalone container is (consider moving it to compose)",
            ));
        }
    }
    for service in view.services() {
        want.push((format!("Service: {}", name(&service, "name")), "what this swarm service is"));
    }
    for instance in view.incus_instances() {
        want.push((
            format!("Instance: {instance}"),
            "what this VM or container is for; is its disk backed up or rebuildable?",
        ));
    }
    for unit in view.custom_units() {
        if !unit.ends_with(".conf") {
            let file = unit.rsplit('/').next().unwrap_or(&unit);
            want.push((format!("Unit: {file}"), "what this unit does"));
        }
    }
    for script in view.scheduled_scripts() {
        want.push((format!("Script: {script}"), "what this script does"));
    }
    want
}

/// Create `NOTES.md` if it does not exist and append a stub for everything that has no heading yet.
/// Returns true if the file was written.
pub fn update(host: &dyn Host, state: &StateDir) -> Result<bool, Error> {
    let path = state.notes();
    let view = View::new(host, state);
    let existing = host.read_to_string(&path).ok();
    let mut text = existing.clone().unwrap_or_else(|| header(&view.hostname()));
    let have: Vec<String> = parse(&text).into_iter().map(|(heading, _)| heading).collect();
    let mut added = false;
    for (heading, hint) in wanted(&view) {
        if !have.contains(&heading) {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("\n## {heading}\n{PLACEHOLDER}{hint}_\n"));
            added = true;
        }
    }
    if existing.is_some() && !added {
        return Ok(false);
    }
    host.write(&path, text.as_bytes(), 0o600)
        .map_err(|e| Error::host(format!("cannot write {}", path.display()), e))?;
    Ok(true)
}
