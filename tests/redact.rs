//! Redaction rules (docs/spec/03-context.md § Redaction). Grows with M3.

use cestmoi::redact::{self, env_entry, is_sensitive_key, mask_token};

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

#[test]
fn text_masks_secrets_by_shape() {
    for (input, expected) in [
        // env files and env lists
        ("POSTGRES_PASSWORD=hunter2", "POSTGRES_PASSWORD=***"),
        ("   \"TUNNEL_TOKEN=FAKEtunnel000\",", "   \"TUNNEL_TOKEN=***\","),
        ("+  -e CF_DNS_API_TOKEN=FAKEcf000 \\", "+  -e CF_DNS_API_TOKEN=*** \\"),
        ("SONARR_KEY=\"FAKE key with spaces\"; next", "SONARR_KEY=\"***\"; next"),
        ("TZ=America/New_York PUID=1000", "TZ=America/New_York PUID=1000"),
        // URLs with credentials
        (
            "DATABASE_URL=postgres://outline:FAKEpw@db:5432/outline",
            "DATABASE_URL=postgres://outline:***@db:5432/outline",
        ),
        ("see https://example.com/a:b@c for details", "see https://example.com/a:b@c for details"),
        // push and ping tokens keep their last four characters
        (
            "curl http://uptime.home/api/push/FAKEresourcesTok01?status=up",
            "curl http://uptime.home/api/push/…ok01?status=up",
        ),
        ("https://hc-ping.com/FAKEpingkey0000000000/slug", "https://hc-ping.com/…0000/slug"),
        // key: value forms
        ("  password: hunter2", "  password: ***"),
        ("  \"api_token\": \"FAKEtoken\",", "  \"api_token\": \"***\","),
        ("secret_access_key = \"FAKEsecret\"", "secret_access_key = \"***\""),
        ("-    smtpPassword: old", "-    smtpPassword: ***"),
        ("password = \"secret:kuma_password\"", "password = \"secret:kuma_password\""),
        ("  labels: {", "  labels: {"),
        ("image: postgres:18", "image: postgres:18"),
        // shadow
        ("prabhas:$y$j9T$FAKEsalt$FAKEhash:20400:0:99999:7:::", "prabhas:***:20400:0:99999:7:::"),
        ("root:!:20400:0:99999:7:::", "root:!:20400:0:99999:7:::"),
    ] {
        assert_eq!(redact::line(input), expected, "{input}");
    }
    assert_eq!(redact::text("A_SECRET=x\nplain\nB_TOKEN=y"), "A_SECRET=***\nplain\nB_TOKEN=***");
}
