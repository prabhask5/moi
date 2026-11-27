//! systemd: which units are enabled, the owner's own unit files, and what triggers running services.
//!
//! Read with `systemctl` (feature-detected, read-only verbs). The D-Bus client the spec plans replaces
//! these calls when the daemon needs it (M6); the files written here do not change.

use std::path::Path;

use super::collect::lines;
use super::{Collector, Ctx, Output, run_tool};
use crate::host::FileKind;

pub(crate) struct Systemd;

const UNIT_SUFFIXES: [&str; 8] = [".service", ".timer", ".mount", ".automount", ".path", ".socket", ".target", ".conf"];

impl Collector for Systemd {
    fn name(&self) -> &'static str {
        "systemd"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let mut out = Output::default();
        let first_column = |text: &str| -> Vec<String> {
            let mut v: Vec<String> =
                text.lines().filter_map(|l| l.split_whitespace().next()).map(str::to_string).collect();
            v.sort();
            v
        };
        if let Some(enabled) =
            run_tool(host, "systemctl", &["list-unit-files", "--state=enabled", "--no-legend", "--no-pager"])
        {
            out.stable("systemd/enabled-units.txt", lines(first_column(&enabled)));
        }
        if let Some(states) =
            run_tool(host, "systemctl", &["list-unit-files", "--type=service", "--no-legend", "--no-pager"])
        {
            let mut rows: Vec<String> = states
                .lines()
                .filter_map(|l| {
                    let mut f = l.split_whitespace();
                    Some(format!("{} {}", f.next()?, f.next()?))
                })
                .collect();
            rows.sort();
            out.stable("systemd/unit-file-states.txt", lines(rows));
        }

        // The owner's units: regular files in /etc/systemd/system and its drop-in directories. Units that
        // packages enable are symlinks there, and snapd's generated units are not the owner's.
        let root = Path::new("/etc/systemd/system");
        let mut custom = Vec::new();
        let scan = |dir: &Path, custom: &mut Vec<String>| {
            for name in host.read_dir(dir).unwrap_or_default() {
                let path = dir.join(&name);
                let regular = host.symlink_metadata(&path).is_ok_and(|m| m.kind == FileKind::File);
                if regular
                    && UNIT_SUFFIXES.iter().any(|s| name.ends_with(s))
                    && !name.starts_with("snap-")
                    && !name.starts_with("snap.")
                {
                    custom.push(path.display().to_string());
                }
            }
        };
        scan(root, &mut custom);
        for name in host.read_dir(root).unwrap_or_default() {
            let dir = root.join(&name);
            if host.symlink_metadata(&dir).is_ok_and(|m| m.kind == FileKind::Dir) {
                scan(&dir, &mut custom);
            }
        }
        custom.sort();
        out.stable("systemd/custom-units.txt", lines(custom));

        let running = run_tool(
            host,
            "systemctl",
            &["list-units", "--type=service", "--state=running", "--no-legend", "--no-pager", "--plain"],
        );
        if let Some(running) = running {
            let units = first_column(&running);
            // one call for all of them: blocks of "Id=…\nTriggeredBy=…" separated by blank lines
            let mut args = vec!["show", "-p", "Id", "-p", "TriggeredBy"];
            args.extend(units.iter().map(String::as_str));
            let mut triggered = Vec::new();
            if !units.is_empty()
                && let Some(shown) = run_tool(host, "systemctl", &args)
            {
                for block in shown.split("\n\n") {
                    let value = |key: &str| block.lines().find_map(|l| l.strip_prefix(key)).unwrap_or_default().trim();
                    if !value("TriggeredBy=").is_empty() {
                        triggered.push(format!("{} {}", value("Id="), value("TriggeredBy=")));
                    }
                }
            }
            triggered.sort();
            out.stable("systemd/triggered-by.txt", lines(triggered));
            out.volatile("running-services.txt", running);
        }
        if let Some(failed) =
            run_tool(host, "systemctl", &["list-units", "--failed", "--no-legend", "--no-pager", "--plain"])
        {
            out.volatile("failed-units.txt", failed);
        }
        Ok(out)
    }
}
