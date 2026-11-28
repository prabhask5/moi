//! Typed access to a parsed TOML document that keeps going after an error.
//!
//! serde stops at the first problem; the spec wants every problem in one run, each with `file:line:col`.
//! So the schema (`super::schema`) reads the spanned TOML tree through [`Tbl`]: every getter records the
//! key as known, reports a wrong type or bad value into [`Ctx`] and returns a usable default, and
//! [`Tbl::finish`] reports the keys nobody asked for.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::Serialize;
use toml::Spanned;
use toml::de::{DeTable, DeValue};

use super::units::{AutoOr, Dur, Secret, Size};
use crate::host::Host;

/// One problem in a config file. `line`/`col` are 1-based; 0 means the file as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub col: usize,
    /// Dotted path of the setting (`checks.resources.cpu_limit`), empty for file-level problems.
    pub key: String,
    pub message: String,
    /// Byte offset in the file, to keep diagnostics in file order.
    #[serde(skip)]
    pub(crate) offset: usize,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        if self.line > 0 {
            write!(f, ":{}:{}", self.line, self.col)?;
        }
        if self.key.is_empty() {
            write!(f, ": {}", self.message)
        } else {
            write!(f, ": {}: {}", self.key, self.message)
        }
    }
}

impl Diagnostic {
    /// A problem with the file as a whole (missing, unreadable, wrong permissions).
    pub(crate) fn file_level(file: &Path, message: String) -> Self {
        Self { file: file.to_path_buf(), line: 0, col: 0, key: String::new(), message, offset: 0 }
    }

    pub(crate) fn at(file: &Path, src: &str, offset: usize, key: &str, message: String) -> Self {
        let offset = offset.min(src.len());
        let before = &src.as_bytes()[..offset];
        let line = 1 + before.iter().filter(|b| **b == b'\n').count();
        let line_start = before.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let col = 1 + String::from_utf8_lossy(&before[line_start..]).chars().count();
        Self { file: file.to_path_buf(), line, col, key: key.to_string(), message, offset }
    }
}

/// Secrets by name: `secrets.toml` first, then `MOI_SECRET_<NAME>` from the environment.
pub(crate) struct SecretStore<'h> {
    pub file: BTreeMap<String, String>,
    pub path: PathBuf,
    pub host: &'h dyn Host,
}

impl SecretStore<'_> {
    pub fn env_name(name: &str) -> String {
        format!("MOI_SECRET_{}", name.to_ascii_uppercase())
    }

    fn get(&self, name: &str) -> Option<String> {
        self.file.get(name).cloned().or_else(|| self.host.env(&Self::env_name(name)))
    }
}

/// A value and where it was written.
#[derive(Debug, Clone)]
pub(crate) struct Sp<T> {
    pub value: T,
    pub span: Range<usize>,
}

/// Everything that accumulates while one file is read.
pub(crate) struct Ctx<'s> {
    pub file: PathBuf,
    pub src: &'s str,
    pub errors: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    pub secrets: &'s SecretStore<'s>,
    /// Secret name → value, for every reference that resolved.
    pub resolved: BTreeMap<String, String>,
}

impl Ctx<'_> {
    pub fn err(&mut self, span: &Range<usize>, key: &str, message: impl Into<String>) {
        self.errors.push(Diagnostic::at(&self.file, self.src, span.start, key, message.into()));
    }

    pub fn warn(&mut self, span: &Range<usize>, key: &str, message: impl Into<String>) {
        self.warnings.push(Diagnostic::at(&self.file, self.src, span.start, key, message.into()));
    }

    /// Resolve a `secret:NAME` reference; other strings pass through. Returns the value and whether it
    /// was a reference. A missing secret is reported and resolves to an empty string.
    fn resolve(&mut self, raw: &str, span: &Range<usize>, key: &str) -> (String, bool) {
        let Some(name) = raw.strip_prefix("secret:") else { return (raw.to_string(), false) };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            self.err(span, key, "invalid secret reference: use \"secret:NAME\" with letters, digits and _ in NAME");
            return (String::new(), true);
        }
        match self.secrets.get(name) {
            Some(value) => {
                self.resolved.insert(name.to_string(), value.clone());
                (value, true)
            }
            None => {
                let message = format!(
                    "secret `{name}` is not defined: add `{name} = \"…\"` under [secrets] in {} or set {}",
                    self.secrets.path.display(),
                    SecretStore::env_name(name)
                );
                self.err(span, key, message);
                (String::new(), true)
            }
        }
    }
}

/// A TOML table being read, or the absence of one (every getter then yields its default).
pub(crate) struct Tbl<'a, 'i> {
    table: Option<&'a DeTable<'i>>,
    pub path: String,
    pub span: Range<usize>,
    known: Vec<&'static str>,
}

type Value<'a, 'i> = &'a Spanned<DeValue<'i>>;

impl<'a, 'i> Tbl<'a, 'i> {
    pub fn root(table: &'a DeTable<'i>) -> Self {
        Self { table: Some(table), path: String::new(), span: 0..0, known: Vec::new() }
    }

    /// True if the section was written in the file.
    pub fn present(&self) -> bool {
        self.table.is_some()
    }

    pub fn key(&self, key: &str) -> String {
        if self.path.is_empty() { key.to_string() } else { format!("{}.{key}", self.path) }
    }

    fn entry(&self, key: &str) -> Option<(&'a Spanned<std::borrow::Cow<'i, str>>, Value<'a, 'i>)> {
        self.table?.iter().find(|(k, _)| k.get_ref().as_ref() == key)
    }

    fn get(&mut self, key: &'static str) -> Option<Value<'a, 'i>> {
        if !self.known.contains(&key) {
            self.known.push(key);
        }
        self.entry(key).map(|(_, v)| v)
    }

    /// True if the key was written.
    pub fn has(&mut self, key: &'static str) -> bool {
        self.get(key).is_some()
    }

    /// Where the key was written (the value), or the table itself if it was not.
    pub fn span_of(&self, key: &str) -> Range<usize> {
        self.entry(key).map_or_else(|| self.span.clone(), |(_, v)| v.span())
    }

    fn wrong_type(&self, cx: &mut Ctx<'_>, key: &str, v: Value<'_, '_>, expected: &str) {
        cx.err(&v.span(), key, format!("expected {expected}, found {}", v.get_ref().type_str()));
    }

    // ---------------------------------------------------------------- scalars

    /// A string with `secret:` references resolved; the flag says whether it was one.
    fn raw_string(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<(Sp<String>, bool)> {
        let v = self.get(key)?;
        let path = self.key(key);
        match v.get_ref() {
            DeValue::String(s) => {
                let (value, was_ref) = cx.resolve(s, &v.span(), &path);
                Some((Sp { value, span: v.span() }, was_ref))
            }
            _ => {
                self.wrong_type(cx, &path, v, "a string");
                None
            }
        }
    }

    pub fn string(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<Sp<String>> {
        self.raw_string(cx, key).map(|(s, _)| s)
    }

    pub fn string_opt(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<String> {
        self.string(cx, key).map(|s| s.value)
    }

    pub fn string_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: &str) -> String {
        self.string_opt(cx, key).unwrap_or_else(|| default.to_string())
    }

    /// A required string: reported as missing (at the table) when the table exists without it.
    pub fn string_req(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> String {
        if !self.has(key) {
            self.missing(cx, key);
        }
        self.string_opt(cx, key).unwrap_or_default()
    }

    pub fn missing(&self, cx: &mut Ctx<'_>, key: &str) {
        cx.err(&self.span, &self.key(key), "missing required key");
    }

    /// A credential. With `must_be_ref` a literal value is an error: secrets never live in `moi.toml`.
    pub fn secret(&mut self, cx: &mut Ctx<'_>, key: &'static str, must_be_ref: bool) -> Option<Secret> {
        let (s, was_ref) = self.raw_string(cx, key)?;
        if must_be_ref && !was_ref {
            cx.err(&s.span, &self.key(key), NOT_A_REFERENCE);
        }
        Some(Secret::new(s.value))
    }

    pub fn bool_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: bool) -> bool {
        let Some(v) = self.get(key) else { return default };
        match v.get_ref() {
            DeValue::Boolean(b) => *b,
            _ => {
                self.wrong_type(cx, &self.key(key), v, "true or false");
                default
            }
        }
    }

    /// An integer within `min..=max`.
    pub fn int_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: u64, min: u64, max: u64) -> u64 {
        let Some(v) = self.get(key) else { return default };
        let path = self.key(key);
        let DeValue::Integer(i) = v.get_ref() else {
            self.wrong_type(cx, &path, v, "an integer");
            return default;
        };
        match i128::from_str_radix(i.as_str(), i.radix()) {
            Ok(n) if n >= i128::from(min) && n <= i128::from(max) => u64::try_from(n).unwrap_or(default),
            _ => {
                let message = if max == u64::MAX {
                    format!("must be at least {min}")
                } else {
                    format!("must be between {min} and {max}")
                };
                cx.err(&v.span(), &path, message);
                default
            }
        }
    }

    /// A percentage limit, 1–100.
    pub fn percent_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: u32) -> u32 {
        u32::try_from(self.int_or(cx, key, u64::from(default), 1, 100)).unwrap_or(default)
    }

    pub fn u32_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: u32, min: u32) -> u32 {
        u32::try_from(self.int_or(cx, key, u64::from(default), u64::from(min), u64::from(u32::MAX))).unwrap_or(default)
    }

    /// A string parsed by `FromStr` whose error is the message (durations, sizes).
    fn parsed<T: std::str::FromStr<Err = String>>(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<T> {
        let s = self.string(cx, key)?;
        match s.value.parse() {
            Ok(v) => Some(v),
            Err(message) => {
                cx.err(&s.span, &self.key(key), message);
                None
            }
        }
    }

    pub fn dur_opt(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<Dur> {
        self.parsed(cx, key)
    }

    pub fn dur_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: Dur) -> Dur {
        self.dur_opt(cx, key).unwrap_or(default)
    }

    pub fn size_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: Size) -> Size {
        self.parsed(cx, key).unwrap_or(default)
    }

    /// A string accepted by `check`, which returns the error message otherwise.
    pub fn checked(
        &mut self,
        cx: &mut Ctx<'_>,
        key: &'static str,
        check: impl Fn(&str) -> Result<(), String>,
    ) -> Option<String> {
        let s = self.string(cx, key)?;
        if let Err(message) = check(&s.value) {
            cx.err(&s.span, &self.key(key), message);
        }
        Some(s.value)
    }

    pub fn path_opt(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<String> {
        self.checked(cx, key, absolute)
    }

    pub fn path_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: &str) -> String {
        self.path_opt(cx, key).unwrap_or_else(|| default.to_string())
    }

    // ---------------------------------------------------------------- lists and maps

    /// A list of strings, each with its position. `None` if absent or not a list.
    pub fn strings(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<Vec<Sp<String>>> {
        let v = self.get(key)?;
        let path = self.key(key);
        let DeValue::Array(items) = v.get_ref() else {
            self.wrong_type(cx, &path, v, "a list of strings");
            return None;
        };
        Some(self.string_items(cx, &path, items))
    }

    fn string_items(&self, cx: &mut Ctx<'_>, path: &str, items: &[Spanned<DeValue<'_>>]) -> Vec<Sp<String>> {
        let mut out = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let item_path = format!("{path}[{i}]");
            match item.get_ref() {
                DeValue::String(s) => {
                    let (value, _) = cx.resolve(s, &item.span(), &item_path);
                    out.push(Sp { value, span: item.span() });
                }
                _ => self.wrong_type(cx, &item_path, item, "a string"),
            }
        }
        out
    }

    pub fn strings_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: &[&str]) -> Vec<String> {
        match self.strings(cx, key) {
            Some(items) => items.into_iter().map(|s| s.value).collect(),
            None => default.iter().map(ToString::to_string).collect(),
        }
    }

    /// A list of strings each accepted by `check`.
    pub fn checked_list(
        &mut self,
        cx: &mut Ctx<'_>,
        key: &'static str,
        default: &[&str],
        check: impl Fn(&str) -> Result<(), String>,
    ) -> Vec<String> {
        let Some(items) = self.strings(cx, key) else { return default.iter().map(ToString::to_string).collect() };
        let path = self.key(key);
        for (i, item) in items.iter().enumerate() {
            if let Err(message) = check(&item.value) {
                cx.err(&item.span, &format!("{path}[{i}]"), message);
            }
        }
        items.into_iter().map(|s| s.value).collect()
    }

    /// A list of absolute paths (globs allowed: only the leading `/` is checked).
    pub fn paths_or(&mut self, cx: &mut Ctx<'_>, key: &'static str, default: &[&str]) -> Vec<String> {
        self.checked_list(cx, key, default, absolute)
    }

    /// `"auto"`, `true` or `false`: `None` is auto.
    pub fn auto_or_bool(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<bool> {
        let v = self.get(key)?;
        match v.get_ref() {
            DeValue::String(s) if s.as_ref() == "auto" => None,
            DeValue::Boolean(b) => Some(*b),
            _ => {
                cx.err(&v.span(), &self.key(key), "expected \"auto\", true or false");
                None
            }
        }
    }

    /// `"auto"` or a list of strings each accepted by `check`.
    pub fn auto_or_list(
        &mut self,
        cx: &mut Ctx<'_>,
        key: &'static str,
        check: impl Fn(&str) -> Result<(), String>,
    ) -> AutoOr<String> {
        let Some(v) = self.get(key) else { return AutoOr::Auto };
        let path = self.key(key);
        match v.get_ref() {
            DeValue::String(s) if s.as_ref() == "auto" => AutoOr::Auto,
            DeValue::Array(items) => {
                let items = self.string_items(cx, &path, items);
                for (i, item) in items.iter().enumerate() {
                    if let Err(message) = check(&item.value) {
                        cx.err(&item.span, &format!("{path}[{i}]"), message);
                    }
                }
                AutoOr::List(items.into_iter().map(|s| s.value).collect())
            }
            _ => {
                cx.err(&v.span(), &path, "expected \"auto\" or a list of strings");
                AutoOr::Auto
            }
        }
    }

    /// A table of string values (`{ "key" = "value" }`), with secret references resolved; each entry
    /// carries the position of its key, and whether its value was a reference.
    pub fn string_map(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Vec<(Sp<String>, Sp<String>, bool)> {
        let Some(v) = self.get(key) else { return Vec::new() };
        let path = self.key(key);
        let DeValue::Table(t) = v.get_ref() else {
            self.wrong_type(cx, &path, v, "a table of strings");
            return Vec::new();
        };
        let mut out = Vec::new();
        for (k, item) in t.iter() {
            let name = k.get_ref().to_string();
            let item_path = format!("{path}.{}", quote_key(&name));
            match item.get_ref() {
                DeValue::String(s) => {
                    let (value, was_ref) = cx.resolve(s, &item.span(), &item_path);
                    out.push((Sp { value: name, span: k.span() }, Sp { value, span: item.span() }, was_ref));
                }
                _ => self.wrong_type(cx, &item_path, item, "a string"),
            }
        }
        out
    }

    // ---------------------------------------------------------------- nested tables

    /// A sub-table; absent (all defaults) if the key is missing or not a table.
    pub fn table(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Tbl<'a, 'i> {
        let path = self.key(key);
        let table = self.get(key).and_then(|v| match v.get_ref() {
            DeValue::Table(t) => Some((t, header_span(cx.src, v.span()))),
            _ => {
                self.wrong_type(cx, &path, v, "a table");
                None
            }
        });
        match table {
            Some((t, span)) => Tbl { table: Some(t), path, span, known: Vec::new() },
            None => Tbl { table: None, path, span: self.span.clone(), known: Vec::new() },
        }
    }

    /// A list of tables (`[[x]]` or `x = [{…}, {…}]`).
    pub fn tables(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Vec<Tbl<'a, 'i>> {
        let Some(v) = self.get(key) else { return Vec::new() };
        let path = self.key(key);
        let DeValue::Array(items) = v.get_ref() else {
            self.wrong_type(cx, &path, v, "a list of tables");
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let item_path = format!("{path}[{i}]");
            match item.get_ref() {
                DeValue::Table(t) => out.push(Tbl {
                    table: Some(t),
                    path: item_path,
                    span: header_span(cx.src, item.span()),
                    known: Vec::new(),
                }),
                _ => self.wrong_type(cx, &item_path, item, "a table"),
            }
        }
        out
    }

    /// `"auto"` (or absent) is `None`; otherwise a list of tables.
    pub fn auto_or_tables(&mut self, cx: &mut Ctx<'_>, key: &'static str) -> Option<Vec<Tbl<'a, 'i>>> {
        let v = self.get(key)?;
        match v.get_ref() {
            DeValue::String(s) if s.as_ref() == "auto" => None,
            DeValue::Array(_) => Some(self.tables(cx, key)),
            _ => {
                cx.err(&v.span(), &self.key(key), "expected \"auto\" or a list of tables");
                None
            }
        }
    }

    /// Report every key that no getter asked for. Call after reading the whole table.
    pub fn finish(self, cx: &mut Ctx<'_>) {
        self.finish_with(cx, "");
    }

    /// Like [`Tbl::finish`], with a hint appended when there is no near-miss to suggest.
    pub fn finish_with(self, cx: &mut Ctx<'_>, hint: &str) {
        let Some(table) = self.table else { return };
        for (k, _) in table.iter() {
            let name = k.get_ref().as_ref();
            if self.known.contains(&name) {
                continue;
            }
            let message = match closest(name, &self.known) {
                Some(suggestion) => format!("unknown key (did you mean `{suggestion}`?)"),
                None if hint.is_empty() => "unknown key".to_string(),
                None => format!("unknown key ({hint})"),
            };
            cx.err(&k.span(), &self.key(&quote_key(name)), message);
        }
    }
}

pub(crate) const NOT_A_REFERENCE: &str = "must be a \"secret:NAME\" reference (secrets never live in moi.toml)";

/// `[x]` tables span their header in some parser versions and their body in others; errors about the
/// table as a whole point at the start of the line that opens it.
fn header_span(src: &str, span: Range<usize>) -> Range<usize> {
    let start = span.start.min(src.len());
    let line_start = src[..start].rfind('\n').map_or(0, |i| i + 1);
    if src[line_start..start].trim_start().starts_with('[') { line_start..span.end } else { span }
}

fn quote_key(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        name.to_string()
    } else {
        format!("\"{name}\"")
    }
}

pub(crate) fn absolute(path: &str) -> Result<(), String> {
    if path.starts_with('/') { Ok(()) } else { Err("must be an absolute path".to_string()) }
}

/// The known key closest to a typo, if it is close enough to be a plausible slip.
fn closest<'k>(name: &str, known: &[&'k str]) -> Option<&'k str> {
    known
        .iter()
        .map(|k| (edit_distance(name, k), *k))
        .filter(|(d, k)| *d <= 2 && *d * 3 <= k.len().max(name.len()))
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Levenshtein distance with adjacent transpositions counted once (`nmae` → `name` is 1).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1).min(rows[i][j - 1] + 1).min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}
