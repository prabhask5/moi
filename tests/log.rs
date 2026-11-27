//! Logging (docs/spec/10-daemon.md § Logging): structured journal fields, stderr fallback, secrets masked.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use cestmoi::host::FixtureHost;
use cestmoi::log::{self, Level, Logger, Target};
use cestmoi::redact::Masker;

fn entry(host: &FixtureHost, i: usize) -> Vec<(String, String)> {
    host.journal_entries()[i].clone()
}

fn pairs(e: &[(String, String)]) -> Vec<(&str, &str)> {
    e.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

#[test]
fn journal_entries_carry_structured_fields() {
    let host = Arc::new(common::minimal());
    let log = Logger::new(host.clone(), "moi").with_target(Target::Journal);
    log.warn("backup is late", &[("job", "backup"), ("late-by", "2h"), ("MOI_CHECK", "storage")]);
    assert_eq!(
        pairs(&entry(&host, 0)),
        [
            ("MESSAGE", "backup is late"),
            ("PRIORITY", "4"),
            ("SYSLOG_IDENTIFIER", "moi"),
            ("MOI_JOB", "backup"),
            ("MOI_LATE_BY", "2h"),
            ("MOI_CHECK", "storage"),
        ]
    );
    log.error("boom", &[]);
    log.info("fine", &[]);
    let prios: Vec<String> = host.journal_entries().iter().map(|e| e[1].1.clone()).collect();
    assert_eq!(prios, ["4", "3", "6"]);
}

#[test]
fn identifier_can_be_a_legacy_tag() {
    let host = Arc::new(common::minimal());
    Logger::new(host.clone(), "resource-check").with_target(Target::Journal).info("up: OK", &[]);
    assert_eq!(entry(&host, 0)[2], ("SYSLOG_IDENTIFIER".to_string(), "resource-check".to_string()));
}

#[test]
fn minimum_level_filters() {
    let host = Arc::new(common::minimal());
    let log = Logger::new(host.clone(), "moi").with_target(Target::Journal).with_min_level(Level::Warning);
    log.info("chatty", &[]);
    log.debug("chattier", &[]);
    log.warn("kept", &[]);
    assert_eq!(host.journal_entries().len(), 1);
}

#[test]
fn secrets_are_masked_in_messages_and_fields() {
    let host = Arc::new(common::minimal());
    let log = Logger::new(host.clone(), "moi")
        .with_target(Target::Journal)
        .with_masker(Masker::new(["FAKEkuma-password".to_string()]));
    log.error("login failed with FAKEkuma-password", &[("url", "http://admin:FAKEkuma-password@kuma.lan")]);
    let e = entry(&host, 0);
    assert_eq!(e[0].1, "login failed with ***");
    assert_eq!(e[3].1, "http://admin:***@kuma.lan");
}

#[test]
fn target_is_the_journal_only_under_systemd() {
    let host = Arc::new(common::minimal());
    assert_eq!(Logger::new(host.clone(), "moi").target(), Target::Stderr);
    host.set_env("JOURNAL_STREAM", "8:12345");
    assert_eq!(Logger::new(host.clone(), "moi").target(), Target::Journal);
}

#[test]
fn stderr_lines_are_human_readable() {
    assert_eq!(
        log::format_line(Level::Warning, "backup is late", &[("job", "backup")]),
        "moi: warning: backup is late (job=backup)"
    );
    assert_eq!(log::format_line(Level::Error, "boom", &[]), "moi: error: boom");
    assert_eq!(log::format_line(Level::Info, "config ok", &[]), "moi: config ok");
}

#[test]
fn native_journal_protocol_encoding() {
    let fields =
        [("MESSAGE".to_string(), "one line".to_string()), ("MOI_DETAIL".to_string(), "two\nlines".to_string())];
    let mut expected = b"MESSAGE=one line\nMOI_DETAIL\n".to_vec();
    expected.extend_from_slice(&9u64.to_le_bytes());
    expected.extend_from_slice(b"two\nlines\n");
    assert_eq!(log::encode_journal(&fields), expected);
}

#[test]
fn masker_replaces_known_values_and_hides_them_from_debug() {
    let m = Masker::new(["FAKElong-secret".to_string(), "abc".to_string(), String::new()]);
    assert_eq!(m.mask("x FAKElong-secret y FAKElong-secret"), "x *** y ***");
    // short secrets are only masked as whole values, so ordinary text is not mangled
    assert_eq!(m.mask("abcdef"), "abcdef");
    assert_eq!(m.mask("abc"), "***");
    assert!(m.is_secret("abc") && !m.is_secret("abcd"));
    assert!(!format!("{m:?}").contains("FAKE"));
    assert_eq!(Masker::default().mask("unchanged"), "unchanged");
}
