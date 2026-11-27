//! Installed software, read from the package managers' own state files. Nothing is executed for
//! dpkg/apt/snap/flatpak/pipx/npm/cargo: commands like `pipx list` write logs, and a snapshot must leave
//! no trace outside the state dir. Only `rpm -qa` (read-only) is run, on rpm systems.

use std::collections::BTreeSet;
use std::path::Path;

use super::collect::{expand_glob, lines};
use super::{Collector, Ctx, Output, run_tool};
use crate::host::{FileKind, Host};

pub(crate) struct Packages;

impl Collector for Packages {
    fn name(&self) -> &'static str {
        "packages"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let mut out = Output::default();
        let names = |dir: &str| host.read_dir(Path::new(dir)).unwrap_or_default();

        if let Ok(status) = host.read_to_string(Path::new("/var/lib/dpkg/status")) {
            out.stable("packages/family", "debian\n");
            let auto =
                auto_installed(&host.read_to_string(Path::new("/var/lib/apt/extended_states")).unwrap_or_default());
            let (mut manual, mut hold, mut all) = (Vec::new(), Vec::new(), Vec::new());
            for p in dpkg_packages(&status) {
                if !p.installed {
                    continue;
                }
                all.push(format!("{}\t{}", p.name, p.version));
                if p.hold {
                    hold.push(p.name.clone());
                }
                // apt keys auto-installed packages by name and architecture; a foreign-arch package is
                // listed as name:arch by apt-mark, a native one by its bare name.
                if !auto.contains(&(p.name.clone(), p.arch.clone())) {
                    manual.push(p.name);
                }
            }
            manual.sort();
            manual.dedup();
            hold.sort();
            all.sort();
            out.stable("packages/apt-manual.txt", lines(manual));
            out.stable("packages/apt-hold.txt", lines(hold));
            out.volatile("apt-all.tsv", lines(all));
            out.grab_all(super::walk_files(host, Path::new("/usr/share/keyrings"), &super::Walk::default()));
        } else if let Some(rpms) = run_tool(host, "rpm", &["-qa"]) {
            out.stable("packages/family", "rhel\n");
            let mut all: Vec<&str> = rpms.lines().collect();
            all.sort_unstable();
            out.volatile("rpm-all.txt", lines(all));
        }

        // snap: one directory per snap under /snap (besides its own bookkeeping)
        let snaps: Vec<String> = names("/snap").into_iter().filter(|n| n != "bin" && n != "README").collect();
        out.stable("packages/snap.txt", lines(snaps));
        // flatpak: system-wide applications
        out.stable("packages/flatpak.txt", lines(names("/var/lib/flatpak/app")));
        // pipx: one venv per application
        let mut pipx = BTreeSet::new();
        for pattern in ["/root/.local/share/pipx/venvs", "/home/*/.local/share/pipx/venvs", "/opt/pipx/venvs"] {
            for dir in expand_glob(host, pattern) {
                pipx.extend(host.read_dir(&dir).unwrap_or_default());
            }
        }
        out.stable("packages/pipx.txt", lines(pipx));
        // npm -g: top-level directories of the global module dirs (scopes expanded)
        let mut npm = BTreeSet::new();
        for dir in ["/usr/lib/node_modules", "/usr/local/lib/node_modules"] {
            for name in names(dir) {
                if name.starts_with('@') {
                    npm.extend(names(&format!("{dir}/{name}")).into_iter().map(|n| format!("{name}/{n}")));
                } else if !name.starts_with('.') {
                    npm.insert(name);
                }
            }
        }
        out.stable("packages/npm-global.txt", lines(npm));
        // cargo install: the registry of installed crates
        let mut cargo = BTreeSet::new();
        for file in expand_glob(host, "/root/.cargo/.crates.toml")
            .into_iter()
            .chain(expand_glob(host, "/home/*/.cargo/.crates.toml"))
        {
            for line in host.read_to_string(&file).unwrap_or_default().lines() {
                if let Some((spec, _)) = line.split_once(" = ") {
                    let mut parts = spec.trim_matches('"').split(' ');
                    if let (Some(name), Some(version)) = (parts.next(), parts.next()) {
                        cargo.insert(format!("{name} {version}"));
                    }
                }
            }
        }
        out.stable("packages/cargo.txt", lines(cargo));

        let mut local = Vec::new();
        for dir in ["/usr/local/bin", "/usr/local/sbin"] {
            for name in names(dir) {
                let path = format!("{dir}/{name}");
                local.push(format!("{path}\t{}", describe(host, Path::new(&path))));
            }
        }
        out.stable("packages/usr-local-bin.tsv", lines(local));
        out.stable("packages/opt-listing.txt", lines(names("/opt")));
        Ok(out)
    }
}

struct DpkgPackage {
    name: String,
    arch: String,
    version: String,
    installed: bool,
    hold: bool,
}

/// Stanzas of `/var/lib/dpkg/status`.
fn dpkg_packages(status: &str) -> Vec<DpkgPackage> {
    let mut out = Vec::new();
    for stanza in status.split("\n\n") {
        let field = |key: &str| {
            stanza
                .lines()
                .find_map(|l| l.strip_prefix(key).and_then(|rest| rest.strip_prefix(": ")))
                .unwrap_or_default()
        };
        let name = field("Package");
        if name.is_empty() {
            continue;
        }
        // "want flag status", e.g. "install ok installed", "hold ok installed", "deinstall ok config-files"
        let state: Vec<&str> = field("Status").split_whitespace().collect();
        out.push(DpkgPackage {
            name: name.to_string(),
            arch: field("Architecture").to_string(),
            version: field("Version").to_string(),
            installed: state.get(2) == Some(&"installed"),
            hold: state.first() == Some(&"hold"),
        });
    }
    out
}

/// (package, architecture) pairs apt installed only as dependencies.
fn auto_installed(extended_states: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for stanza in extended_states.split("\n\n") {
        let field = |key: &str| {
            stanza
                .lines()
                .find_map(|l| l.strip_prefix(key).and_then(|rest| rest.strip_prefix(": ")))
                .unwrap_or_default()
        };
        if field("Auto-Installed") == "1" {
            out.insert((field("Package").to_string(), field("Architecture").to_string()));
        }
    }
    out
}

/// What kind of file sits in `/usr/local/bin`: enough to tell a script from a binary from a link.
fn describe(host: &dyn Host, path: &Path) -> String {
    match host.symlink_metadata(path).map(|m| m.kind) {
        Ok(FileKind::Symlink) => {
            format!("symlink to {}", host.read_link(path).map(|t| t.display().to_string()).unwrap_or_default())
        }
        Ok(FileKind::Dir) => "directory".to_string(),
        Ok(FileKind::File) => {
            let data = host.read(path).unwrap_or_default();
            if data.starts_with(b"\x7fELF") {
                "ELF executable".to_string()
            } else if data.starts_with(b"#!") {
                let first = String::from_utf8_lossy(&data[..data.len().min(120)]).into_owned();
                format!("script ({})", first.lines().next().unwrap_or_default().trim_start_matches("#!").trim())
            } else if super::collect::is_text(&data) {
                "text".to_string()
            } else {
                "data".to_string()
            }
        }
        _ => "other".to_string(),
    }
}
