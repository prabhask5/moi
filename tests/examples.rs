//! The shipped examples stay valid, and `examples/moi.toml` documents exactly the defaults in the code
//! (docs/spec/01-config.md § Acceptance).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use cestmoi::config::{self, Paths};
use cestmoi::host::Host;

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(name)
}

fn paths() -> Paths {
    Paths::new("/etc/moi/moi.toml".into(), None)
}

/// A secrets file defining every `secret:NAME` the text refers to, with fake values.
fn fake_secrets(config: &str) -> String {
    let mut names: Vec<&str> = config
        .split("secret:")
        .skip(1)
        .map(|rest| rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next().unwrap())
        .filter(|name| !name.is_empty()) // prose such as "secret: refs" in a comment
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut out = String::from("[secrets]\n");
    for n in names {
        out.push_str(&format!("{n} = \"FAKE-{n}-value\"\n"));
    }
    out
}

#[test]
fn reference_example_is_exactly_the_defaults_in_the_code() {
    let host = common::minimal_rw("examples-reference");
    common::write_config(&host, "", None);
    let empty = config::load(host.as_ref(), &paths()).unwrap();
    common::assert_golden(&example("moi.toml"), &config::reference_toml(&empty));
}

#[test]
fn reference_example_parses_back_to_the_same_config() {
    let host = common::minimal_rw("examples-roundtrip");
    common::write_config(&host, "", None);
    let empty = config::load(host.as_ref(), &paths()).unwrap();
    host.write(Path::new("/etc/moi/moi.toml"), &std::fs::read(example("moi.toml")).unwrap(), 0o644).unwrap();
    let reloaded = config::load(host.as_ref(), &paths()).unwrap();
    assert_eq!(reloaded.config, empty.config);
    assert!(reloaded.warnings.is_empty());
}

#[test]
fn minimal_and_cestmoi_examples_validate_and_leak_nothing() {
    for name in ["minimal.toml", "cestmoi.toml"] {
        let text = std::fs::read_to_string(example(name)).unwrap();
        let host = common::minimal_rw(&format!("examples-{name}"));
        common::write_config(&host, &text, Some(&fake_secrets(&text)));
        let loaded = config::load(host.as_ref(), &paths()).unwrap_or_else(|e| panic!("examples/{name}:\n{e}"));
        assert!(loaded.warnings.is_empty(), "examples/{name}: {:?}", loaded.warnings);
        assert!(loaded.secrets_resolved > 0);
        let shown = format!("{}\n{}\n{:?}", loaded.effective_toml(), loaded.effective_json(), loaded.config);
        assert!(!shown.contains("FAKE-"), "examples/{name} leaks a secret:\n{shown}");
    }
}

#[test]
fn cestmoi_example_reproduces_the_legacy_setup() {
    let text = std::fs::read_to_string(example("cestmoi.toml")).unwrap();
    let host = common::minimal_rw("examples-cestmoi-values");
    common::write_config(&host, &text, Some(&fake_secrets(&text)));
    let c = config::load(host.as_ref(), &paths()).unwrap().config;
    assert_eq!(c.server.name, "cestmoi");
    assert_eq!(c.checks.resources.common.journal_tag, "resource-check");
    assert_eq!(c.checks.services.http.len(), 17);
    assert_eq!(c.checks.services.exec.len(), 4);
    assert_eq!(c.checks.edge.public_urls.len(), 7);
    assert_eq!(c.heal.len(), 3);
    assert!(c.firewall.manage && c.backup.enabled && c.backup.offsite.enabled && c.backup.offsite.r2_guard.enabled);
    assert_eq!(c.backup.offsite.env.len(), 2);
}
