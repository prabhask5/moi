//! Shared helpers for integration tests. Everything runs against fixture hosts, never the real system.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

pub mod fake_kuma;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::host::{FixtureHost, Host};

/// Directory of a fixture host (`tests/fixtures/hosts/<name>`).
pub fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hosts").join(name)
}

/// A fresh, empty scratch directory under cargo's target tmp dir, unique per test name.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The `minimal` fixture host, read-only.
pub fn minimal() -> FixtureHost {
    FixtureHost::open(&fixture_dir("minimal")).unwrap()
}

/// The `minimal` fixture host with a writable overlay in a scratch dir named after the test.
pub fn minimal_rw(test: &str) -> Arc<FixtureHost> {
    Arc::new(minimal().with_overlay(&scratch(test)).unwrap())
}

/// Write `moi.toml` (and optionally a 0600 `secrets.toml`) into the overlay under `/etc/moi`.
pub fn write_config(host: &FixtureHost, config: &str, secrets: Option<&str>) {
    host.create_dir_all(Path::new("/etc/moi"), 0o755).unwrap();
    host.write(Path::new("/etc/moi/moi.toml"), config.as_bytes(), 0o644).unwrap();
    if let Some(s) = secrets {
        host.write(Path::new("/etc/moi/secrets.toml"), s.as_bytes(), 0o600).unwrap();
    }
}

/// Compare with a golden file; `MOI_UPDATE_GOLDEN=1` rewrites it instead.
pub fn assert_golden(path: &Path, actual: &str) {
    if std::env::var_os("MOI_UPDATE_GOLDEN").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("golden file {} missing ({e}); run MOI_UPDATE_GOLDEN=1 cargo test", path.display()));
    assert!(
        expected == actual,
        "output differs from golden file {}\n--- expected\n{expected}\n--- actual\n{actual}\n(MOI_UPDATE_GOLDEN=1 cargo test regenerates it; review the diff)",
        path.display()
    );
}
