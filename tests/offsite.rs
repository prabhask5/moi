//! The off-site step of a backup, the state bundle, `moi push`, `moi pull`, `moi recovery-card`
//! (docs/spec/07-backup.md § Run 3, docs/spec/08-bundle.md).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cestmoi::cli;
use cestmoi::host::{FixtureHost, Host, parse_rfc3339};
use cestmoi::state::StateDir;

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
}

fn server_with(test: &str, net: &str) -> Server {
    let overlay = common::scratch(test);
    let mut fixture = FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap();
    if !net.is_empty() {
        let case = common::scratch(&format!("{test}-case"));
        std::fs::write(case.join("net"), net).unwrap();
        fixture = fixture.with_case(&case).unwrap();
    }
    let fixture = Arc::new(fixture.with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture }
}

fn server(test: &str) -> Server {
    server_with(test, "")
}

const GRAPHQL: &str = "http api.cloudflare.com/client/v4/graphql";
const UPLOAD: &str = "restic backup --quiet --tag nightly --pack-size 64 --exclude-file /var/lib/moi/staging/restic-excludes --exclude-caches ";

impl Server {
    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), e.to_string()),
        }
    }

    /// The off-site lines of a backup's log, without their timestamp and prefix.
    fn offsite(&self) -> (u8, Vec<String>) {
        let (code, out) = self.moi(&["backup"]);
        let lines = out
            .lines()
            .filter_map(|l| l.split_once("] ").map(|(_, rest)| rest))
            .filter(|l| l.starts_with("offsite: ") || l.starts_with("ERROR") || l.starts_with("Backup "))
            .map(|l| l.trim_start_matches("offsite: ").to_string())
            .collect();
        (code, lines)
    }

    fn restic(&self) -> Vec<String> {
        self.fixture.commands_run().into_iter().filter(|c| c.starts_with("restic ")).collect()
    }

    fn edit_config(&self, edit: impl FnOnce(String) -> String) {
        let path = Path::new("/etc/moi/moi.toml");
        let text = edit(self.host.read_to_string(path).unwrap());
        self.host.write(path, text.as_bytes(), 0o644).unwrap();
    }
}

const RUN: &str = "/mnt/storage/backups/2026-11-26_1200";

#[test]
fn a_whole_local_backup_goes_off_site_behind_every_guard() {
    let s = server("offsite-ok");
    let (code, lines) = s.offsite();
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines,
        [
            // PutObject + ListObjectsV2 are Class A; Get/Head Class B; deletes are free. Storage: per bucket
            // the largest size seen, summed (2 GB + 1 MB, and 1 GB).
            "R2 usage, last 30 days: Class A 1500/500000, Class B 9800/5000000, storage 3/8 GB",
            "uploading 15 paths (attempt 1)",
            "repository size: 2048 MB (limit 8 GB)",
            "state bundle saved",
            "done",
            &format!("Backup OK: {RUN}"),
        ]
    );
    // restic, in the script's order; no prune on a Thursday
    let restic = s.restic();
    assert_eq!(restic[0], "restic version");
    assert_eq!(restic[1], "restic unlock --quiet");
    assert!(restic[2].starts_with(&format!("{UPLOAD}{RUN}/db /etc/docker /etc/dokploy /etc/moi ")), "{}", restic[2]);
    assert!(
        restic[2].ends_with(" /var/lib/moi"),
        "the state directory, with the consistent copies of live databases in it: {}",
        restic[2]
    );
    assert!(!restic[2].contains("/mnt/storage/media") && !restic[2].contains("configs.tar.gz"));
    assert_eq!(
        restic[3..],
        [
            "restic forget --quiet --tag nightly --keep-daily 7 --keep-weekly 4 --keep-monthly 6",
            "restic stats --quiet --mode raw-data --json",
            "restic backup --quiet --tag moi-bundle --host cestmoi /var/lib/moi/state /etc/moi",
            "restic forget --quiet --tag moi-bundle --host cestmoi --keep-last 30 --keep-daily 30",
        ]
    );
    // the repository, its password and the keys are in restic's environment and nowhere else
    let env = s.fixture.environment_of("restic backup --quiet --tag nightly");
    let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["RESTIC_REPOSITORY", "RESTIC_PASSWORD", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]);
    assert_eq!(env[1].1, "FAKE-restic_password-value");
    let (_, out) = s.moi(&["backup", "--dry-run"]);
    for secret in [
        "FAKE-restic_password-value",
        "FAKE-restic_repository-value",
        "FAKE-r2_secret_access_key-value",
        "FAKE-cf_api_token-value",
    ] {
        assert!(
            !s.restic().iter().any(|c| c.contains(secret)) && !out.contains(secret),
            "{secret} on a command line or in output"
        );
    }
    assert!(!s.host.read_to_string(Path::new("/var/log/server-backup.log")).unwrap().contains("FAKE-"));
}

#[test]
fn what_restic_must_skip_is_written_for_it() {
    let s = server("offsite-excludes");
    // read the exclude file at the moment restic would (the run removes its staging directory afterwards)
    let seen: Arc<std::sync::Mutex<String>> = Arc::default();
    let (host, keep) = (s.host.clone(), seen.clone());
    s.fixture.on_command(
        "restic backup --quiet --tag nightly ...",
        Arc::new(move || {
            *keep.lock().unwrap() =
                host.read_to_string(Path::new("/var/lib/moi/staging/restic-excludes")).unwrap_or_default();
        }),
    );
    assert_eq!(s.offsite().0, 0);
    let excludes = seen.lock().unwrap().clone();
    let has = |line: &str| excludes.lines().any(|l| l == line);
    // patterns from the configuration, raw database files, the fungible, the tarballs
    for line in [
        "*/cache",
        "*/transcodes",
        "/etc/dokploy/logs",
        "/opt/services/outline/pgdata",
        "/mnt/storage/database/db-data",
        "/mnt/storage/media",
        "/var/lib/incus",
        "/mnt/storage/backups/*/configs.tar.gz",
    ] {
        assert!(has(line), "{line} should be excluded:\n{excludes}");
    }
    // live SQLite files: their consistent copies in staging are uploaded instead
    for line in [
        "/opt/services/monitoring/uptime-kuma/data/kuma.db",
        "/opt/services/monitoring/uptime-kuma/data/kuma.db-wal",
        "/var/lib/moi/moi.db",
    ] {
        assert!(has(line), "{line} should be excluded:\n{excludes}");
    }
    assert!(!s.host.exists(Path::new("/var/lib/moi/staging")), "staging is removed when the run is over");
}

#[test]
fn at_most_one_upload_per_interval() {
    let s = server("offsite-rate-limit");
    assert_eq!(s.offsite().0, 0);
    let uploads = |s: &Server| s.restic().iter().filter(|c| c.starts_with(UPLOAD)).count();
    assert_eq!(uploads(&s), 1);
    // a second run nineteen hours later: the local backup is taken, the upload is skipped — and that is a success
    s.fixture.advance(Duration::from_secs(19 * 3600));
    let (code, lines) = s.offsite();
    assert_eq!(code, 0);
    assert_eq!(lines[0], "skipped: last upload was 19h ago (minimum 20h)");
    assert_eq!(uploads(&s), 1);
    // twenty hours after the first one it uploads again
    s.fixture.advance(Duration::from_secs(3600));
    assert_eq!(s.offsite().1[1], "uploading 15 paths (attempt 1)");
    assert_eq!(uploads(&s), 2);
}

#[test]
fn nothing_is_uploaded_when_the_account_is_past_its_limits_or_cannot_be_asked() {
    let usage = |ops: &str, storage: u64| {
        format!(
            r#"{GRAPHQL} 200 {{"data":{{"viewer":{{"accounts":[{{"ops":[{ops}],"sto":[{{"max":{{"payloadSize":{storage},"metadataSize":0}},"dimensions":{{"bucketName":"b"}}}}]}}]}}}},"errors":null}}"#
        )
    };
    let op = |action: &str, n: u64| format!(r#"{{"sum":{{"requests":{n}}},"dimensions":{{"actionType":"{action}"}}}}"#);
    let refused = "ERROR: R2 usage past the safety limit, refusing to upload (check the R2 dashboard)";
    let cases: Vec<(&str, String, String)> = vec![
        (
            "class-a",
            usage(&op("PutObject", 500_000), 0),
            "R2 usage, last 30 days: Class A 500000/500000, Class B 0/5000000, storage 0/8 GB".into(),
        ),
        (
            "class-b",
            usage(&op("GetObject", 5_000_000), 0),
            "R2 usage, last 30 days: Class A 0/500000, Class B 5000000/5000000, storage 0/8 GB".into(),
        ),
        (
            "storage",
            usage(&op("DeleteObject", 9_000_000), 8 << 30),
            "R2 usage, last 30 days: Class A 0/500000, Class B 0/5000000, storage 8/8 GB".into(),
        ),
    ];
    for (name, net, usage_line) in cases {
        let s = server_with(&format!("offsite-r2-{name}"), &net);
        let (code, lines) = s.offsite();
        assert_eq!(code, 1, "{name}");
        assert_eq!(
            lines,
            [
                usage_line.as_str(),
                refused,
                "ERROR: off-site backup failed",
                &format!("Backup finished WITH ERRORS: {RUN}")
            ],
            "{name}"
        );
        assert!(!s.restic().iter().any(|c| c.starts_with("restic backup")), "{name}: nothing may be uploaded");
    }
    // usage cannot be read: fail closed
    for (name, net, error) in [
        (
            "down",
            "tcp 198.51.100.20:443 refused".to_string(),
            "ERROR: cannot read R2 usage from Cloudflare (refusing to upload)".to_string(),
        ),
        (
            "500",
            format!("{GRAPHQL} 500 oops"),
            "ERROR: cannot read R2 usage from Cloudflare (refusing to upload)".to_string(),
        ),
        (
            "not-json",
            format!("{GRAPHQL} 200 <html>"),
            "ERROR: cannot read R2 usage from Cloudflare (refusing to upload)".to_string(),
        ),
        (
            "errors",
            format!(r#"{GRAPHQL} 200 {{"data":null,"errors":[{{"message":"not authorized for that account"}}]}}"#),
            r#"ERROR: R2 usage query failed (refusing to upload): [{"message":"not authorized for that account"}]"#
                .to_string(),
        ),
        (
            "no-account",
            format!(r#"{GRAPHQL} 200 {{"data":{{"viewer":{{"accounts":[]}}}},"errors":null}}"#),
            "ERROR: R2 usage query failed (refusing to upload): null".to_string(),
        ),
    ] {
        let s = server_with(&format!("offsite-r2-{name}"), &net);
        let (code, lines) = s.offsite();
        assert_eq!((code, lines[0].as_str()), (1, error.as_str()), "{name}");
        assert!(!s.restic().iter().any(|c| c.starts_with("restic backup")), "{name}: nothing may be uploaded");
    }
    // the monitor is told the run failed
    let s = server_with("offsite-r2-push", &format!("{GRAPHQL} 500 oops"));
    assert_eq!(s.moi(&["backup", "--json"]).0, 1);
    let db = StateDir::new("/var/lib/moi").open_db(s.host.as_ref()).unwrap();
    assert_eq!(
        db.conn().query_row("SELECT message FROM backup_runs", [], |r| r.get::<_, String>(0)).unwrap(),
        "ERROR: off-site backup failed"
    );
}

#[test]
fn a_failed_upload_is_tried_once_more_after_ten_minutes() {
    // fails, then works
    let s = server("offsite-retry");
    s.fixture.add_command_then("restic backup --quiet --tag nightly ...", 1, "");
    s.fixture.add_command_then("restic backup --quiet --tag nightly ...", 0, "");
    let before = s.host.now();
    let (code, lines) = s.offsite();
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines[1..4],
        [
            "uploading 15 paths (attempt 1)",
            "backup failed (1), retrying in 10 minutes",
            "uploading 15 paths (attempt 2)"
        ]
    );
    assert_eq!(s.host.now().duration_since(before).unwrap(), Duration::from_secs(600));
    // fails twice: the run has failed, and no upload is on record (the next run tries again)
    let s = server("offsite-retry-fails");
    s.fixture.add_command("restic backup --quiet --tag nightly ...", 1, "");
    let (code, lines) = s.offsite();
    assert_eq!(code, 1);
    assert_eq!(
        lines[3..],
        [
            "uploading 15 paths (attempt 2)",
            "ERROR: restic backup failed (1)",
            "ERROR: off-site backup failed",
            &format!("Backup finished WITH ERRORS: {RUN}")
        ]
    );
    assert!(!s.restic().iter().any(|c| c.starts_with("restic forget")), "no retention after a failed upload");
    s.fixture.add_command("restic backup --quiet --tag nightly ...", 0, "");
    assert_eq!(s.offsite().1[1], "uploading 15 paths (attempt 1)", "not rate-limited by an upload that failed");
}

#[test]
fn files_restic_could_not_read_are_a_warning() {
    let s = server("offsite-exit-3");
    s.fixture.add_command("restic backup --quiet --tag nightly ...", 3, "");
    let (code, lines) = s.offsite();
    assert_eq!(code, 0);
    assert_eq!(lines[2], "warning: some files could not be read (snapshot still saved)");
    assert_eq!(lines.last().unwrap(), &format!("Backup OK: {RUN}"));
}

#[test]
fn pruning_and_the_integrity_check_happen_on_the_configured_day() {
    let s = server("offsite-sunday");
    // Sunday noon in New York
    s.fixture.set_now(parse_rfc3339("2026-11-29T17:00:00Z").unwrap());
    let (code, lines) = s.offsite();
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(lines[2..4], ["retention + prune", "integrity check (re-reads 2% of the data)"]);
    let restic = s.restic();
    assert!(restic.contains(
        &"restic forget --quiet --tag nightly --keep-daily 7 --keep-weekly 4 --keep-monthly 6 --prune".to_string()
    ));
    assert!(restic.contains(&"restic check --quiet --read-data-subset=2%".to_string()));
    // each of them failing fails the run
    for (command, error) in [
        (
            "restic forget --quiet --tag nightly --keep-daily 7 --keep-weekly 4 --keep-monthly 6 --prune",
            "ERROR: restic forget/prune failed",
        ),
        ("restic check --quiet --read-data-subset=2%", "ERROR: restic check failed"),
    ] {
        let s = server(&format!("offsite-sunday-{}", error.len()));
        s.fixture.set_now(parse_rfc3339("2026-11-29T17:00:00Z").unwrap());
        s.fixture.add_command(command, 1, "");
        let (code, lines) = s.offsite();
        assert_eq!(code, 1);
        assert!(lines.contains(&error.to_string()), "{lines:?}");
    }
    // on other days a failing forget
    let s = server("offsite-forget-fails");
    s.fixture.add_command("restic forget --quiet --tag nightly --keep-daily 7 --keep-weekly 4 --keep-monthly 6", 1, "");
    assert!(s.offsite().1.contains(&"ERROR: restic forget failed".to_string()));
}

#[test]
fn a_repository_at_its_ceiling_fails_the_run() {
    let s = server("offsite-repo-size");
    s.fixture.add_command("restic stats --quiet --mode raw-data --json", 0, r#"{"total_size":8589934592}"#);
    let (code, lines) = s.offsite();
    assert_eq!(code, 1);
    assert_eq!(lines[2..4], ["repository size: 8192 MB (limit 8 GB)", "ERROR: repository near the free tier"]);
    // restic missing altogether
    let s = server("offsite-no-restic");
    s.fixture.add_command("restic version", 127, "");
    assert_eq!(s.offsite().1[0], "ERROR: restic not installed");
}

// ------------------------------------------------------------------ the state bundle

#[test]
fn push_stores_the_bundle_only_when_something_changed() {
    let s = server("bundle-push");
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    let pushes =
        |s: &Server| s.restic().iter().filter(|c| c.starts_with("restic backup --quiet --tag moi-bundle")).count();
    assert_eq!(s.moi(&["push"]), (0, "state bundle saved off-site\n".to_string()));
    assert_eq!(pushes(&s), 1);
    assert_eq!(
        s.restic().last().unwrap(),
        "restic forget --quiet --tag moi-bundle --host cestmoi --keep-last 30 --keep-daily 30"
    );
    // nothing changed: nothing is stored
    assert_eq!(s.moi(&["push"]), (0, "nothing changed since the last push (use --force to push anyway)\n".to_string()));
    assert_eq!(pushes(&s), 1);
    assert_eq!(s.moi(&["push", "--force"]).0, 0);
    assert_eq!(pushes(&s), 2);
    // the configuration changed
    s.edit_config(|t| format!("{t}\n# a note\n"));
    assert_eq!(s.moi(&["push"]).1, "state bundle saved off-site\n");
    // the recorded state changed
    s.host.write(Path::new("/etc/hosts"), b"127.0.0.1 localhost changed\n", 0o644).unwrap();
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    assert_eq!(s.moi(&["push"]).1, "state bundle saved off-site\n");
    assert_eq!(pushes(&s), 4);
    // restic refusing is an error that says why
    s.fixture.add_command("restic backup --quiet --tag moi-bundle --host cestmoi /var/lib/moi/state /etc/moi", 1, "");
    let (code, err) = s.moi(&["push", "--force"]);
    assert_eq!(code, 2);
    assert!(err.starts_with("restic backup failed (1)"), "{err}");

    // without an off-site repository there is nowhere to push to
    let s = server("bundle-push-none");
    s.edit_config(|t| {
        let start = t.find("[backup.offsite]").unwrap();
        let end = t.find("# ------------------------------------------------------------------ restore").unwrap();
        format!("{}{}", &t[..start], &t[end..])
    });
    let (code, err) = s.moi(&["push"]);
    assert_eq!(code, 2);
    assert!(err.starts_with("there is no off-site repository to push the state bundle to"), "{err}");
}

#[test]
fn the_daemon_pushes_after_a_change_at_most_once_an_hour() {
    let s = server("bundle-daemon");
    let loaded =
        cestmoi::config::load(s.host.as_ref(), &cestmoi::config::Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let mut daemon = cestmoi::daemon::Daemon::new(s.host.clone(), loaded).unwrap();
    daemon.tick().unwrap();
    let pushes =
        |s: &Server| s.restic().iter().filter(|c| c.starts_with("restic backup --quiet --tag moi-bundle")).count();
    let mut change = |n: u32| {
        s.host.write(Path::new("/etc/hosts"), format!("127.0.0.1 localhost v{n}\n").as_bytes(), 0o644).unwrap();
        daemon.file_changed(Path::new("/etc/hosts"));
        s.fixture.advance(Duration::from_secs(120));
        daemon
            .tick()
            .unwrap()
            .into_iter()
            .find(|(name, _)| name == "snapshot")
            .map(|(_, o)| o.message)
            .unwrap_or_default()
    };
    assert!(change(1).ends_with("; state bundle pushed"));
    assert_eq!(pushes(&s), 1);
    // more changes within the hour are recorded locally and wait
    for n in 2..5 {
        assert!(!change(n).contains("state bundle"));
    }
    assert_eq!(pushes(&s), 1);
    s.fixture.advance(Duration::from_secs(3600));
    assert!(change(5).ends_with("; state bundle pushed"));
    assert_eq!(pushes(&s), 2);
}

const SNAPSHOTS: &str = r#"[{"time":"2026-11-25T08:41:10.123456789Z","hostname":"cestmoi","tags":["moi-bundle"],"paths":["/etc/moi","/var/lib/moi/state"],"id":"aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111","short_id":"aaaa1111"},{"time":"2026-11-26T08:41:12.5Z","hostname":"cestmoi","tags":["moi-bundle"],"paths":["/etc/moi","/var/lib/moi/state"],"id":"bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222","short_id":"bbbb2222"}]"#;

#[test]
fn pull_restores_the_latest_bundle_and_says_what_comes_next() {
    // a new machine: nothing of moi on it but the binary
    let bare = |test: &str| {
        let case = common::scratch(&format!("{test}-case"));
        std::fs::write(case.join("absent"), "/etc/moi\n").unwrap();
        let fixture = Arc::new(
            FixtureHost::open(&common::fixture_dir("cestmoi"))
                .unwrap()
                .with_case(&case)
                .unwrap()
                .with_overlay(&common::scratch(test))
                .unwrap(),
        );
        Server { host: fixture.clone(), fixture }
    };
    let s = bare("bundle-pull");
    let dir = "/var/lib/moi/restore/2026-11-26T170000Z";
    s.fixture.add_command("restic snapshots --tag moi-bundle --json", 0, SNAPSHOTS);
    s.fixture.add_command(
        &format!("restic restore bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222 --target {dir}"),
        0,
        "",
    );
    // no repository named
    assert_eq!(
        s.moi(&["pull"]),
        (2, "which repository? `moi pull --repository <URL>` (the URL is on your recovery card)".to_string())
    );
    s.fixture.set_env("RESTIC_PASSWORD", "FAKE-typed-password");
    let (code, out) = s.moi(&["pull", "--repository", "s3:https://FAKE.r2.example.com/bucket"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        format!(
            "state bundle of cestmoi from 2026-11-26 08:41 restored to {dir}\nnext: moi restore --bundle {dir} --dry-run\n"
        )
    );
    // the newest snapshot, into a private directory; the repository goes to restic in its environment
    assert_eq!(
        s.restic(),
        [
            "restic snapshots --tag moi-bundle --json".to_string(),
            format!("restic restore bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222 --target {dir}")
        ]
    );
    assert_eq!(s.host.metadata(Path::new(dir)).unwrap().mode, 0o700);
    assert_eq!(
        s.fixture.environment_of("restic snapshots"),
        [("RESTIC_REPOSITORY".to_string(), "s3:https://FAKE.r2.example.com/bucket".to_string())]
    );

    // typed at the prompt when the environment has none
    let s2 = bare("bundle-pull-prompt");
    s2.fixture.add_command("restic snapshots --tag moi-bundle --json", 0, "[]");
    s2.fixture.answer("FAKE-typed-password");
    let (code, err) = s2.moi(&["pull", "--repository", "/srv/repo"]);
    assert_eq!(
        (code, err.as_str()),
        (2, "this repository holds no state bundle (no snapshot tagged moi-bundle): is it the right repository?")
    );
    assert_eq!(
        s2.fixture.environment_of("restic snapshots")[1],
        ("RESTIC_PASSWORD".to_string(), "FAKE-typed-password".to_string())
    );
    // no password anywhere and nobody answers: restic is not even started
    let s4 = bare("bundle-pull-silent");
    let (code, err) = s4.moi(&["pull", "--repository", "s3:https://FAKE.r2.example.com/bucket"]);
    assert_eq!(code, 2);
    assert!(
        err.starts_with("the repository's password is needed: set RESTIC_PASSWORD or RESTIC_PASSWORD_FILE"),
        "{err}"
    );
    assert_eq!(s4.restic(), Vec::<String>::new());
    // a wrong password, as restic reports it
    let s3 = server("bundle-pull-wrong");
    s3.fixture.add_command("restic snapshots --tag moi-bundle --json", 1, "");
    assert!(
        s3.moi(&["pull"]).1.starts_with("cannot read the repository: "),
        "a configured machine pulls from its own repository"
    );
}

#[test]
fn the_recovery_card_names_what_to_keep_and_contains_none_of_it() {
    let s = server("bundle-card");
    let (code, card) = s.moi(&["recovery-card"]);
    assert_eq!(code, 0, "{card}");
    assert!(card.starts_with("Recovery card for cestmoi\n"), "{card}");
    for line in [
        "[ ] restic repository URL      secret `restic_repository` in /etc/moi/secrets.toml",
        "[ ] restic repository password secret `restic_password` in /etc/moi/secrets.toml",
        "[ ] AWS_ACCESS_KEY_ID          secret `r2_access_key_id` in /etc/moi/secrets.toml",
        "[ ] AWS_SECRET_ACCESS_KEY      secret `r2_secret_access_key` in /etc/moi/secrets.toml",
        "    backups are tagged `nightly`, the state bundle `moi-bundle`, host `cestmoi`",
        "[ ] Uptime Kuma login          http://uptime.home (Kuma's own data comes back with the backup)",
        "[ ] not backed up: /mnt/storage/media — media library — temporary, re-downloadable",
    ] {
        assert!(card.contains(&format!("{line}\n")), "missing {line:?} in:\n{card}");
    }
    assert!(card.ends_with("  4. sudo moi restore --bundle /var/lib/moi/restore/<time>\n"));
    // not one secret value, in either form of the output
    let json = s.moi(&["recovery-card", "--json"]).1;
    let secrets = std::fs::read_to_string(common::fixture_dir("cestmoi").join("secrets")).unwrap();
    for secret in secrets.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        assert!(!card.contains(secret) && !json.contains(secret), "the card prints a secret: {secret}");
    }
    // a server without off-site backup is told what that means
    s.edit_config(|t| {
        let start = t.find("[backup.offsite]").unwrap();
        let end = t.find("# ------------------------------------------------------------------ restore").unwrap();
        format!("{}{}", &t[..start], &t[end..])
    });
    assert!(
        s.moi(&["recovery-card"])
            .1
            .contains("[!] No off-site backup is configured: if this machine is lost, so is everything on it.\n")
    );
}
