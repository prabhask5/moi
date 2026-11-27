//! What the packages ship next to the binary — the unit, the completions, the man page — is generated
//! from the code and committed under `packaging/`; these tests keep the two the same
//! (`MOI_UPDATE_GOLDEN=1 cargo test --test packaging` regenerates). Spec: docs/spec/12-distribution.md.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::host::Host;
use cestmoi::{cli, init};
use clap_complete::Shell;

fn packaging(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging").join(file)
}

#[test]
fn the_packaged_unit_is_the_one_init_writes_for_usr_bin() {
    common::assert_golden(&packaging("moi.service"), &init::unit_text("/usr/bin/moi"));
}

#[test]
fn completions_and_man_page_are_up_to_date() {
    common::assert_golden(&packaging("completions/moi.bash"), &cli::completions(Shell::Bash));
    common::assert_golden(&packaging("completions/_moi"), &cli::completions(Shell::Zsh));
    common::assert_golden(&packaging("completions/moi.fish"), &cli::completions(Shell::Fish));
    let man = cli::man_page();
    assert!(man.contains(".TH moi 1") && man.contains("restore"), "{man}");
    common::assert_golden(&packaging("moi.1"), &man);
}

#[test]
fn the_commands_print_what_is_packaged() {
    let host: Arc<dyn Host> = Arc::new(common::minimal());
    let mut out = Vec::new();
    cli::execute(&cli::parse(&["completions", "bash"]).unwrap(), host.clone(), &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), std::fs::read_to_string(packaging("completions/moi.bash")).unwrap());
    let mut out = Vec::new();
    cli::execute(&cli::parse(&["man"]).unwrap(), host, &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), std::fs::read_to_string(packaging("moi.1")).unwrap());
}

#[test]
fn a_packaged_unit_is_switched_on_not_written_again() {
    let host = common::minimal_rw("packaging-unit");
    for (path, text) in [("/usr/bin/moi", ""), (init::PACKAGED_UNIT, "packaged")] {
        host.create_dir_all(Path::new(path).parent().unwrap(), 0o755).unwrap();
        host.write(Path::new(path), text.as_bytes(), 0o755).unwrap();
    }
    host.add_command("systemctl daemon-reload", 0, "");
    host.add_command("systemctl enable --now moi.service", 0, "");
    assert_eq!(init::install_service(host.as_ref()).unwrap(), "/usr/bin/moi");
    assert!(!host.exists(Path::new(init::UNIT_PATH)), "no second unit in /etc");
    assert!(host.commands_run().contains(&"systemctl enable --now moi.service".to_string()));
    // uninstall switches it off and leaves the file to the package manager
    host.add_command("systemctl disable --now moi.service", 0, "");
    let done = init::uninstall(host.as_ref(), None);
    assert!(done.iter().any(|d| d.contains("belongs to the package")), "{done:?}");
    assert_eq!(host.read_to_string(Path::new(init::PACKAGED_UNIT)).unwrap(), "packaged");
}
