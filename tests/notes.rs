//! Owner notes (docs/spec/03-context.md § Owner notes) and `moi notes`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::snapshot::{View, parse_crontab};
use cestmoi::state::history::History;
use cestmoi::state::{StateDir, notes};

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    state: StateDir,
    overlay: PathBuf,
}

fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture, state: StateDir::new("/var/lib/moi"), overlay }
}

impl Server {
    /// Run a moi command in-process against the fixture; returns stdout or the error text.
    fn moi(&self, args: &[&str]) -> Result<String, String> {
        let mut out = Vec::new();
        let parsed = cli::parse(args).unwrap();
        cli::execute(&parsed, self.host.clone(), &mut out)
            .map(|()| String::from_utf8(out).unwrap())
            .map_err(|e| e.to_string())
    }

    fn notes(&self) -> String {
        self.host.read_to_string(&self.state.notes()).unwrap()
    }

    fn set_notes(&self, text: &str) {
        self.host.write(&self.state.notes(), text.as_bytes(), 0o600).unwrap();
    }
}

#[test]
fn the_first_snapshot_creates_notes_with_a_stub_for_everything_worth_describing() {
    let s = server("notes-stubs");
    s.moi(&["snapshot"]).unwrap();
    let text = s.notes();
    assert!(text.starts_with("# Owner notes — cestmoi\n\nFree-form notes that moi merges into CONTEXT.md"), "{text}");
    let headings: Vec<String> = notes::parse(&text).into_iter().map(|(h, _)| h).collect();
    assert_eq!(headings[..3], ["Overview", "Gotchas / do not touch", "Planned changes / ideas"]);
    for expected in [
        "Stack: yams",
        "Stack: old-wiki",
        "Container: dokploy-traefik",
        "Service: stellar-planner-x7k2",
        "Instance: devbox",
        "Unit: host-firewall.service",
        "Unit: server-backup.timer",
        "Script: /opt/scripts/backup.sh",
        "Script: /home/prabhas/bin/weekly-note.sh",
    ] {
        assert!(headings.contains(&expected.to_string()), "{expected} missing from {headings:#?}");
    }
    // drop-ins are part of their unit, scripts nobody schedules need no note, compose containers belong to their stack
    assert!(
        !headings
            .iter()
            .any(|h| h.ends_with(".conf") || h == "Script: /opt/scripts/README.md" || h == "Container: jellyfin")
    );
    assert!(text.contains(
        "\n## Stack: yams\n_Not described yet — what it does, its URL, who uses it, anything non-obvious_\n"
    ));
    assert_eq!(s.host.metadata(&s.state.notes()).unwrap().mode, 0o600);
    // nothing in it is a note yet
    assert_eq!(notes::read(s.host.as_ref(), &s.state), Vec::<(String, String)>::new());
    // and it is part of the first commit
    let (_, changes) = History::open(s.host.as_ref(), &s.state).unwrap().show("HEAD").unwrap();
    assert!(changes.iter().any(|c| c.path == "NOTES.md"));
}

#[test]
fn owner_text_survives_a_hundred_regenerations_byte_for_byte() {
    let s = server("notes-preserved");
    s.moi(&["snapshot"]).unwrap();
    // an owner who reorders sections, adds their own, uses odd spacing and leaves no final newline
    let mut edited = s.notes().replace(
        "## Overview\n_Not described yet — what this server is for, who uses it, how it is reached (domain, VPN, LAN IP…)_\n",
        "## Overview\nHome server in the closet.   Reached at http://home (LAN) and *.example.com.\n\n\n- AdGuard is the DHCP server: do not stop it\n",
    );
    edited.push_str("\n##   My own heading  \nfree text\twith a tab\n\n## Stack: yams\nduplicate heading, mine now");
    s.set_notes(&edited);
    for _ in 0..100 {
        assert!(!notes::update(s.host.as_ref(), &s.state).unwrap());
    }
    assert_eq!(s.notes(), edited);

    let read = notes::read(s.host.as_ref(), &s.state);
    assert_eq!(read[0].0, "Overview");
    assert!(read[0].1.starts_with("Home server in the closet.") && read[0].1.ends_with("do not stop it"));
    assert!(read.iter().any(|(h, body)| h == "My own heading" && body == "free text\twith a tab"));
    assert_eq!(read.len(), 3, "Overview, the owner's heading, and the owner's own yams section: {read:#?}");
}

#[test]
fn a_new_thing_gets_one_stub_appended_and_nothing_else_changes() {
    let s = server("notes-new-thing");
    s.moi(&["snapshot"]).unwrap();
    let before = s.notes().replace(
        "_Not described yet — what this unit does_\n\n## Unit: homelab-services.service",
        "runs at boot\n\n## Unit: homelab-services.service",
    );
    s.set_notes(&before);
    s.host
        .write(
            Path::new("/etc/systemd/system/nightly-report.service"),
            b"[Service]\nExecStart=/opt/scripts/backup.sh --report\n",
            0o644,
        )
        .unwrap();
    let out = s.moi(&["snapshot"]).unwrap();
    // the new unit shows up under systemd, and as one more caller of backup.sh in the monitoring integrations
    assert!(
        out.contains("changed: NOTES.md, monitoring, systemd, /etc/systemd/system/nightly-report.service"),
        "{out}"
    );
    let after = s.notes();
    assert_eq!(
        after,
        format!("{before}\n## Unit: nightly-report.service\n_Not described yet — what this unit does_\n")
    );
    // removing the unit removes nothing from the notes
    s.host.remove_file(Path::new("/etc/systemd/system/nightly-report.service")).unwrap();
    s.moi(&["snapshot"]).unwrap();
    assert_eq!(s.notes(), after);
}

#[test]
fn placeholders_are_not_notes() {
    assert!(!notes::meaningful(""));
    assert!(!notes::meaningful("_Not described yet — what this unit does_"));
    assert!(!notes::meaningful("  _Not described yet — anything_  \n"));
    assert!(notes::meaningful("_Not described yet — what this unit does_\nActually: it applies the firewall."));
    assert!(notes::meaningful("Applies the firewall."));
    let parsed = notes::parse("intro\n## A\none\n\ntwo\n## B\n\n## C  \n three ");
    assert_eq!(parsed, [("A".into(), "one\n\ntwo".into()), ("B".into(), String::new()), ("C".into(), "three".into())]);
}

#[test]
fn moi_notes_opens_the_editor_and_records_what_changed() {
    let s = server("notes-command");
    assert_eq!(
        s.moi(&["notes"]).unwrap_err(),
        "no snapshot yet: run `moi snapshot` first (notes are attached to what it finds)"
    );
    s.moi(&["snapshot"]).unwrap();
    let real = s.overlay.join("var/lib/moi/state/NOTES.md");

    // no editor installed
    let err = s.moi(&["notes"]).unwrap_err();
    assert_eq!(
        err,
        "no editor found (nano, vi): set EDITOR, or edit /var/lib/moi/state/NOTES.md yourself and run `moi snapshot`"
    );

    // the editor gets the real file; here the "owner" has typed something by the time it returns
    s.fixture.set_env("EDITOR", "nano -w");
    s.fixture.add_command(&format!("nano -w {}", real.display()), 0, "");
    assert_eq!(s.moi(&["notes"]).unwrap(), "notes unchanged\n");
    s.set_notes(&s.notes().replace(
        "_Not described yet — what this swarm service is_\n\n## Service: dokploy-bridge",
        "Compiler playground.\n\n## Service: dokploy-bridge",
    ));
    let out = s.moi(&["notes"]).unwrap();
    assert!(out.starts_with("notes saved: recorded "), "{out}");
    let log = s.moi(&["log", "-n", "1"]).unwrap();
    assert!(log.ends_with("  changed: NOTES.md\n"), "{log}");

    // $VISUAL wins over $EDITOR; an editor that fails records nothing
    s.fixture.set_env("VISUAL", "code --wait");
    s.fixture.add_command(&format!("code --wait {}", real.display()), 1, "");
    s.set_notes(&format!("{}\nmore\n", s.notes()));
    assert_eq!(s.moi(&["notes"]).unwrap_err(), "code exited with status 1: notes not recorded");
    assert!(s.moi(&["log", "-n", "1"]).unwrap().ends_with("  changed: NOTES.md\n"));
    assert_eq!(s.moi(&["log"]).unwrap().lines().count(), 2);
}

#[test]
fn crontab_lines_are_parsed_with_and_without_a_user_column() {
    let user = "# comment\nMAILTO=me@example.com\n\n0 9 * * 1 /home/me/bin/weekly.sh --flag  x\n@reboot  /home/me/bin/boot.sh\n*/5 * * * * cd /srv && ./run\n";
    assert_eq!(
        parse_crontab(user, false),
        [
            ("0 9 * * 1".into(), String::new(), "/home/me/bin/weekly.sh --flag  x".into()),
            ("@reboot".into(), String::new(), "/home/me/bin/boot.sh".into()),
            ("*/5 * * * *".into(), String::new(), "cd /srv && ./run".into()),
        ]
    );
    let system = "SHELL=/bin/sh\n17 *\t* * *\troot    cd / && run-parts --report /etc/cron.hourly\n@daily www-data /usr/local/bin/clean\n";
    assert_eq!(
        parse_crontab(system, true),
        [
            ("17 * * * *".into(), "root".into(), "cd / && run-parts --report /etc/cron.hourly".into()),
            ("@daily".into(), "www-data".into(), "/usr/local/bin/clean".into()),
        ]
    );
}

#[test]
fn the_view_reads_jobs_units_and_callers_from_a_snapshot() {
    let s = server("notes-view");
    s.moi(&["snapshot"]).unwrap();
    let view = View::new(s.host.as_ref(), &s.state);
    let jobs = view.cron_jobs();
    let job = |cmd: &str| jobs.iter().find(|j| j.command.contains(cmd)).unwrap_or_else(|| panic!("{cmd}"));
    assert_eq!(
        (job("weekly-note").source.as_str(), job("weekly-note").user.as_str(), job("weekly-note").schedule.as_str()),
        ("crontab -u prabhas", "prabhas", "0 9 * * 1")
    );
    assert_eq!(
        (job("storage-check.sh").source.as_str(), job("storage-check.sh").user.as_str()),
        ("/etc/cron.d/storage-check", "root")
    );
    assert!(!jobs.iter().any(|j| j.command.contains("run-parts")), "stock run-parts lines are not jobs of their own");

    let info = view.unit_info("/etc/systemd/system/server-backup.timer");
    assert_eq!(info["OnCalendar"], ["03:30"]);
    assert_eq!(
        view.unit_info("/etc/systemd/system/host-firewall.service")["Description"],
        ["Host firewall (INPUT + DOCKER-USER)"]
    );
    assert_eq!(
        view.callers("/opt/scripts/service-check.sh"),
        ["cron (/etc/cron.d/service-check, */5 * * * *)", "homelab-selfcheck.service"]
    );
    assert_eq!(view.callers("/opt/scripts/backup.sh.bak"), Vec::<String>::new());
    assert_eq!(view.incus_instances(), ["devbox"]);
    assert_eq!(view.container_states()["jellyfin"], "running");
    assert!(view.file("/etc/fstab").unwrap().contains("LABEL=storage"));
    assert_eq!(view.file("/usr/local/bin/ctop"), None, "binary copies are not text");
}
