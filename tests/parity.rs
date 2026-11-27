//! Parity suites (docs/spec/05-checks.md § Parity contract): for the machine state of each case, the
//! built-in check must produce exactly the status and message the legacy script printed.
//!
//! A case is `tests/parity/<check>/<case>/`: `host/` (a fixture case layered over `hosts/cestmoi`),
//! `prev.json` (what the previous run left behind), `expected` (status, then the message — trailing
//! spaces included) and `README.md` (the branch of the script it covers).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::checks::{self, Memory, Mode};
use cestmoi::config::{self, Paths};
use cestmoi::host::{FixtureHost, Host};
use cestmoi::state::StateDir;

fn parity_dir(check: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parity").join(check)
}

fn cases(check: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(parity_dir(check))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Run one case; returns (expected, actual) as `status\nmessage`.
fn run_case(check: &str, case: &str) -> (String, String) {
    let dir = parity_dir(check).join(case);
    let overlay = common::scratch(&format!("parity-{check}-{case}"));
    let fixture = FixtureHost::open(&common::fixture_dir("cestmoi"))
        .unwrap()
        .with_case(&dir.join("host"))
        .unwrap()
        .with_overlay(&overlay)
        .unwrap();
    let host: Arc<dyn Host> = Arc::new(fixture);
    let loaded = config::load(host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let config = Arc::new(loaded.config);
    let state = StateDir::new(config.state.dir.clone());
    state.ensure(host.as_ref()).unwrap();
    let db = state.open_db(host.as_ref()).unwrap();

    // what the previous run left behind
    let prev: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("prev.json")).unwrap()).unwrap();
    let mut memory = Memory::default();
    for (key, value) in prev["values"].as_object().into_iter().flatten() {
        memory.values.insert(key.clone(), value.as_str().unwrap().to_string());
    }
    for (condition, runs) in prev["sustained"].as_object().into_iter().flatten() {
        memory.sustained.insert(condition.clone(), u32::try_from(runs.as_u64().unwrap()).unwrap());
    }
    memory.save(&db, check, 0).unwrap();

    let the_check = checks::enabled(&config)
        .into_iter()
        .find(|c| c.name() == check)
        .unwrap_or_else(|| panic!("{check} is not enabled"));
    let result = checks::run(&host, &config, &db, &the_check, Mode::Record).unwrap();
    let expected = std::fs::read_to_string(dir.join("expected")).unwrap();
    // the message may end in a space: only the final newline of the file is not part of it
    let expected = expected.strip_suffix('\n').unwrap_or(&expected).to_string();
    (expected, format!("{}\n{}", result.status.as_str(), result.message))
}

fn run_suite(check: &str, required: &[&str]) {
    let names = cases(check);
    for needed in required {
        assert!(
            names.iter().any(|n| n == needed),
            "tests/parity/{check}/{needed} is missing: every branch of the script needs a case"
        );
    }
    let mut failures = Vec::new();
    for case in &names {
        let dir = parity_dir(check).join(case);
        assert!(dir.join("README.md").is_file(), "{check}/{case} must say which branch it covers (README.md)");
        let (expected, actual) = run_case(check, case);
        if expected != actual {
            failures.push(format!("{check}/{case}\n  expected: {expected:?}\n  actual:   {actual:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ from the legacy script's output:\n{}",
        failures.len(),
        names.len(),
        failures.join("\n")
    );
}

#[test]
fn resources_reproduces_resource_check_sh() {
    run_suite(
        "resources",
        &[
            "ok",
            "first-run",
            "cpu-counters-unchanged",
            "cpu-hot-first-run",
            "cpu-hot-sustained",
            "cpu-cooled-down",
            "mem-hot-first-run",
            "mem-hot-sustained",
            "swap-high",
            "no-swap",
            "temp-hot-first-run",
            "temp-hot-sustained",
            "no-cpu-sensor",
            "other-sensors-ignored",
            "oom",
            "restarts",
            "restarts-first-run",
            "everything",
        ],
    );
}

#[test]
fn storage_reproduces_storage_check_sh() {
    run_suite(
        "storage",
        &[
            "ok",
            "disk-missing",
            "smart-timeout",
            "smart-unreadable",
            "smartctl-not-installed",
            "smart-failing",
            "smart-failing-and-threshold",
            "smart-other-bits",
            "nvme-critical-warning",
            "nvme-worn",
            "nvme-no-wear-value",
            "nvme-media-errors",
            "nvme-too-hot",
            "hdd-bad-sectors",
            "hdd-too-hot",
            "hdd-temp-from-190",
            "hdd-no-temp",
            "kernel-errors",
            "disk-full",
            "inodes",
            "no-inode-count",
            "df-failed",
            "read-only",
            "not-mounted",
            "not-writable",
            "everything",
        ],
    );
}

#[test]
fn expected_files_are_never_empty_and_name_a_status() {
    for check in ["resources", "storage"] {
        for case in cases(check) {
            let text = std::fs::read_to_string(parity_dir(check).join(&case).join("expected")).unwrap();
            let (status, message) =
                text.split_once('\n').unwrap_or_else(|| panic!("{check}/{case}: expected needs two lines"));
            assert!(status == "up" || status == "down", "{check}/{case}: {status}");
            assert!(!message.trim().is_empty(), "{check}/{case}");
        }
    }
}

// ------------------------------------------------------------------ the legacy script itself as the judge

/// Runs `reference/cestmoi/scripts/resource-check.sh` — the real script, with its absolute paths pointed
/// into a sandbox and `journalctl`, `docker` and `curl` replaced by stubs that serve the case's data —
/// and checks that it prints exactly what the case's `expected` file says. This is what makes the
/// `expected` files evidence rather than a transcription of the implementation. (The storage script
/// parses smartctl's text output, which the cases do not carry, so its files are derived by hand.)
#[test]
fn the_legacy_resource_script_prints_what_the_expected_files_say() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("reference/cestmoi/scripts/resource-check.sh");
    let Ok(source) = std::fs::read_to_string(&script) else {
        eprintln!("reference/ is gone (after M12): the expected files stand on their own");
        return;
    };
    if std::process::Command::new("bash").arg("--version").output().is_err() {
        eprintln!("bash is not installed: skipping");
        return;
    }
    let base = common::fixture_dir("cestmoi");
    let mut failures = Vec::new();
    for case in cases("resources") {
        let case_dir = parity_dir("resources").join(&case).join("host");
        let sandbox = common::scratch(&format!("legacy-resources-{case}"));
        let absent: Vec<String> =
            std::fs::read_to_string(case_dir.join("absent")).unwrap_or_default().lines().map(str::to_string).collect();
        // the files the script reads: the case's version if it has one, else the fixture host's
        let source_of = |rel: &str| -> Option<PathBuf> {
            if absent.iter().any(|gone| format!("/{rel}").starts_with(gone.as_str())) {
                return None;
            }
            [case_dir.join("root").join(rel), base.join("root").join(rel)].into_iter().find(|p| p.exists())
        };
        for rel in ["proc/stat", "proc/meminfo"] {
            let target = sandbox.join(rel);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(source_of(rel).unwrap(), target).unwrap();
        }
        for hwmon in std::fs::read_dir(base.join("root/sys/class/hwmon")).unwrap() {
            let name = hwmon.unwrap().file_name().to_string_lossy().into_owned();
            for file in std::fs::read_dir(base.join("root/sys/class/hwmon").join(&name)).unwrap() {
                let rel = format!("sys/class/hwmon/{name}/{}", file.unwrap().file_name().to_string_lossy());
                if let Some(from) = source_of(&rel) {
                    let target = sandbox.join(&rel);
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::copy(from, target).unwrap();
                }
            }
        }
        // what the previous run left in $STATE
        let state = sandbox.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let prev: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(parity_dir("resources").join(&case).join("prev.json")).unwrap(),
        )
        .unwrap();
        if let Some(cpu) = prev["values"]["cpu"].as_str() {
            std::fs::write(state.join("cpu"), format!("{cpu}\n")).unwrap();
        }
        for (condition, _) in prev["sustained"].as_object().into_iter().flatten() {
            std::fs::write(state.join(format!("hot-{condition}")), "").unwrap();
        }
        if let Some(restarts) = prev["values"]["restarts"].as_str() {
            let counts: std::collections::BTreeMap<String, u64> = serde_json::from_str(restarts).unwrap();
            std::fs::write(
                state.join("restarts"),
                counts.iter().map(|(name, n)| format!("{name} {n}\n")).collect::<String>(),
            )
            .unwrap();
        }
        // stubs: the kernel log of the case, the containers' restart counts, and a curl that pushes nowhere
        let stubs = sandbox.join("bin");
        std::fs::create_dir_all(&stubs).unwrap();
        let key = FixtureHost::command_key("journalctl -k --since -6min --no-pager -q");
        let journal = [case_dir.join("commands"), base.join("commands")]
            .into_iter()
            .map(|d| d.join(format!("{key}.out")))
            .find(|p| p.exists())
            .unwrap();
        std::fs::copy(journal, sandbox.join("journal.txt")).unwrap();
        let list: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(base.join("docker/containers/json.json")).unwrap()).unwrap();
        let mut inspect = String::new();
        for summary in list.as_array().unwrap() {
            let rel = format!("docker/containers/{}/json.json", summary["Id"].as_str().unwrap());
            let file = [case_dir.join(&rel), base.join(&rel)].into_iter().find(|p| p.exists()).unwrap();
            let c: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            inspect.push_str(&format!("{} {}\n", c["Name"].as_str().unwrap(), c["RestartCount"]));
        }
        std::fs::write(sandbox.join("inspect.txt"), inspect).unwrap();
        let stub = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let path = stubs.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        stub("journalctl", &format!("cat '{}'", sandbox.join("journal.txt").display()));
        stub(
            "docker",
            &format!("case \"$1\" in ps) echo all ;; inspect) cat '{}' ;; esac", sandbox.join("inspect.txt").display()),
        );
        stub("curl", "exit 0");
        // the script, with nothing changed but where it looks
        let root = sandbox.display().to_string();
        let redirected = source
            .replace("/proc/stat", &format!("{root}/proc/stat"))
            .replace("/proc/meminfo", &format!("{root}/proc/meminfo"))
            .replace("/sys/class/hwmon/hwmon*", &format!("{root}/sys/class/hwmon/hwmon*"))
            .replace("STATE=/var/lib/resource-check", &format!("STATE={root}/state"));
        assert_eq!(redirected.matches(&root).count(), 8, "the script changed: review the redirections");
        std::fs::write(sandbox.join("resource-check.sh"), redirected).unwrap();
        let path = format!("{}:/usr/bin:/bin", stubs.display());
        let out = std::process::Command::new("bash")
            .arg(sandbox.join("resource-check.sh"))
            .env_clear()
            .env("PATH", path)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        // the script prints "status: message"
        let printed = String::from_utf8_lossy(&out.stdout);
        let printed = printed.strip_suffix('\n').unwrap_or(&printed);
        let expected = std::fs::read_to_string(parity_dir("resources").join(&case).join("expected")).unwrap();
        let expected = expected.strip_suffix('\n').unwrap_or(&expected).replacen('\n', ": ", 1);
        if printed != expected {
            failures.push(format!(
                "{case}\n  script:   {printed:?}\n  expected: {expected:?}\n  stderr: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "the legacy script disagrees with {} expected file(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
