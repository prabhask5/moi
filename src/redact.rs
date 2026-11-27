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

/// Secrets shorter than this are only masked when they are a whole value: replacing every occurrence of
/// a 3-character password inside ordinary text would mangle the text without protecting anything.
const MIN_SUBSTRING_LEN: usize = 6;

/// Masks secret values that are known exactly (everything resolved from `secrets.toml` and
/// `MOI_SECRET_*`). Applied to every string that leaves the process: logs, effective config, `--json`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Masker {
    /// Longest first, so a secret containing another is masked whole.
    values: Vec<String>,
}

impl Masker {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut values: Vec<String> = values.into_iter().filter(|v| !v.is_empty()).collect();
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Self { values }
    }

    /// Number of distinct secret values known.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// True if the whole string is a secret value.
    pub fn is_secret(&self, text: &str) -> bool {
        self.values.iter().any(|v| v == text)
    }

    /// The text with every known secret replaced by `***`.
    pub fn mask(&self, text: &str) -> String {
        if self.is_secret(text) {
            return MASK.to_string();
        }
        let mut out = text.to_string();
        for v in self.values.iter().filter(|v| v.len() >= MIN_SUBSTRING_LEN) {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), MASK);
            }
        }
        out
    }
}

/// What a masked secret looks like.
pub const MASK: &str = "***";

impl std::fmt::Debug for Masker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Masker({} secrets)", self.values.len())
    }
}
