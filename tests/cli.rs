//! CLI surface: every command from CLAUDE.md §2 exists, `--version` carries build info,
//! unimplemented commands exit 3 and name their milestone.

use std::process::Command;

fn moi(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_moi")).args(args).output().unwrap_or_else(|e| panic!("run moi: {e}"))
}

#[test]
fn help_lists_every_command() {
    let out = moi(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for cmd in [
        "init",
        "snapshot",
        "log",
        "diff",
        "context",
        "notes",
        "audit",
        "check",
        "monitors",
        "backup",
        "push",
        "pull",
        "restore",
        "verify",
        "recovery-card",
        "firewall",
        "migrate",
        "config",
        "status",
        "daemon",
        "uninstall",
    ] {
        assert!(help.contains(&format!("  {cmd} ")), "missing command `{cmd}` in --help:\n{help}");
    }
}

#[test]
fn version_has_build_info() {
    let out = moi(&["--version"]);
    assert!(out.status.success());
    let v = String::from_utf8_lossy(&out.stdout);
    assert!(v.starts_with("moi "), "{v}");
    assert!(v.contains(env!("CARGO_PKG_VERSION")), "{v}");
    assert!(v.contains('(') && v.contains(')'), "{v}");
}

#[test]
fn unimplemented_commands_exit_3_and_name_the_milestone() {
    let out = moi(&["snapshot"]);
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("milestone M2"), "{err}");
}

#[test]
fn bad_usage_exits_2() {
    let out = moi(&["no-such-command"]);
    assert_eq!(out.status.code(), Some(2));
}
