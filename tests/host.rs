//! `host::Host` (CLAUDE.md §3): files under a root prefix, canned command outputs, fake clock.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use cestmoi::host::{self, Cmd, FileKind, FixtureHost, Host, RealHost};

// ------------------------------------------------------------------ FixtureHost: files

#[test]
fn fixture_reads_files_under_its_root() {
    let h = common::minimal();
    assert_eq!(h.read_to_string(Path::new("/etc/hosts")).unwrap(), "127.0.0.1 localhost\n");
    assert_eq!(h.read(Path::new("/proc/sys/kernel/hostname")).unwrap(), b"myserver.lan\n");
    assert!(h.exists(Path::new("/etc/moi/moi.toml")));
    assert!(!h.exists(Path::new("/etc/shadow")));
    assert_eq!(h.read(Path::new("/etc/shadow")).unwrap_err().kind(), ErrorKind::NotFound);
}

#[test]
fn relative_paths_are_rejected() {
    let h = common::minimal();
    assert_eq!(h.read(Path::new("etc/hosts")).unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn dotdot_cannot_escape_the_root() {
    let h = common::minimal();
    // `/..` is `/` on a real system; inside a fixture it must stay inside the fixture root.
    assert_eq!(h.read_to_string(Path::new("/../../../etc/hosts")).unwrap(), "127.0.0.1 localhost\n");
    assert!(!h.exists(Path::new("/../clock")));
}

#[test]
fn absolute_symlinks_resolve_inside_the_root() {
    let h = common::minimal();
    assert_eq!(h.read_link(Path::new("/etc/localtime")).unwrap(), Path::new("/usr/share/zoneinfo/Etc/UTC"));
    assert_eq!(h.read_to_string(Path::new("/etc/localtime")).unwrap(), "TZif-fake\n");
    assert_eq!(h.symlink_metadata(Path::new("/etc/localtime")).unwrap().kind, FileKind::Symlink);
    assert_eq!(h.metadata(Path::new("/etc/localtime")).unwrap().kind, FileKind::File);
}

#[test]
fn read_dir_is_sorted_names() {
    let h = common::minimal();
    assert_eq!(h.read_dir(Path::new("/etc")).unwrap(), ["hosts", "localtime", "moi"]);
    assert_eq!(h.read_dir(Path::new("/nope")).unwrap_err().kind(), ErrorKind::NotFound);
}

#[test]
fn metadata_uses_the_modes_manifest_and_defaults_to_root_ownership() {
    let h = common::minimal();
    let hosts = h.metadata(Path::new("/etc/hosts")).unwrap();
    assert_eq!((hosts.mode, hosts.uid, hosts.gid), (0o640, 0, 4));
    assert_eq!(hosts.kind, FileKind::File);
    assert_eq!(hosts.size, 20);
    let secrets = h.metadata(Path::new("/cases/full/secrets.toml")).unwrap();
    assert_eq!(secrets.mode, 0o600);
    let dir = h.metadata(Path::new("/etc")).unwrap();
    assert_eq!((dir.kind, dir.uid, dir.gid), (FileKind::Dir, 0, 0));
}

#[test]
fn fixture_without_overlay_is_read_only() {
    let h = common::minimal();
    let e = h.write(Path::new("/etc/new"), b"x", 0o644).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::PermissionDenied);
    assert!(h.create_dir_all(Path::new("/var/lib/moi"), 0o700).is_err());
    assert!(h.remove_file(Path::new("/etc/hosts")).is_err());
    assert!(common::fixture_dir("minimal").join("root/etc/hosts").exists());
}

#[test]
fn overlay_takes_writes_and_shadows_the_fixture() {
    let h = common::minimal_rw("host-overlay");
    h.write(Path::new("/etc/hosts"), b"changed\n", 0o600).unwrap();
    assert_eq!(h.read_to_string(Path::new("/etc/hosts")).unwrap(), "changed\n");
    assert_eq!(h.metadata(Path::new("/etc/hosts")).unwrap().mode, 0o600);
    // the committed fixture is untouched
    let base = common::fixture_dir("minimal").join("root/etc/hosts");
    assert_eq!(std::fs::read_to_string(base).unwrap(), "127.0.0.1 localhost\n");

    h.create_dir_all(Path::new("/var/lib/moi/state"), 0o700).unwrap();
    assert_eq!(h.metadata(Path::new("/var/lib/moi/state")).unwrap().mode, 0o700);
    h.write(Path::new("/var/lib/moi/state/a"), b"1", 0o644).unwrap();
    h.write(Path::new("/etc/zz"), b"2", 0o644).unwrap();
    assert_eq!(h.read_dir(Path::new("/etc")).unwrap(), ["hosts", "localtime", "moi", "zz"]);

    h.set_mode(Path::new("/etc/zz"), 0o400).unwrap();
    assert_eq!(h.metadata(Path::new("/etc/zz")).unwrap().mode, 0o400);
    h.remove_file(Path::new("/etc/zz")).unwrap();
    assert!(!h.exists(Path::new("/etc/zz")));

    // writing into a directory that exists nowhere fails like a real filesystem
    assert_eq!(h.write(Path::new("/no/such/dir/f"), b"x", 0o644).unwrap_err().kind(), ErrorKind::NotFound);
}

#[test]
fn write_is_atomic_and_leaves_no_temp_files() {
    let h = common::minimal_rw("host-atomic");
    h.create_dir_all(Path::new("/data"), 0o755).unwrap();
    h.write(Path::new("/data/f"), b"one", 0o644).unwrap();
    h.write(Path::new("/data/f"), b"two", 0o644).unwrap();
    assert_eq!(h.read(Path::new("/data/f")).unwrap(), b"two");
    assert_eq!(h.read_dir(Path::new("/data")).unwrap(), ["f"]);
}

#[test]
fn real_path_maps_into_overlay_for_libraries_that_need_a_path() {
    let scratch = common::scratch("host-real-path");
    let h = common::minimal().with_overlay(&scratch).unwrap();
    h.create_dir_all(Path::new("/var/lib/moi"), 0o700).unwrap();
    let p = h.real_path(Path::new("/var/lib/moi/moi.db")).unwrap();
    assert!(p.starts_with(&scratch), "{}", p.display());
    assert!(p.ends_with("var/lib/moi/moi.db"));
}

// ------------------------------------------------------------------ FixtureHost: commands, clock, env, journal

#[test]
fn canned_command_output_comes_from_the_commands_dir() {
    let h = common::minimal();
    let cmd = Cmd::new("smartctl").args(["--json", "-H", "/dev/sda"]);
    assert_eq!(cmd.line(), "smartctl --json -H /dev/sda");
    assert_eq!(FixtureHost::command_key(&cmd.line()), "6be6a1a671445cb7");
    let out = h.run(&cmd).unwrap();
    assert_eq!(out.status, 4);
    assert_eq!(out.stdout_str(), "{\"smart_status\":{\"passed\":true}}\n");
    assert!(!out.timed_out);
    assert_eq!(h.commands_run(), ["smartctl --json -H /dev/sda"]);
}

#[test]
fn unknown_command_fails_and_says_which_file_to_add() {
    let h = common::minimal();
    let e = h.run(&Cmd::new("iptables").args(["-S", "DOCKER-USER"])).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
    let msg = e.to_string();
    assert!(msg.contains("iptables -S DOCKER-USER"), "{msg}");
    assert!(msg.contains(&format!("{}.out", FixtureHost::command_key("iptables -S DOCKER-USER"))), "{msg}");
}

#[test]
fn commands_can_be_canned_in_memory() {
    let h = common::minimal();
    h.add_command("restic version", 0, "restic 0.18.0\n");
    let out = h.run(&Cmd::new("restic").arg("version")).unwrap();
    assert_eq!((out.status, out.stdout_str().as_ref()), (0, "restic 0.18.0\n"));
    assert!(out.success());
}

#[test]
fn clock_is_fixed_and_only_moves_when_told() {
    let h = common::minimal();
    let t0 = h.now();
    assert_eq!(host::format_rfc3339(t0), "2026-03-01T12:00:00Z");
    assert_eq!(h.now(), t0);
    h.advance(Duration::from_secs(90));
    assert_eq!(host::format_rfc3339(h.now()), "2026-03-01T12:01:30Z");
    h.set_now(UNIX_EPOCH + Duration::from_secs(86_400));
    assert_eq!(host::format_rfc3339(h.now()), "1970-01-02T00:00:00Z");
}

#[test]
fn rfc3339_round_trips_and_handles_offsets() {
    for s in ["1970-01-01T00:00:00Z", "2000-02-29T23:59:59Z", "2026-11-27T08:15:00Z", "2100-03-01T00:00:00Z"] {
        assert_eq!(host::format_rfc3339(host::parse_rfc3339(s).unwrap()), s);
    }
    let with_offset = host::parse_rfc3339("2026-03-01T07:00:00-05:00").unwrap();
    assert_eq!(host::format_rfc3339(with_offset), "2026-03-01T12:00:00Z");
    assert_eq!(host::parse_rfc3339("2026-03-01T12:00:00.250Z"), host::parse_rfc3339("2026-03-01T12:00:00Z"));
    for bad in ["", "2026-03-01", "2026-13-01T00:00:00Z", "2026-02-30T00:00:00Z", "2026-03-01T25:00:00Z", "yesterday"] {
        assert!(host::parse_rfc3339(bad).is_none(), "{bad}");
    }
}

#[test]
fn env_is_isolated_from_the_process_environment() {
    let h = common::minimal();
    assert_eq!(h.env("PATH"), None);
    assert_eq!(h.env("HOME"), None);
    h.set_env("MOI_SECRET_X", "FAKEvalue");
    assert_eq!(h.env("MOI_SECRET_X").as_deref(), Some("FAKEvalue"));
}

#[test]
fn hostname_and_timezone_helpers_read_through_the_host() {
    let h = common::minimal();
    assert_eq!(host::short_hostname(&h), "myserver");
    assert_eq!(host::timezone(&h).as_deref(), Some("Etc/UTC"));
}

// ------------------------------------------------------------------ RealHost (only ever pointed at a scratch root)

#[test]
fn real_host_with_root_prefix_stays_inside_it() {
    let root = common::scratch("host-real-root");
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/hostname"), "scratch\n").unwrap();
    std::os::unix::fs::symlink("/etc/hostname", root.join("etc/alias")).unwrap();
    let h = RealHost::with_root(&root);

    assert_eq!(h.read_to_string(Path::new("/etc/hostname")).unwrap(), "scratch\n");
    assert_eq!(h.read_to_string(Path::new("/etc/alias")).unwrap(), "scratch\n");
    assert_eq!(h.read_to_string(Path::new("/../etc/hostname")).unwrap(), "scratch\n");
    h.create_dir_all(Path::new("/var/lib/moi"), 0o700).unwrap();
    h.write(Path::new("/var/lib/moi/x"), b"data", 0o600).unwrap();
    assert_eq!(std::fs::read(root.join("var/lib/moi/x")).unwrap(), b"data");
    let md = h.metadata(Path::new("/var/lib/moi/x")).unwrap();
    assert_eq!((md.mode, md.size, md.kind), (0o600, 4, FileKind::File));
    assert_eq!(h.read_dir(Path::new("/var/lib/moi")).unwrap(), ["x"]);
    assert_eq!(h.real_path(Path::new("/var/lib/moi/x")).unwrap(), root.join("var/lib/moi/x"));
}

#[test]
fn real_host_with_root_prefix_refuses_to_run_commands_or_log() {
    let h = RealHost::with_root(&common::scratch("host-real-norun"));
    assert_eq!(h.run(&Cmd::new("true")).unwrap_err().kind(), ErrorKind::Unsupported);
    assert!(h.journal(&[("MESSAGE".into(), "x".into())]).is_err());
    assert_eq!(h.connect_unix(Path::new("/var/run/docker.sock")).unwrap_err().kind(), ErrorKind::Unsupported);
}

#[test]
fn real_host_runs_commands_with_exit_code_output_stdin_and_timeout() {
    // Harmless shell builtins only: this exercises process plumbing, not the machine's state.
    let h = RealHost::new();
    let out = h.run(&Cmd::new("sh").args(["-c", "cat; echo err >&2; exit 3"]).stdin(b"hello".to_vec())).unwrap();
    assert_eq!(out.status, 3);
    assert_eq!(out.stdout, b"hello");
    assert_eq!(out.stderr, b"err\n");
    assert!(!out.timed_out && !out.success());

    let out = h.run(&Cmd::new("sh").args(["-c", "echo $MOI_T"]).env("MOI_T", "v")).unwrap();
    assert_eq!(out.stdout, b"v\n");

    let started = std::time::Instant::now();
    let out = h.run(&Cmd::new("sleep").arg("30").timeout(Duration::from_millis(150))).unwrap();
    assert!(out.timed_out);
    assert!(!out.success());
    assert!(started.elapsed() < Duration::from_secs(5));

    assert_eq!(h.run(&Cmd::new("moi-no-such-program-xyz")).unwrap_err().kind(), ErrorKind::NotFound);
}
