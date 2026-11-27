//! Directory structure (not contents) of each data root, to `capture.tree_depth`: enough to recreate the
//! layout with the right owners on a new disk. Volatile — media libraries reshuffle constantly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::storage::data_roots;
use super::{Collector, Ctx, Output};
use crate::host::FileKind;

pub(crate) struct Trees;

fn skipped(name: &str) -> bool {
    ["lost+found", "@eaDir", ".snapshots", "node_modules", ".git"].contains(&name) || name.starts_with(".Trash-")
}

impl Collector for Trees {
    fn name(&self) -> &'static str {
        "trees"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let users = super::id_names(host, "/etc/passwd");
        let groups = super::id_names(host, "/etc/group");
        let name = |map: &BTreeMap<u32, String>, id: u32| map.get(&id).cloned().unwrap_or_else(|| id.to_string());
        let max_depth = cx.config.capture.tree_depth as usize;
        let mut out = Output::default();
        for root in data_roots(cx) {
            let Ok(root_md) = host.metadata(Path::new(&root)) else { continue };
            if root_md.kind != FileKind::Dir {
                continue;
            }
            let mut rows = Vec::new();
            let mut stack = vec![(PathBuf::from(&root), 0usize)];
            while let Some((dir, depth)) = stack.pop() {
                let Ok(md) = host.symlink_metadata(&dir) else { continue };
                // stay on the root's filesystem: another disk mounted below is its own data root
                if md.kind != FileKind::Dir || md.dev != root_md.dev {
                    continue;
                }
                rows.push((
                    dir.display().to_string(),
                    format!("{:o}\t{}\t{}", md.mode, name(&users, md.uid), name(&groups, md.gid)),
                ));
                if depth < max_depth {
                    for child in host.read_dir(&dir).unwrap_or_default() {
                        if !skipped(&child) {
                            stack.push((dir.join(child), depth + 1));
                        }
                    }
                }
            }
            rows.sort();
            let file = match root.trim_start_matches('/').replace('/', "_") {
                empty if empty.is_empty() => "root".to_string(),
                name => name,
            };
            let text: String = rows.iter().map(|(path, meta)| format!("{meta}\t{path}\n")).collect();
            out.volatile(&format!("trees/{file}.tsv"), text);
        }
        Ok(out)
    }
}
