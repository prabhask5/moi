//! Mutation tests: thousands of damaged configurations and Docker payloads, none of which may make
//! moi panic (docs/spec/14-security.md). Deterministic — the same inputs on every run — so a failure
//! names an input that fails again. Not coverage-guided fuzzing: see the spec for what that means.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use cestmoi::checks::{self, Mode};
use cestmoi::config::{self, Paths};
use cestmoi::docker::Docker;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::state::StateDir;
use serde_json::Value;

/// xorshift64: small, fast, and the same sequence everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { usize::try_from(self.next() % n as u64).unwrap() }
    }
}

/// Damage a text the way hands and editors do: cut it, drop or double a line, swap a character for
/// one that means something to the parser.
fn damage(text: &str, rng: &mut Rng) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    for _ in 0..=rng.below(3) {
        if bytes.is_empty() {
            break;
        }
        let at = rng.below(bytes.len());
        match rng.below(7) {
            0 => bytes.truncate(at),
            1 => bytes[at] = b"[]{}=\"'#,.\n\\-0 "[rng.below(15)],
            2 => bytes[at] = u8::try_from(rng.below(256)).unwrap(),
            3 => {
                bytes.remove(at);
            }
            4 => {
                // a line twice: duplicate keys and tables
                let start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                let end = bytes[at..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |i| at + i + 1);
                let line = bytes[start..end].to_vec();
                bytes.splice(start..start, line);
            }
            5 => {
                let start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                let end = bytes[at..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |i| at + i + 1);
                bytes.drain(start..end);
            }
            _ => {
                let token: &[u8] =
                    [b"99999999999999999999".as_slice(), b"-1", b"\"\"", b"[[", b"secret:", b"1e999", b"0d", b"\0"]
                        [rng.below(8)];
                bytes.splice(at..at, token.iter().copied());
            }
        }
    }
    bytes
}

fn quietly<T>(what: impl FnOnce() -> T) -> Result<T, String> {
    // the hosts are shared with nothing that outlives a failed round
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(what)).map_err(|panic| {
        panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "panic".to_string())
    })
}

#[test]
fn no_damaged_configuration_makes_the_loader_panic() {
    let host = common::minimal_rw("fuzz-config");
    let dir = common::fixture_dir("minimal").join("root/cases/full");
    let sources = [
        std::fs::read_to_string(dir.join("moi.toml")).unwrap(),
        std::fs::read_to_string(common::fixture_dir("cestmoi").join("root/etc/moi/moi.toml")).unwrap(),
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/moi.toml")).unwrap(),
    ];
    let secrets = std::fs::read_to_string(dir.join("secrets.toml")).unwrap();
    host.create_dir_all(Path::new("/etc/moi"), 0o755).unwrap();
    let paths = Paths::new("/etc/moi/moi.toml".into(), None);
    let mut rng = Rng(0x00C0_FFEE_D00D_F00D);
    let (mut loaded, mut refused) = (0, 0);
    for round in 0..4000 {
        let source = &sources[round % sources.len()];
        let text = damage(source, &mut rng);
        // every fourth round the secrets are the damaged file
        let (config_bytes, secret_bytes) = if round % 4 == 3 {
            (source.as_bytes().to_vec(), damage(&secrets, &mut rng))
        } else {
            (text, secrets.as_bytes().to_vec())
        };
        host.write(&paths.config, &config_bytes, 0o644).unwrap();
        host.write(&paths.secrets, &secret_bytes, 0o600).unwrap();
        let shared: Arc<dyn Host> = host.clone();
        let paths = paths.clone();
        match quietly(move || config::load(shared.as_ref(), &paths).map(|_| ())) {
            Ok(Ok(())) => loaded += 1,
            Ok(Err(_)) => refused += 1,
            Err(panic) => {
                panic!("round {round}: the loader panicked ({panic}) on:\n{}", String::from_utf8_lossy(&config_bytes))
            }
        }
    }
    // the damage is real, and not all of it fatal
    assert!(refused > 1000 && loaded > 100, "{loaded} loaded, {refused} refused");
}

/// Replace, remove or retype a random part of a JSON document.
fn damage_json(value: &mut Value, rng: &mut Rng, depth: usize) {
    let replacement = |rng: &mut Rng| match rng.below(9) {
        0 => Value::Null,
        1 => Value::Bool(true),
        2 => serde_json::json!(-1),
        3 => serde_json::json!(18_446_744_073_709_551_615_u64),
        4 => serde_json::json!(""),
        5 => serde_json::json!("../../etc/passwd\n\u{0}\u{202e}"),
        6 => serde_json::json!([]),
        7 => serde_json::json!({}),
        _ => serde_json::json!(1e300),
    };
    // deeper more often than not, so that leaves are hit as well as whole documents
    let descend = depth < 8 && rng.below(4) != 0;
    match value {
        Value::Object(map) if descend && !map.is_empty() => {
            let key = map.keys().nth(rng.below(map.len())).cloned().unwrap();
            if rng.below(6) == 0 {
                map.remove(&key);
            } else {
                damage_json(map.get_mut(&key).unwrap(), rng, depth + 1);
            }
        }
        Value::Array(items) if descend && !items.is_empty() => {
            let at = rng.below(items.len());
            match rng.below(8) {
                0 => {
                    items.remove(at);
                }
                1 => {
                    let copy = items[at].clone();
                    items.push(copy);
                }
                _ => damage_json(&mut items[at], rng, depth + 1),
            }
        }
        other => *other = replacement(rng),
    }
}

#[test]
fn no_damaged_docker_answer_makes_a_check_or_the_monitor_plan_panic() {
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_overlay(&common::scratch("fuzz-docker"))
            .unwrap(),
    );
    let host: Arc<dyn Host> = fixture.clone();
    let loaded = config::load(host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let config = Arc::new(loaded.config);
    let state = StateDir::new("/var/lib/moi");
    state.ensure(host.as_ref()).unwrap();
    let docker_dir = common::fixture_dir("cestmoi").join("docker");
    let read =
        |file: &str| -> Value { serde_json::from_slice(&std::fs::read(docker_dir.join(file)).unwrap()).unwrap() };
    let container = std::fs::read_dir(docker_dir.join("containers"))
        .unwrap()
        .flatten()
        .find(|e| e.path().is_dir())
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    // the answers the checks and the plans are built on
    let answers = [
        ("/containers/json", read("containers/json.json")),
        (&*format!("/containers/{container}/json"), read(&format!("containers/{container}/json.json"))),
        ("/services", read("services.json")),
        ("/networks", read("networks.json")),
        ("/volumes", read("volumes.json")),
        ("/info", read("info.json")),
    ]
    .map(|(path, value)| (path.to_string(), value));
    let checks = checks::enabled(&config);
    let mut rng = Rng(0xD0C4_E500_BAD5_EED5);
    let mut rounds = 0;
    for round in 0..240 {
        let (path, original) = &answers[round % answers.len()];
        let mut damaged = original.clone();
        for _ in 0..=rng.below(3) {
            damage_json(&mut damaged, &mut rng, 0);
        }
        // every tenth answer is not JSON at all
        let body = if round % 10 == 9 {
            damaged.to_string()[..damaged.to_string().len() / 2].to_string()
        } else {
            damaged.to_string()
        };
        fixture.set_docker(path, &body);
        let (host, config, state, checks) = (host.clone(), config.clone(), state.clone(), checks.clone());
        let outcome = quietly(move || {
            let db = state.open_db(host.as_ref()).unwrap();
            for check in &checks {
                let _ = checks::run(&host, &config, &db, check, Mode::Dry);
            }
            let docker = Docker::new(host.clone());
            let _ = cestmoi::monitors::desired(&config, &docker);
            let _ = cestmoi::daemon::compose_dirs(host.as_ref(), "/opt/services/**");
        });
        if let Err(panic) = outcome {
            panic!("round {round}: a damaged answer to GET {path} caused a panic ({panic}):\n{body}");
        }
        fixture.set_docker(path, &original.to_string());
        rounds += 1;
    }
    assert_eq!(rounds, 240);
}

#[test]
fn no_text_from_outside_makes_the_small_parsers_panic() {
    use cestmoi::daemon::zone::Zone;
    let mut rng = Rng(0x5EED_5EED_0BAD_F00D);
    // the time zone file of the machine: a binary format with counts that say how much follows
    let tzif = std::fs::read(common::fixture_dir("cestmoi").join("root/etc/localtime")).unwrap_or_default();
    assert!(Zone::parse(&tzif).is_some(), "the fixture's zone file parses");
    for round in 0..6000 {
        let mut bytes = tzif.clone();
        for _ in 0..=rng.below(4) {
            let at = rng.below(bytes.len().max(1)).min(bytes.len().saturating_sub(1));
            match rng.below(4) {
                0 if !bytes.is_empty() => bytes[at] = u8::try_from(rng.below(256)).unwrap(),
                1 if !bytes.is_empty() => bytes[at] = 0xff,
                2 => bytes.truncate(at),
                _ if !bytes.is_empty() => {
                    bytes.remove(at);
                }
                _ => {}
            }
        }
        if let Err(panic) = quietly(|| {
            Zone::parse(&bytes).map(|zone| {
                (
                    zone.to_local(1_795_000_000),
                    zone.to_utc(1_795_000_000),
                    zone.offset_at(i64::MAX),
                    zone.offset_at(i64::MIN),
                )
            })
        }) {
            panic!("round {round}: a damaged zone file caused a panic ({panic}): {bytes:?}");
        }
    }
    // text that scripts, containers and monitors hand over: masked and parsed, whatever is in it
    let pieces = [
        "password=",
        "token: ",
        "https://user:pa55@host/x?key=",
        "Bearer ",
        "é",
        "日本",
        "\u{202e}",
        "\u{0}",
        "=",
        ":",
        "@",
        "\"",
        "'",
        "\n",
        " ",
        "AKIA",
        "2026-11-26T17:00:00Z",
        "+05:30",
        "-",
        "T",
        "9999",
        "🔑",
        "API_KEY",
        "secret",
        "//",
        "?",
        "&",
        "#",
    ];
    for round in 0..20_000 {
        let mut text = String::new();
        for _ in 0..rng.below(9) {
            text.push_str(pieces[rng.below(pieces.len())]);
            if rng.below(3) == 0 {
                text.push(char::from_u32(u32::try_from(rng.below(0x2FFF)).unwrap()).unwrap_or('x'));
            }
        }
        let outcome = quietly(|| {
            let _ = cestmoi::redact::text(&text);
            let _ = cestmoi::redact::line(&text);
            let _ = cestmoi::redact::env_entry(&text);
            let _ = cestmoi::redact::mask_token(&text);
            let _ = cestmoi::redact::Masker::new([text.clone(), "é".to_string()]).mask(&format!("x{text}y{text}"));
            let _ = cestmoi::host::parse_rfc3339(&text);
            let _ = cestmoi::migrate::compare::mask_numbers(&text);
        });
        if let Err(panic) = outcome {
            panic!("round {round}: {text:?} caused a panic ({panic})");
        }
    }
}
