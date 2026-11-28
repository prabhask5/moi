//! What moi writes down after `moi devbox` changed something: devbox's identity among the backup
//! paths (docs/spec/15-devbox.md § Records). The snapshot and the context file are the caller's.

use crate::config::{Config, Paths};
use crate::host::Host;

/// devbox's identity on the host: its configuration and secrets, and the gateway's wake history.
pub const IDENTITY: [&str; 2] = ["/etc/devbox", "/var/lib/devbox/gateway"];

/// The identity paths `[backup] paths` does not cover yet.
pub fn uncovered(config: &Config) -> Vec<&'static str> {
    IDENTITY
        .iter()
        .copied()
        .filter(|path| !config.backup.paths.iter().any(|covered| crate::backup::under(path, covered)))
        .collect()
}

/// Add devbox's identity paths to `backup.paths` in `moi.toml`, with a comment saying who did. Only
/// when backups are configured: where there is no `[backup]`, the audit proposes the paths instead.
/// Returns the paths that were added.
pub fn extend_backup_paths(host: &dyn Host, config: &Config, paths: &Paths) -> Vec<String> {
    let missing = uncovered(config);
    if !config.backup.enabled || missing.is_empty() {
        return Vec::new();
    }
    let Ok(text) = host.read_to_string(&paths.config) else { return Vec::new() };
    let Some(new) = with_paths(&text, &missing) else { return Vec::new() };
    // what is written must still be a configuration
    if new.parse::<toml::Table>().is_err() || host.write(&paths.config, new.as_bytes(), 0o644).is_err() {
        return Vec::new();
    }
    missing.iter().map(ToString::to_string).collect()
}

/// `moi.toml` with `extra` appended to the `paths` array of `[backup]`. `None` when the array
/// cannot be found the way people write it.
pub fn with_paths(text: &str, extra: &[&str]) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let table = lines.iter().position(|l| l.trim() == "[backup]")?;
    let end_of_table = lines
        .iter()
        .enumerate()
        .skip(table + 1)
        .find(|(_, l)| l.trim_start().starts_with('[') && !l.trim_start().starts_with("[["))
        .map_or(lines.len(), |(i, _)| i);
    let start = (table + 1..end_of_table).find(|i| {
        let line = lines[*i].trim_start();
        line.strip_prefix("paths").is_some_and(|rest| rest.trim_start().starts_with('='))
    })?;
    // the bracket that closes the array: the first `]` outside a string and a comment
    let mut close: Option<(usize, usize)> = None;
    let mut opened = false;
    'search: for (i, line) in lines.iter().enumerate().take(end_of_table).skip(start) {
        let mut in_string = false;
        for (at, c) in line.char_indices() {
            match c {
                '"' => in_string = !in_string,
                '#' if !in_string => break,
                '[' if !in_string => opened = true,
                ']' if !in_string && opened => {
                    close = Some((i, at));
                    break 'search;
                }
                _ => {}
            }
        }
    }
    let (line, at) = close?;
    let added: Vec<String> = extra.iter().map(|p| format!("{p:?}")).collect();
    let before = lines[line][..at].trim_end();
    // an entry before the bracket wants a comma; an array opened on this line with nothing in it does not
    let empty_so_far = before.ends_with('[') || before.ends_with(',') || before.is_empty();
    let mut out: Vec<String> = lines[..line].iter().map(ToString::to_string).collect();
    out.push(format!(
        "{before}{}{}]{}   # devbox's identity, added by `moi devbox up`",
        if empty_so_far { "" } else { ", " },
        added.join(", "),
        &lines[line][at + 1..]
    ));
    out.extend(lines[line + 1..].iter().map(ToString::to_string));
    Some(out.join("\n") + "\n")
}
