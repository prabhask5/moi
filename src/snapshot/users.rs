//! Human users and their groups, plus the dotfiles named in `capture.home_files`.

use std::path::Path;

use super::collect::{LIGHT_PRUNE, lines};
use super::{Collector, Ctx, Output, Walk, walk_files};

pub(crate) struct Users;

/// uid 0 and the range distributions give to people.
pub(crate) fn is_person(uid: u32) -> bool {
    uid == 0 || (1000..60000).contains(&uid)
}

/// (name, uid, home) of root and every human user in `/etc/passwd`.
pub(crate) fn people(passwd: &str) -> Vec<(String, u32, String)> {
    passwd
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            let uid: u32 = f.get(2)?.parse().ok()?;
            let home = f.get(5)?;
            is_person(uid).then(|| (f[0].to_string(), uid, (*home).to_string()))
        })
        .collect()
}

impl Collector for Users {
    fn name(&self) -> &'static str {
        "users"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let passwd = host.read_to_string(Path::new("/etc/passwd")).map_err(|e| format!("/etc/passwd: {e}"))?;
        let group = host.read_to_string(Path::new("/etc/group")).unwrap_or_default();
        let mut out = Output::default();
        out.stable(
            "users/users.passwd",
            lines(passwd.lines().filter(|l| l.split(':').nth(2).and_then(|u| u.parse().ok()).is_some_and(is_person))),
        );
        // groups people can be in: their own range, or any group with members
        out.stable(
            "users/groups.group",
            lines(group.lines().filter(|l| {
                let f: Vec<&str> = l.split(':').collect();
                let gid = f.get(2).and_then(|g| g.parse::<u32>().ok());
                gid.is_some_and(|g| (1000..60000).contains(&g)) || f.get(3).is_some_and(|members| !members.is_empty())
            })),
        );
        let walk = Walk {
            max_depth: 4,
            prune_names: LIGHT_PRUNE.iter().map(ToString::to_string).collect(),
            limit: 2000,
            ..Walk::default()
        };
        for (_, _, home) in people(&passwd) {
            if home.is_empty() || home == "/" {
                continue;
            }
            for file in &cx.config.capture.home_files {
                out.grab_all(walk_files(host, &Path::new(&home).join(file), &walk));
            }
        }
        Ok(out)
    }
}
