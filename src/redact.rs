//! Secret detection and masking — the single place that decides what is a secret.
//!
//! Spec: docs/spec/03-context.md § Redaction, docs/spec/14-security.md. The rest of the rules
//! (URL credentials, `password=` forms, long tokens, hc-ping URLs) are implemented in milestone M3;
//! the env-key rule below is used by every module that prints container or service environments.

/// Environment variable names whose values are always secret.
const SENSITIVE_KEY_PARTS: &[&str] = &[
    "PASS",
    "PWD",
    "SECRET",
    "TOKEN",
    "KEY",
    "PRIVATE",
    "CRED",
    "AUTH",
    "SALT",
    "COOKIE",
    "SESSION",
    "SIGNATURE",
    "DSN",
    "WEBHOOK",
    "LICENSE",
    "ACCESS",
];

/// True if an environment variable with this name holds a secret.
pub fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SENSITIVE_KEY_PARTS.iter().any(|part| upper.contains(part))
}

/// Mask a `KEY=value` environment entry if its key is sensitive.
pub fn env_entry(entry: &str) -> String {
    match entry.split_once('=') {
        Some((k, v)) if !v.is_empty() && is_sensitive_key(k) => format!("{k}=***"),
        _ => entry.to_string(),
    }
}

/// Mask a push token or similar identifier, keeping the last 4 characters (`…abcd`).
pub fn mask_token(token: &str) -> String {
    let n = token.chars().count();
    if n <= 6 {
        return token.to_string();
    }
    let tail: String = token.chars().skip(n - 4).collect();
    format!("…{tail}")
}
