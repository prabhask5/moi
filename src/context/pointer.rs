//! The Claude Code pointer: a marked block in `~/.claude/CLAUDE.md` that tells Claude Code, whenever it
//! runs on this server, to read the context file first. moi owns exactly the text between the markers.

use std::io;
use std::path::{Path, PathBuf};

use crate::host::Host;

const BEGIN: &str = "<!-- moi:begin -->";
const END: &str = "<!-- moi:end -->";

fn block(context_path: &str) -> String {
    format!(
        "{BEGIN}\n\
         ## This server (block maintained by moi — edit outside the markers)\n\
         Before changing anything on this machine, read `{context_path}`: it describes everything that runs here and is regenerated after every change.\n\
         Keep changes reproducible: compose files in existing stack directories, scripts in captured directories, units in `/etc/systemd/system`, jobs via `crontab`, health checks / backups / monitors in `/etc/moi/moi.toml`.\n\
         Afterwards run `sudo moi snapshot` and record the why with `sudo moi notes`.\n\
         {END}\n"
    )
}

/// `(home, gid)` of a user id from `/etc/passwd`.
fn home_of(host: &dyn Host, uid: u32) -> Option<(PathBuf, u32)> {
    let passwd = host.read_to_string(Path::new("/etc/passwd")).ok()?;
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.get(2)?.parse::<u32>().ok()? == uid).then(|| Some((PathBuf::from(f.get(5)?), f.get(3)?.parse().ok()?)))?
    })
}

/// The text with moi's block removed (and the blank line it left behind).
fn without_block(text: &str) -> String {
    match (text.find(BEGIN), text.find(END)) {
        (Some(start), Some(end)) if end > start => {
            let after = text[end + END.len()..].strip_prefix('\n').unwrap_or(&text[end + END.len()..]);
            let before = &text[..start];
            let before = if after.is_empty() { before.trim_end_matches('\n') } else { before };
            let mut out = before.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(after);
            out
        }
        _ => text.to_string(),
    }
}

/// Create or refresh the block in the user's `~/.claude/CLAUDE.md`. Everything outside the markers is
/// left exactly as it is.
pub fn write_pointer(host: &dyn Host, uid: u32, context_path: &str) -> io::Result<()> {
    let (home, gid) = home_of(host, uid).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such user"))?;
    host.metadata(&home)?;
    let dir = home.join(".claude");
    let file = dir.join("CLAUDE.md");
    let existing = host.read_to_string(&file).unwrap_or_default();
    let block = block(context_path);
    let updated = match (existing.find(BEGIN), existing.find(END)) {
        (Some(start), Some(end)) if end > start => {
            let after = existing[end + END.len()..].strip_prefix('\n').unwrap_or(&existing[end + END.len()..]);
            format!("{}{block}{after}", &existing[..start])
        }
        _ if existing.trim().is_empty() => block,
        _ => format!("{}\n\n{block}", existing.trim_end_matches('\n')),
    };
    if updated == existing {
        return Ok(());
    }
    if !host.exists(&dir) {
        host.create_dir_all(&dir, 0o755)?;
        host.set_owner(&dir, uid, gid)?;
    }
    let new_file = !host.exists(&file);
    host.write(&file, updated.as_bytes(), 0o644)?;
    if new_file {
        host.set_owner(&file, uid, gid)?;
    }
    Ok(())
}

/// Remove moi's block from the user's `~/.claude/CLAUDE.md`; delete the file if nothing else is in it.
/// Returns true if something was removed.
pub fn remove_pointer(host: &dyn Host, uid: u32) -> io::Result<bool> {
    let Some((home, _)) = home_of(host, uid) else { return Ok(false) };
    let file = home.join(".claude/CLAUDE.md");
    let Ok(existing) = host.read_to_string(&file) else { return Ok(false) };
    let remaining = without_block(&existing);
    if remaining == existing {
        return Ok(false);
    }
    if remaining.trim().is_empty() {
        host.remove_file(&file)?;
    } else {
        let mode = host.metadata(&file).map_or(0o644, |m| m.mode);
        host.write(&file, remaining.as_bytes(), mode)?;
    }
    Ok(true)
}
