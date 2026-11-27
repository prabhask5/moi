//! Redaction rules (docs/spec/03-context.md § Redaction). Grows with M3.

use cestmoi::redact::{env_entry, is_sensitive_key, mask_token};

#[test]
fn sensitive_env_keys_are_masked() {
    for e in ["POSTGRES_PASSWORD=hunter2", "CF_DNS_API_TOKEN=abc", "SECRET_KEY_BASE=x", "TUNNEL_TOKEN=y"] {
        assert!(env_entry(e).ends_with("=***"), "{e}");
    }
}

#[test]
fn harmless_env_keys_are_kept() {
    for e in ["TZ=America/New_York", "PUID=1000", "NODE_ENV=production", "EMPTY_PASSWORD="] {
        assert_eq!(env_entry(e), e);
    }
    assert!(!is_sensitive_key("PGDATA"));
}

#[test]
fn tokens_keep_last_four() {
    assert_eq!(mask_token("FAKEres0001"), "…0001");
    assert_eq!(mask_token("short"), "short");
}
