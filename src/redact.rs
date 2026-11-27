//! Secret detection and masking — the single place that decides what is a secret.
//!
//! Spec: docs/spec/03-context.md § Redaction, docs/spec/14-security.md. [`text`] masks what can be
//! recognised by shape (assignments to sensitive names, URL credentials, push and ping tokens, password
//! hashes); [`Masker`] masks values that are known exactly.

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

/// Mask secrets in free text (a diff, a config file, a log excerpt), line by line.
pub fn text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        out.push_str(&self::line(body));
        out.push_str(newline);
    }
    out
}

/// Mask secrets in one line of text.
pub fn line(input: &str) -> String {
    let mut s = url_credentials(input);
    s = path_tokens(&s, "/api/push/");
    s = path_tokens(&s, "hc-ping.com/");
    s = shadow_hash(&s);
    s = assignments(&s);
    key_value(&s)
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// `scheme://user:password@host` → `scheme://user:***@host`.
fn url_credentials(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (before, after) = rest.split_at(i + 3);
        out.push_str(before);
        let authority_end =
            after.find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'').unwrap_or(after.len());
        match after[..authority_end].rfind('@') {
            Some(at) => match after[..at].find(':') {
                Some(colon) if at > colon + 1 => {
                    out.push_str(&after[..=colon]);
                    out.push_str(MASK);
                    rest = &after[at..];
                }
                _ => rest = after,
            },
            None => rest = after,
        }
    }
    out.push_str(rest);
    out
}

/// The token that follows `marker` (a Kuma push token, a healthchecks ping key) → `…last4`.
fn path_tokens(s: &str, marker: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(marker) {
        let (before, after) = rest.split_at(i + marker.len());
        out.push_str(before);
        let len = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(after.len());
        out.push_str(&mask_token(&after[..len]));
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// `/etc/shadow` lines: `name:$y$…:…` → `name:***:…`.
fn shadow_hash(s: &str) -> String {
    let mut fields: Vec<&str> = s.split(':').collect();
    let hashed = |f: &str| f.starts_with('$') && f.matches('$').count() >= 3;
    match fields.iter().position(|f| hashed(f.trim_start_matches(['+', '-', ' ']))) {
        Some(i) if i >= 1 && fields.len() >= 3 => {
            fields[i] = MASK;
            fields.join(":")
        }
        _ => s.to_string(),
    }
}

/// `NAME=value` where NAME is sensitive, wherever it appears on the line (env files, JSON string lists,
/// `-e NAME=value` arguments). A quoted value is masked up to its closing quote.
fn assignments(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut copied = 0;
    while i < chars.len() {
        if chars[i] != '=' {
            i += 1;
            continue;
        }
        let mut start = i;
        while start > 0 && name_char(chars[start - 1]) {
            start -= 1;
        }
        let key: String = chars[start..i].iter().collect();
        let mut end = i + 1;
        if key.is_empty() || !is_sensitive_key(&key) || end >= chars.len() {
            i += 1;
            continue;
        }
        let quote = matches!(chars[end], '"' | '\'').then(|| chars[end]);
        if let Some(q) = quote {
            end += 1;
            while end < chars.len() && chars[end] != q {
                end += 1;
            }
        } else {
            while end < chars.len() && !chars[end].is_whitespace() && !matches!(chars[end], '"' | '\'') {
                end += 1;
            }
        }
        let value_start = i + 1 + usize::from(quote.is_some());
        if end > value_start {
            out.extend(&chars[copied..value_start]);
            out.push_str(MASK);
            copied = end;
        }
        i = end.max(i + 1);
    }
    out.extend(&chars[copied..]);
    out
}

/// A line that is `name: value` or `"name": "value"` (YAML, JSON, INI with spaces) with a sensitive name.
fn key_value(s: &str) -> String {
    let indent = s.len() - s.trim_start_matches([' ', '\t', '+', '-']).len();
    let body = &s[indent..];
    let unquoted = body.trim_start_matches(['"', '\'']);
    let key_len = unquoted.find(|c: char| !name_char(c)).unwrap_or(unquoted.len());
    let key = &unquoted[..key_len];
    let after_key = unquoted[key_len..].trim_start_matches(['"', '\'']);
    let separator = if after_key.starts_with(": ") {
        ": "
    } else if after_key.starts_with(" = ") {
        " = "
    } else {
        return s.to_string();
    };
    if key.is_empty() || !is_sensitive_key(key) {
        return s.to_string();
    }
    let value = &after_key[separator.len()..];
    let trailing_comma = value.trim_end().ends_with(',');
    let bare = value.trim_end().trim_end_matches(',');
    // structures and references are not values: `password: {`, `key = "secret:name"`, `token: null`
    if bare.is_empty()
        || matches!(bare, "{" | "[" | "null" | "~" | "true" | "false")
        || bare.trim_matches(['"', '\'']).starts_with("secret:")
    {
        return s.to_string();
    }
    let quote = bare.chars().next().filter(|c| matches!(c, '"' | '\'')).map(String::from).unwrap_or_default();
    let head = &s[..s.len() - value.len()];
    format!("{head}{quote}{MASK}{quote}{}", if trailing_comma { "," } else { "" })
}
