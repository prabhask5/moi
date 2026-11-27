//! Configuration (docs/spec/01-config.md § Acceptance): validation with locations, all errors at once,
//! `secret:` resolution from file and environment, defaults.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use cestmoi::config::{self, Action, AutoOr, Diagnostic, Loaded, MonitorMode, Paths, When};
use cestmoi::error::Error;
use cestmoi::host::{FixtureHost, Host};

fn paths() -> Paths {
    Paths::new(PathBuf::from("/etc/moi/moi.toml"), None)
}

fn load(test: &str, config: &str, secrets: Option<&str>) -> Result<Loaded, Error> {
    let host = common::minimal_rw(test);
    common::write_config(&host, config, secrets);
    config::load(host.as_ref(), &paths())
}

fn diags(r: Result<Loaded, Error>) -> Vec<Diagnostic> {
    match r {
        Err(Error::ConfigInvalid(d)) => d,
        Err(e) => panic!("expected ConfigInvalid, got {e}"),
        Ok(_) => panic!("expected validation errors, config was accepted"),
    }
}

fn lines(r: Result<Loaded, Error>) -> Vec<String> {
    diags(r).iter().map(ToString::to_string).collect()
}

/// The single error a config is expected to produce.
fn only(test: &str, config: &str) -> String {
    let l = lines(load(test, config, None));
    assert_eq!(l.len(), 1, "{l:#?}");
    l.into_iter().next().unwrap()
}

// ------------------------------------------------------------------ defaults

#[test]
fn empty_config_is_valid_and_capture_only() {
    let l = load("cfg-empty", "", None).unwrap();
    let c = &l.config;
    assert_eq!(c.server.name, "myserver");
    assert_eq!(c.server.timezone.as_deref(), Some("Etc/UTC"));
    assert_eq!(c.state.dir, "/var/lib/moi");
    assert_eq!(c.state.history_keep, None);
    assert_eq!(c.capture.data_roots, AutoOr::Auto);
    assert_eq!(c.capture.tree_depth, 4);
    assert_eq!(c.capture.max_file.bytes(), 1024 * 1024);
    assert!(c.context.claude_pointer);
    // no checks, no backups, no monitors, nothing managed
    assert!(!c.checks.resources.common.enabled);
    assert!(!c.checks.storage.common.enabled);
    assert!(!c.checks.services.common.enabled);
    assert!(!c.checks.edge.enabled);
    assert!(c.checks.custom.is_empty());
    assert!(c.heal.is_empty());
    assert!(!c.backup.enabled && !c.backup.offsite.enabled && !c.backup.offsite.r2_guard.enabled);
    assert!(!c.monitors.kuma.enabled);
    assert!(!c.firewall.manage);
    assert_eq!(c.monitors.mode, MonitorMode::Live);
    assert!(l.warnings.is_empty());
    assert_eq!(l.secrets_resolved, 0);
}

#[test]
fn a_section_is_enabled_by_being_present_and_gets_documented_defaults() {
    let l = load(
        "cfg-present",
        r#"
[checks.resources]
[checks.storage]
interval = "10m"
journal_tag = "storage-check"
[checks.services]
enabled = false
[backup]
local_dir = "/srv/backups"
[monitors.kuma]
url = "http://kuma.lan:3001"
"#,
        None,
    )
    .unwrap();
    let c = &l.config;
    let r = &c.checks.resources;
    assert!(r.common.enabled);
    assert_eq!(r.common.interval.as_duration(), Duration::from_secs(300));
    assert_eq!(r.common.timeout.as_duration(), Duration::from_secs(60));
    assert_eq!(r.common.journal_tag, "moi-check-resources");
    assert_eq!(r.common.monitor, None);
    assert_eq!((r.cpu_limit, r.mem_limit, r.swap_limit, r.temp_limit, r.sustain_runs), (90, 90, 80, 85, 2));
    assert_eq!(r.oom_window.as_duration(), Duration::from_secs(360));
    assert!(r.container_restarts);

    let s = &c.checks.storage;
    assert_eq!(s.common.interval.as_duration(), Duration::from_secs(600));
    assert_eq!(s.common.journal_tag, "storage-check");
    assert_eq!(s.mounts, ["/"]);
    assert_eq!((s.disk_limit, s.inode_limit, s.nvme_wear_limit, s.allowed_bad_sectors), (85, 85, 90, 0));
    assert_eq!(s.disks, AutoOr::Auto);

    assert!(!c.checks.services.common.enabled);

    let b = &c.backup;
    assert!(b.enabled);
    assert_eq!(b.schedule, "03:30");
    assert_eq!(b.keep_local.as_duration(), Duration::from_secs(7 * 86_400));
    assert_eq!(b.max_local.bytes(), 20 * 1024 * 1024 * 1024);
    assert_eq!(b.databases, AutoOr::Auto);
    assert_eq!(b.exclude, ["*/cache", "*/Cache", "*/transcodes", "*/logs", "*/log"]);
    assert!(!b.offsite.enabled);
    assert_eq!((b.offsite.keep.daily, b.offsite.keep.weekly, b.offsite.keep.monthly), (7, 4, 6));

    let k = &c.monitors.kuma;
    assert!(k.enabled && k.containers && k.freshness);
    assert_eq!(k.docker_host, "local");
}

#[test]
fn default_interval_applies_to_checks_without_their_own() {
    let l = load(
        "cfg-interval",
        "[checks]\ndefault_interval = \"1m\"\n[checks.resources]\n[[checks.custom]]\nname = \"x\"\nrun = \"/usr/local/bin/x\"\n",
        None,
    )
    .unwrap();
    assert_eq!(l.config.checks.resources.common.interval.as_duration(), Duration::from_secs(60));
    let custom = &l.config.checks.custom[0];
    assert_eq!(custom.common.interval.as_duration(), Duration::from_secs(60));
    assert_eq!(custom.common.journal_tag, "moi-check-x");
    assert!(custom.common.enabled);
}

#[test]
fn heal_rules_parse_into_typed_triggers_and_actions() {
    let l = load(
        "cfg-heal",
        r#"
[[heal]]
name = "traefik-homelab"
when = { docker_event = { container = "dokploy-traefik", action = "start" } }
check = { container_on_network = { container = "dokploy-traefik", network = "homelab" } }
action = { network_connect = { container = "dokploy-traefik", network = "homelab" } }
report = "[reconnected]"

[[heal]]
name = "guard"
when = { path_changed = "/etc/dokploy/traefik/traefik.yml" }
action = { run = "/opt/scripts/guard.py" }
report_stdout = true

[[heal]]
name = "vpn-stack"
when = { failing = ["gluetun", "qbittorrent"] }
action = { restart = ["gluetun"], wait_healthy = { container = "gluetun", timeout = "2m" }, then_restart = ["qbittorrent"] }
cooldown = "30m"
"#,
        None,
    )
    .unwrap();
    let h = &l.config.heal;
    assert_eq!(h.len(), 3);
    assert!(
        matches!(&h[0].when, When::DockerEvent { container, action } if container == "dokploy-traefik" && action == "start")
    );
    assert!(matches!(&h[0].action, Action::NetworkConnect { network_connect } if network_connect.network == "homelab"));
    assert!(h[0].check.is_some());
    assert!(matches!(&h[1].when, When::PathChanged(p) if p == "/etc/dokploy/traefik/traefik.yml"));
    assert!(matches!(&h[1].action, Action::Run { run } if run == "/opt/scripts/guard.py"));
    assert!(h[1].report_stdout);
    assert!(matches!(&h[2].when, When::Failing(names) if names == &["gluetun", "qbittorrent"]));
    match &h[2].action {
        Action::Restart { restart, wait_healthy, then_restart } => {
            assert_eq!(restart, &["gluetun"]);
            assert_eq!(wait_healthy.as_ref().unwrap().timeout.as_duration(), Duration::from_secs(120));
            assert_eq!(then_restart, &["qbittorrent"]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(h[2].cooldown.unwrap().as_duration(), Duration::from_secs(1800));
}

// ------------------------------------------------------------------ validation: one error each, with location

#[test]
fn unknown_key_is_an_error_with_location_and_suggestion() {
    assert_eq!(
        only("cfg-unknown-key", "[checks.resources]\ncpu_limt = 80\n"),
        "/etc/moi/moi.toml:2:1: checks.resources.cpu_limt: unknown key (did you mean `cpu_limit`?)"
    );
}

#[test]
fn unknown_section_is_an_error() {
    assert_eq!(
        only("cfg-unknown-section", "[chekcs.resources]\n"),
        "/etc/moi/moi.toml:1:2: chekcs: unknown key (did you mean `checks`?)"
    );
}

#[test]
fn wrong_type_is_an_error_with_location() {
    assert_eq!(
        only("cfg-wrong-type", "[checks.resources]\ncpu_limit = \"ninety\"\n"),
        "/etc/moi/moi.toml:2:13: checks.resources.cpu_limit: expected an integer, found string"
    );
    assert_eq!(
        only("cfg-wrong-type-2", "[context]\ncopies = \"/home/me/CONTEXT.md\"\n"),
        "/etc/moi/moi.toml:2:10: context.copies: expected a list of strings, found string"
    );
    assert_eq!(
        only("cfg-wrong-type-3", "[data]\nfungible = [\"/srv/media\", 7]\n"),
        "/etc/moi/moi.toml:2:27: data.fungible[1]: expected a string, found integer"
    );
}

#[test]
fn bad_duration_and_size_are_errors() {
    assert_eq!(
        only("cfg-bad-duration", "[checks]\ndefault_interval = \"5 minutes\"\n"),
        "/etc/moi/moi.toml:2:20: checks.default_interval: invalid duration \"5 minutes\": use a number followed by s, m, h or d (e.g. \"30s\", \"5m\", \"20h\", \"7d\")"
    );
    assert_eq!(
        only("cfg-bad-size", "[capture]\nmax_file = \"1 meg\"\n"),
        "/etc/moi/moi.toml:2:12: capture.max_file: invalid size \"1 meg\": use a number followed by B, KB, MB, GB or TB (e.g. \"512KB\", \"20GB\")"
    );
}

#[test]
fn missing_secret_names_the_key_and_where_to_define_it() {
    assert_eq!(
        only("cfg-missing-secret", "[monitors.kuma]\nurl = \"http://kuma.lan\"\npassword = \"secret:kuma_password\"\n"),
        "/etc/moi/moi.toml:3:12: monitors.kuma.password: secret `kuma_password` is not defined: add `kuma_password = \"…\"` under [secrets] in /etc/moi/secrets.toml or set MOI_SECRET_KUMA_PASSWORD"
    );
}

#[test]
fn out_of_range_and_relative_paths_and_bad_enums_are_errors() {
    assert_eq!(
        only("cfg-range", "[checks.resources]\nmem_limit = 140\n"),
        "/etc/moi/moi.toml:2:13: checks.resources.mem_limit: must be between 1 and 100"
    );
    assert_eq!(
        only("cfg-relpath", "[state]\ndir = \"var/lib/moi\"\n"),
        "/etc/moi/moi.toml:2:7: state.dir: must be an absolute path"
    );
    assert_eq!(
        only("cfg-mode", "[monitors]\nmode = \"quiet\"\n"),
        "/etc/moi/moi.toml:2:8: monitors.mode: must be \"live\" or \"shadow\""
    );
    assert_eq!(
        only("cfg-schedule", "[backup]\nlocal_dir = \"/b\"\nschedule = \"half past three\"\n"),
        "/etc/moi/moi.toml:3:12: backup.schedule: not a valid schedule: use systemd OnCalendar syntax (e.g. \"03:30\", \"Sun 04:00\", \"*-*-* 03:30:00\")"
    );
    assert_eq!(
        only("cfg-url", "[monitors.kuma]\nurl = \"kuma.lan\"\n"),
        "/etc/moi/moi.toml:2:7: monitors.kuma.url: must start with http:// or https://"
    );
}

#[test]
fn required_keys_are_checked_when_the_section_is_enabled() {
    assert_eq!(
        only("cfg-req-kuma", "[monitors.kuma]\n"),
        "/etc/moi/moi.toml:1:1: monitors.kuma.url: missing required key"
    );
    assert_eq!(only("cfg-req-backup", "[backup]\n"), "/etc/moi/moi.toml:1:1: backup.local_dir: missing required key");
    // disabled sections may stay incomplete
    assert!(load("cfg-req-disabled", "[monitors.kuma]\nenabled = false\n[backup]\nenabled = false\n", None).is_ok());
    let l = lines(load("cfg-req-custom", "[[checks.custom]]\ninterval = \"1m\"\n", None));
    assert_eq!(l.len(), 2, "{l:#?}");
    assert!(l[0].ends_with("checks.custom[0].name: missing required key"), "{l:#?}");
    assert!(l[1].ends_with("checks.custom[0].run: missing required key"), "{l:#?}");
}

#[test]
fn credentials_must_be_secret_references() {
    assert_eq!(
        only("cfg-literal-password", "[monitors.kuma]\nurl = \"http://kuma.lan\"\npassword = \"hunter2\"\n"),
        "/etc/moi/moi.toml:3:12: monitors.kuma.password: must be a \"secret:NAME\" reference (secrets never live in moi.toml)"
    );
    // a literal value never appears in the error itself
    let l = lines(load(
        "cfg-literal-env",
        "[backup]\nlocal_dir = \"/b\"\n[backup.offsite]\nrepository = \"/srv/restic\"\npassword = \"secret:p\"\nenv = { AWS_SECRET_ACCESS_KEY = \"FAKEliteral\", AWS_REGION = \"auto\" }\n",
        Some("[secrets]\np = \"FAKEpw\"\n"),
    ));
    assert_eq!(l.len(), 1, "{l:#?}");
    assert!(l[0].contains("backup.offsite.env.AWS_SECRET_ACCESS_KEY: must be a \"secret:NAME\" reference"), "{l:#?}");
    assert!(!l[0].contains("FAKEliteral"));
}

#[test]
fn duplicate_names_and_monitors_are_errors() {
    let l = lines(load(
        "cfg-dups",
        r#"
[checks.resources]
monitor = "Server"
[checks.storage]
monitor = "Server"
[[checks.custom]]
name = "resources"
run = "/bin/a"
[[checks.custom]]
name = "a"
run = "/bin/a"
[[checks.custom]]
name = "a"
run = "/bin/b"
"#,
        None,
    ));
    assert_eq!(
        l,
        [
            "/etc/moi/moi.toml:5:11: checks.storage.monitor: monitor \"Server\" is already used by checks.resources.monitor",
            "/etc/moi/moi.toml:7:8: checks.custom[0].name: \"resources\" is the name of a built-in check",
            "/etc/moi/moi.toml:13:8: checks.custom[2].name: duplicate name \"a\" (also checks.custom[1])",
        ]
    );
}

#[test]
fn fungible_reason_must_refer_to_a_fungible_path() {
    assert_eq!(
        only(
            "cfg-fungible-reason",
            "[data]\nfungible = [\"/srv/media\"]\nfungible_reason = { \"/srv/medai\" = \"typo\" }\n"
        ),
        "/etc/moi/moi.toml:3:21: data.fungible_reason.\"/srv/medai\": not listed in data.fungible"
    );
}

#[test]
fn heal_rules_need_exactly_one_trigger_and_one_action() {
    let l = lines(load(
        "cfg-heal-bad",
        "[[heal]]\nname = \"x\"\nwhen = { failing = [\"a\"], path_changed = \"/etc/x\" }\naction = { then_restart = [\"a\"] }\n",
        None,
    ));
    assert_eq!(l.len(), 2, "{l:#?}");
    assert!(l[0].ends_with("heal[0].when: exactly one of docker_event, path_changed, failing is required"), "{l:#?}");
    assert!(
        l[1].ends_with("heal[0].action: exactly one of network_connect, run, compose_up, restart is required"),
        "{l:#?}"
    );
}

#[test]
fn toml_syntax_errors_have_locations() {
    let l = lines(load("cfg-syntax", "[server]\nname = \n", None));
    assert_eq!(l.len(), 1, "{l:#?}");
    assert!(l[0].starts_with("/etc/moi/moi.toml:2:"), "{l:#?}");
}

#[test]
fn missing_config_file_says_what_to_do() {
    let host = common::minimal();
    let e = config::load(&host, &Paths::new(PathBuf::from("/etc/moi/nope.toml"), None)).unwrap_err();
    assert_eq!(e.exit_code(), 2);
    assert_eq!(e.to_string(), "/etc/moi/nope.toml: not found: run `moi init` to create it, or pass --config");
}

// ------------------------------------------------------------------ all errors in one run

#[test]
fn all_errors_are_reported_in_one_run_in_file_order() {
    let l = lines(load(
        "cfg-all-errors",
        r#"[server]
nmae = "typo"

[checks]
default_interval = "5 minutes"

[checks.resources]
cpu_limit = "ninety"
swap_limit = 0

[checks.storage]
mounts = ["relative/path"]

[backup]
max_local = "lots"

[backup.offsite]
password = "secret:restic_password"

[monitors.kuma]
url = "http://kuma.lan"
password = "secret:kuma_password"
bogus = true
"#,
        Some("[secrets]\nkuma_password = \"FAKEpw\"\n"),
    ));
    assert_eq!(
        l,
        [
            "/etc/moi/moi.toml:2:1: server.nmae: unknown key (did you mean `name`?)",
            "/etc/moi/moi.toml:5:20: checks.default_interval: invalid duration \"5 minutes\": use a number followed by s, m, h or d (e.g. \"30s\", \"5m\", \"20h\", \"7d\")",
            "/etc/moi/moi.toml:8:13: checks.resources.cpu_limit: expected an integer, found string",
            "/etc/moi/moi.toml:9:14: checks.resources.swap_limit: must be between 1 and 100",
            "/etc/moi/moi.toml:12:11: checks.storage.mounts[0]: must be an absolute path",
            "/etc/moi/moi.toml:14:1: backup.local_dir: missing required key",
            "/etc/moi/moi.toml:15:13: backup.max_local: invalid size \"lots\": use a number followed by B, KB, MB, GB or TB (e.g. \"512KB\", \"20GB\")",
            "/etc/moi/moi.toml:17:1: backup.offsite.repository: missing required key",
            "/etc/moi/moi.toml:18:12: backup.offsite.password: secret `restic_password` is not defined: add `restic_password = \"…\"` under [secrets] in /etc/moi/secrets.toml or set MOI_SECRET_RESTIC_PASSWORD",
            "/etc/moi/moi.toml:23:1: monitors.kuma.bogus: unknown key",
        ]
    );
}

#[test]
fn errors_in_both_files_are_reported_together() {
    let l = lines(load("cfg-both-files", "[server]\nnmae = 1\n", Some("[secrets]\ntoken = 5\n[other]\n")));
    assert_eq!(
        l,
        [
            "/etc/moi/moi.toml:2:1: server.nmae: unknown key (did you mean `name`?)",
            "/etc/moi/secrets.toml:2:9: secrets.token: expected a string, found integer",
            "/etc/moi/secrets.toml:3:2: other: unknown key (secrets go under [secrets])",
        ]
    );
}

// ------------------------------------------------------------------ secrets

const KUMA: &str = "[monitors.kuma]\nurl = \"http://kuma.lan\"\nusername = \"secret:kuma_username\"\npassword = \"secret:kuma_password\"\n";

#[test]
fn secrets_resolve_from_the_secrets_file() {
    let l = load(
        "cfg-secret-file",
        KUMA,
        Some("[secrets]\nkuma_username = \"FAKEadmin\"\nkuma_password = \"FAKEkuma-pw\"\n"),
    )
    .unwrap();
    let k = &l.config.monitors.kuma;
    assert_eq!(k.username.as_ref().unwrap().expose(), "FAKEadmin");
    assert_eq!(k.password.as_ref().unwrap().expose(), "FAKEkuma-pw");
    assert_eq!(l.secrets_resolved, 2);
}

#[test]
fn secrets_resolve_from_the_environment_when_absent_from_the_file() {
    let host = common::minimal_rw("cfg-secret-env");
    common::write_config(&host, KUMA, Some("[secrets]\nkuma_username = \"FAKEfrom-file\"\n"));
    host.set_env("MOI_SECRET_KUMA_USERNAME", "FAKEfrom-env");
    host.set_env("MOI_SECRET_KUMA_PASSWORD", "FAKEenv-pw");
    let l = config::load(host.as_ref(), &paths()).unwrap();
    let k = &l.config.monitors.kuma;
    assert_eq!(k.username.as_ref().unwrap().expose(), "FAKEfrom-file", "the file wins over the environment");
    assert_eq!(k.password.as_ref().unwrap().expose(), "FAKEenv-pw");
}

#[test]
fn secrets_work_without_a_secrets_file_at_all() {
    let host = common::minimal_rw("cfg-secret-nofile");
    common::write_config(&host, KUMA, None);
    host.set_env("MOI_SECRET_KUMA_USERNAME", "FAKEu");
    host.set_env("MOI_SECRET_KUMA_PASSWORD", "FAKEp");
    assert!(config::load(host.as_ref(), &paths()).is_ok());
}

#[test]
fn any_string_value_may_be_a_secret_reference() {
    let l = load(
        "cfg-secret-anywhere",
        "[server]\nname = \"secret:server_name\"\n[checks.edge]\npublic_urls = [\"secret:private_url\"]\n",
        Some("[secrets]\nserver_name = \"FAKEname\"\nprivate_url = \"https://FAKEprivate.example.com/\"\n"),
    )
    .unwrap();
    assert_eq!(l.config.server.name, "FAKEname");
    assert_eq!(l.config.checks.edge.public_urls, ["https://FAKEprivate.example.com/"]);
    let shown = l.effective_toml();
    assert!(!shown.contains("FAKE"), "{shown}");
}

#[test]
fn secrets_file_readable_by_others_is_refused() {
    let host = common::minimal_rw("cfg-secret-perms");
    common::write_config(&host, KUMA, Some("[secrets]\nkuma_username = \"FAKEu\"\nkuma_password = \"FAKEp\"\n"));
    host.set_mode(Path::new("/etc/moi/secrets.toml"), 0o644).unwrap();
    let l = lines(config::load(host.as_ref(), &paths()));
    assert_eq!(
        l,
        ["/etc/moi/secrets.toml: mode 0644 lets other users read the secrets: run `chmod 600 /etc/moi/secrets.toml`"]
    );
}

#[test]
fn secrets_path_defaults_to_the_config_files_sibling_and_can_be_overridden() {
    assert_eq!(Paths::new("/srv/x/moi.toml".into(), None).secrets, Path::new("/srv/x/secrets.toml"));
    assert_eq!(Paths::new("/srv/x/moi.toml".into(), Some("/run/s.toml".into())).secrets, Path::new("/run/s.toml"));
}

// ------------------------------------------------------------------ masking

fn full(host: &FixtureHost) -> Loaded {
    config::load(host, &Paths::new("/cases/full/moi.toml".into(), None)).unwrap()
}

#[test]
fn effective_config_masks_every_resolved_secret() {
    let host = common::minimal();
    let l = full(&host);
    assert_eq!(l.secrets_resolved, 7);
    let secrets_file = host.read_to_string(Path::new("/cases/full/secrets.toml")).unwrap();
    let toml = l.effective_toml();
    let json = l.effective_json().to_string();
    for line in secrets_file.lines().filter(|l| l.contains(" = ")) {
        let value = line.split_once(" = ").unwrap().1.trim_matches('"');
        assert!(!toml.contains(value), "{value} leaked into:\n{toml}");
        assert!(!json.contains(value), "{value} leaked into:\n{json}");
    }
    assert!(!toml.contains("FAKE") && !json.contains("FAKE"));
    assert!(toml.contains("password = \"***\""), "{toml}");
    // non-secret values stay readable
    assert!(toml.contains("url = \"http://kuma.lan:3001\""), "{toml}");
}

#[test]
fn debug_output_never_contains_secret_values() {
    let host = common::minimal();
    let l = full(&host);
    let dbg = format!("{l:?} {:?} {:#?}", l.config, l.config.backup.offsite);
    assert!(!dbg.contains("FAKE"), "{dbg}");
}

#[test]
fn monitor_without_kuma_is_a_warning_not_an_error() {
    let l = load("cfg-warning", "[checks.resources]\nmonitor = \"Server Resources\"\n", None).unwrap();
    let w: Vec<String> = l.warnings.iter().map(ToString::to_string).collect();
    assert_eq!(
        w,
        [
            "/etc/moi/moi.toml:2:11: checks.resources.monitor: no [monitors.kuma] section, so nothing will be pushed to this monitor"
        ]
    );
}

// ------------------------------------------------------------------ units

#[test]
fn durations_and_sizes_parse_and_print_canonically() {
    use cestmoi::config::{Dur, Size};
    for (s, secs) in [("30s", 30), ("5m", 300), ("1h", 3600), ("20h", 72_000), ("7d", 604_800), ("0s", 0), ("90s", 90)]
    {
        let d: Dur = s.parse().unwrap();
        assert_eq!(d.as_duration(), Duration::from_secs(secs), "{s}");
        assert_eq!(d.to_string(), s);
    }
    assert_eq!("120s".parse::<Dur>().unwrap().to_string(), "2m");
    for bad in ["", "5", "m", "5 m", "-5m", "5min", "1.5h", "5M"] {
        assert!(bad.parse::<Dur>().is_err(), "{bad}");
    }
    for (s, bytes) in [("512KB", 512u64 << 10), ("1MB", 1 << 20), ("20GB", 20 << 30), ("2TB", 2 << 40), ("100B", 100)] {
        let z: Size = s.parse().unwrap();
        assert_eq!(z.bytes(), bytes, "{s}");
        assert_eq!(z.to_string(), s);
    }
    assert_eq!("1024KB".parse::<Size>().unwrap().to_string(), "1MB");
    for bad in ["", "20", "GB", "20 GB", "20gb", "1.5GB", "20G", "99999999999TB"] {
        assert!(bad.parse::<Size>().is_err(), "{bad}");
    }
}

#[test]
fn context_share_needs_its_bucket_credentials_as_secret_references() {
    let l = lines(load("cfg-req-share", "[context.share]\n", None));
    assert_eq!(l.len(), 3, "{l:#?}");
    assert!(l[0].ends_with("context.share.endpoint: missing required key"), "{l:#?}");
    assert!(l[1].ends_with("context.share.access_key_id: missing required key"), "{l:#?}");
    assert!(l[2].ends_with("context.share.secret_access_key: missing required key"), "{l:#?}");
    assert!(load("cfg-share-off", "[context.share]\nenabled = false\n", None).is_ok());
    let literal = only(
        "cfg-share-literal",
        "[context.share]\nendpoint = \"https://x.r2.cloudflarestorage.com\"\naccess_key_id = \"id\"\nsecret_access_key = \"plain\"\n",
    );
    assert!(
        literal.ends_with(
            "context.share.secret_access_key: must be a \"secret:NAME\" reference (secrets never live in moi.toml)"
        ),
        "{literal}"
    );
    assert!(!literal.contains("plain"));
}

#[test]
fn context_share_checks_its_endpoint_and_ttls() {
    let secrets = "[secrets]\nep = \"r2.example.com\"\nkey = \"FAKEshare-secret\"\n";
    let l = lines(load(
        "cfg-share-values",
        "[context.share]\nendpoint = \"secret:ep\"\naccess_key_id = \"id\"\nsecret_access_key = \"secret:key\"\ndefault_ttl = \"2d\"\n",
        Some(secrets),
    ));
    assert_eq!(
        l,
        [
            "/etc/moi/moi.toml:2:12: context.share.endpoint: must start with http:// or https://",
            "/etc/moi/moi.toml:5:15: context.share.default_ttl: is longer than max_ttl (1d)",
        ]
    );
}
