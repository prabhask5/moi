//! CLI surface: every command from CLAUDE.md §2 exists, `--version` carries build info,
//! unimplemented commands exit 3 and name their milestone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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
    let out = moi(&["migrate", "finish"]);
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("milestone M9"), "{err}");
}

#[test]
fn bad_usage_exits_2() {
    let out = moi(&["no-such-command"]);
    assert_eq!(out.status.code(), Some(2));
}

// ------------------------------------------------------------------ M1: config check, --json plumbing, exit codes
// The binary only ever sees the `minimal` fixture host (MOI_ROOT), never this machine.

mod common;

fn moi_on_fixture(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_moi"))
        .args(args)
        .env_clear()
        .env("MOI_ROOT", common::fixture_dir("minimal"))
        .envs(env.iter().copied())
        .output()
        .unwrap_or_else(|e| panic!("run moi: {e}"))
}

fn golden(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/config-check").join(name)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn config_check_on_an_empty_config_prints_the_defaults() {
    let out = moi_on_fixture(&["config", "check"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let reference = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/moi.toml"))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(reference.ends_with(&text(&out.stdout)), "stdout is not the reference config:\n{}", text(&out.stdout));
    assert_eq!(text(&out.stderr), "moi: /etc/moi/moi.toml: ok (no secrets used)\n");
}

#[test]
fn quiet_keeps_stdout_and_drops_the_ok_line() {
    let out = moi_on_fixture(&["config", "check", "--quiet"], &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!out.stdout.is_empty());
    assert_eq!(text(&out.stderr), "");
}

#[test]
fn config_check_masks_secrets_from_file_and_environment() {
    let out = moi_on_fixture(&["--config", "/cases/full/moi.toml", "config", "check"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(!all.contains("FAKE"), "{all}");
    assert!(all.contains("7 secrets resolved"), "{all}");
    common::assert_golden(&golden("full.toml"), &text(&out.stdout));
}

#[test]
fn config_check_json_is_a_stable_contract() {
    let out = moi_on_fixture(&["--config", "/cases/full/moi.toml", "config", "check", "--json"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(!text(&out.stdout).contains("FAKE"));
    common::assert_golden(&golden("full.json"), &text(&out.stdout));
}

#[test]
fn config_check_reports_every_error_and_exits_2() {
    let out = moi_on_fixture(&["--config", "/cases/bad/moi.toml", "config", "check"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout), "");
    common::assert_golden(&golden("bad.stderr"), &text(&out.stderr));
}

#[test]
fn config_check_json_errors_are_a_document_on_stdout() {
    let out = moi_on_fixture(&["--config", "/cases/bad/moi.toml", "config", "check", "--json"], &[]);
    assert_eq!(out.status.code(), Some(2));
    common::assert_golden(&golden("bad.json"), &text(&out.stdout));
    // people still get the readable form on stderr
    assert!(text(&out.stderr).contains("server.nmae: unknown key"));
}

#[test]
fn secrets_from_the_environment_reach_the_binary_and_stay_masked() {
    let out = moi_on_fixture(
        &["--config", "/cases/bad/moi.toml", "config", "check"],
        &[("MOI_SECRET_RESTIC_PASSWORD", "FAKEenv-restic-password")],
    );
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2));
    assert!(!err.contains("restic_password"), "the secret is now defined:\n{err}");
    assert!(!err.contains("FAKEenv"), "{err}");
}

#[test]
fn secrets_file_with_open_permissions_is_refused() {
    // git checks the fixture out 0644 and the modes manifest does not tighten this one
    let out = moi_on_fixture(&["--config", "/cases/open-secrets/moi.toml", "config", "check"], &[]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(err.contains("/cases/open-secrets/secrets.toml: mode 0644 lets other users read the secrets"), "{err}");
    assert!(!err.contains("FAKE"), "{err}");
}

#[test]
fn secrets_flag_and_env_override_the_sibling_default() {
    let out = moi_on_fixture(
        &["--config", "/cases/open-secrets/moi.toml", "--secrets", "/cases/full/secrets.toml", "config", "check", "-q"],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = moi_on_fixture(
        &["--config", "/cases/open-secrets/moi.toml", "config", "check", "-q"],
        &[("MOI_SECRETS", "/cases/full/secrets.toml")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

#[test]
fn missing_config_exits_2_and_says_what_to_do() {
    let out = moi_on_fixture(&["--config", "/etc/moi/nope.toml", "config", "check"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("/etc/moi/nope.toml: not found: run `moi init`"), "{}", text(&out.stderr));
}

#[test]
fn config_path_prints_both_files() {
    let out = moi_on_fixture(&["config", "path"], &[]);
    assert_eq!(text(&out.stdout), "config   /etc/moi/moi.toml\nsecrets  /etc/moi/secrets.toml\n");
    let out = moi_on_fixture(&["config", "path", "--json"], &[("MOI_CONFIG", "/srv/moi.toml")]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(v, serde_json::json!({ "ok": true, "config": "/srv/moi.toml", "secrets": "/srv/secrets.toml" }));
}

#[test]
fn json_errors_cover_unimplemented_commands_too() {
    let out = moi_on_fixture(&["migrate", "finish", "--json"], &[]);
    assert_eq!(out.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["kind"], "not_implemented");
    assert_eq!(v["error"]["exit_code"], 3);
    assert!(text(&out.stderr).contains("milestone M9"));
}

#[test]
fn a_moi_root_that_is_not_a_fixture_is_an_error_not_a_fallback_to_the_real_system() {
    let out = Command::new(env!("CARGO_BIN_EXE_moi"))
        .args(["config", "check"])
        .env_clear()
        .env("MOI_ROOT", common::scratch("cli-not-a-fixture"))
        .output()
        .unwrap_or_else(|e| panic!("run moi: {e}"));
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("is not a fixture host"), "{}", text(&out.stderr));
}

#[test]
fn context_share_is_wired() {
    let out = moi(&["context", "share", "--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--ttl") && help.contains("--revoke") && help.contains("--list"), "{help}");
    // on a server without [context.share] it says how to set it up
    let out = moi_on_fixture(&["context", "share", "--ttl", "1h"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("context share: not configured: add a [context.share] section"),
        "{}",
        text(&out.stderr)
    );
}

// ------------------------------------------------------------------ M2: snapshot

#[test]
fn snapshot_captures_a_fixture_host_into_the_overlay_and_reports_changes() {
    let overlay = common::scratch("cli-snapshot");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_moi"))
            .args(args)
            .env_clear()
            .env("MOI_ROOT", common::fixture_dir("cestmoi"))
            .env("MOI_ROOT_OVERLAY", &overlay)
            .output()
            .unwrap_or_else(|e| panic!("run moi: {e}"))
    };
    let first = run(&["snapshot"]);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first.stderr));
    let line = text(&first.stdout);
    assert!(line.starts_with("snapshot saved to /var/lib/moi/state: "), "{line}");
    assert!(line.contains(" files, recorded "), "{line}");
    assert!(overlay.join("var/lib/moi/state/snapshot/meta/hostname").exists());

    let second = run(&["snapshot", "--json"]);
    assert_eq!(second.status.code(), Some(0), "{}", text(&second.stderr));
    let v: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(v["ok"], true);
    assert_eq!(v["snapshot"]["changed"], serde_json::json!([]));
    assert_eq!(v["snapshot"]["files_read"], 0);
    assert!(v["snapshot"]["collectors"].as_array().is_some_and(|c| c.iter().all(|c| c["error"].is_null())));
    // secrets from the fixture's secrets.toml never reach the output
    assert!(!text(&second.stdout).contains("FAKE-"));
}

#[test]
fn snapshot_without_somewhere_to_write_fails_cleanly() {
    let out = moi_on_fixture(&["snapshot"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("cannot prepare state directory /var/lib/moi"), "{}", text(&out.stderr));
}

// ------------------------------------------------------------------ M2: history

fn moi_with_overlay(overlay: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_moi"))
        .args(args)
        .env_clear()
        .env("MOI_ROOT", common::fixture_dir("cestmoi"))
        .env("MOI_ROOT_OVERLAY", overlay)
        .output()
        .unwrap_or_else(|e| panic!("run moi: {e}"))
}

#[test]
fn log_and_diff_before_the_first_snapshot_say_so() {
    let overlay = common::scratch("cli-no-history");
    let out = moi_with_overlay(&overlay, &["log"]);
    assert_eq!((out.status.code(), text(&out.stdout).as_str()), (Some(0), "no history yet: run `moi snapshot`\n"));
    let out = moi_with_overlay(&overlay, &["diff", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!overlay.join("var/lib/moi/state/.git").exists(), "looking at history must not create it");
}

#[test]
fn snapshot_log_and_diff_tell_the_story_of_a_change() {
    let overlay = common::scratch("cli-history");
    let first = moi_with_overlay(&overlay, &["snapshot"]);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first.stderr));
    assert!(text(&first.stdout).ends_with("\n  initial snapshot of cestmoi\n"), "{}", text(&first.stdout));
    let again = moi_with_overlay(&overlay, &["snapshot"]);
    assert!(text(&again.stdout).ends_with("no configuration changes\n"), "{}", text(&again.stdout));

    // the owner changes a database password in a compose env file
    let env = overlay.join("opt/services/outline/.env");
    std::fs::create_dir_all(env.parent().unwrap_or(&overlay)).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(
        &env,
        "SECRET_KEY=FAKEoutlinesecretkey00000000000000\nPOSTGRES_PASSWORD=FAKEnewpassword99\nLOG_LEVEL=debug\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let third = moi_with_overlay(&overlay, &["snapshot", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&third.stdout).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(v["commit"]["subject"], "changed: /opt/services/outline/.env");

    let log = moi_with_overlay(&overlay, &["log"]);
    assert_eq!(
        text(&log.stdout),
        "2026-11-26 17:00  changed: /opt/services/outline/.env\n2026-11-26 17:00  initial snapshot of cestmoi\n"
    );
    let log = moi_with_overlay(&overlay, &["log", "-n", "1", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(v["commits"].as_array().map(Vec::len), Some(1));

    // the diff shows what changed without showing the secrets
    let diff = moi_with_overlay(&overlay, &["diff"]);
    assert_eq!(diff.status.code(), Some(0), "{}", text(&diff.stderr));
    let shown = text(&diff.stdout);
    assert!(shown.contains("M\t/opt/services/outline/.env\n"), "{shown}");
    assert!(shown.contains("--- /opt/services/outline/.env\n+++ /opt/services/outline/.env\n"), "{shown}");
    assert!(shown.contains("+POSTGRES_PASSWORD=***\n") && shown.contains("+LOG_LEVEL=debug\n"), "{shown}");
    assert!(!shown.contains("FAKE"), "{shown}");
    assert!(!shown.contains("files.manifest.tsv"), "{shown}");
    assert!(shown.ends_with("(secrets are masked; `sudo moi diff HEAD --show-secrets` shows them)\n"), "{shown}");

    // --show-secrets is for root only
    let refused = moi_with_overlay(&overlay, &["diff", "--show-secrets"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(text(&refused.stderr).contains("--show-secrets needs root"), "{}", text(&refused.stderr));
    std::fs::create_dir_all(overlay.join("proc/self")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(overlay.join("proc/self/status"), "Name:\tmoi\nUid:\t0\t0\t0\t0\n")
        .unwrap_or_else(|e| panic!("{e}"));
    let as_root = moi_with_overlay(&overlay, &["diff", "--show-secrets"]);
    assert!(text(&as_root.stdout).contains("+POSTGRES_PASSWORD=FAKEnewpassword99\n"), "{}", text(&as_root.stdout));

    let bad = moi_with_overlay(&overlay, &["diff", "HEAD~7"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(text(&bad.stderr).contains("no such revision: HEAD~7"), "{}", text(&bad.stderr));
}
