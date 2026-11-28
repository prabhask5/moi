//! Configuration: `moi.toml` + `secrets.toml`, defaults, validation, `secret:NAME` references.
//!
//! Spec: docs/spec/01-config.md. [`load`] reads both files through a [`Host`], reports *every* problem
//! with `file:line:col` in one [`Error::ConfigInvalid`], and returns the effective [`Config`] together
//! with a [`Masker`] for every secret value it saw.

mod reader;
mod schema;
mod units;

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use toml::de::{DeTable, DeValue};

pub use reader::Diagnostic;
use reader::{Ctx, SecretStore, Tbl};
pub use schema::*;
pub use units::{AutoOr, Dur, Secret, Size};

use crate::error::Error;
use crate::host::Host;
use crate::redact::{MASK, Masker};

/// Where the two config files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub secrets: PathBuf,
}

impl Paths {
    /// `secrets` defaults to `secrets.toml` next to the config file.
    pub fn new(config: PathBuf, secrets: Option<PathBuf>) -> Self {
        let secrets = secrets.unwrap_or_else(|| config.with_file_name("secrets.toml"));
        Self { config, secrets }
    }
}

/// A validated configuration.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub paths: Paths,
    /// Knows every secret value from `secrets.toml` and every `MOI_SECRET_*` that was used.
    pub masker: Masker,
    /// Things that are allowed but probably not intended.
    pub warnings: Vec<Diagnostic>,
    /// Number of distinct secrets the config refers to.
    pub secrets_resolved: usize,
}

/// Read and validate `moi.toml` and `secrets.toml`.
pub fn load(host: &dyn Host, paths: &Paths) -> Result<Loaded, Error> {
    let mut errors = Vec::new();
    let store = read_secrets(host, &paths.secrets, &mut errors);

    let src = match host.read_to_string(&paths.config) {
        Ok(src) => src,
        Err(e) => {
            let message = if e.kind() == ErrorKind::NotFound {
                "not found: run `moi init` to create it, or pass --config".to_string()
            } else {
                format!("cannot read: {e}")
            };
            errors.insert(0, Diagnostic::file_level(&paths.config, message));
            return Err(Error::ConfigInvalid(errors));
        }
    };

    let (doc, syntax) = DeTable::parse_recoverable(&src);
    let mut cx = Ctx {
        file: paths.config.clone(),
        src: &src,
        errors: Vec::new(),
        warnings: Vec::new(),
        secrets: &store,
        resolved: BTreeMap::new(),
    };
    for e in &syntax {
        cx.err(&e.span().unwrap_or(0..0), "", e.message());
    }
    // A document that does not parse is not checked against the schema: half a table would only
    // produce misleading follow-up errors.
    let config = syntax.is_empty().then(|| {
        let mut root = Tbl::root(doc.get_ref());
        let config = Config::read(&mut root, &mut cx, host);
        root.finish(&mut cx);
        config
    });

    let Ctx { errors: mut config_errors, mut warnings, resolved, .. } = cx;
    config_errors.sort_by_key(|d| d.offset);
    warnings.sort_by_key(|d| d.offset);
    config_errors.append(&mut errors);
    match config {
        Some(config) if config_errors.is_empty() => Ok(Loaded {
            config,
            paths: paths.clone(),
            masker: Masker::new(store.file.values().cloned().chain(resolved.values().cloned())),
            warnings,
            secrets_resolved: resolved.len(),
        }),
        _ => Err(Error::ConfigInvalid(config_errors)),
    }
}

/// One secret by name, from `secrets.toml` or `MOI_SECRET_<NAME>`: for a command that has to work
/// before the section that would refer to the secret is written (`moi ssh up`).
pub fn secret_named(host: &dyn Host, paths: &Paths, name: &str) -> Option<Secret> {
    let store = read_secrets(host, &paths.secrets, &mut Vec::new());
    store.file.get(name).cloned().or_else(|| host.env(&SecretStore::env_name(name))).map(Secret::new)
}

/// `secrets.toml`: a single `[secrets]` table of strings, readable by its owner only. A missing file is
/// fine (secrets may all come from the environment).
fn read_secrets<'h>(host: &'h dyn Host, path: &Path, errors: &mut Vec<Diagnostic>) -> SecretStore<'h> {
    let mut store = SecretStore { file: BTreeMap::new(), path: path.to_path_buf(), host };
    let src = match host.read_to_string(path) {
        Ok(src) => src,
        Err(e) if e.kind() == ErrorKind::NotFound => return store,
        Err(e) => {
            errors.push(Diagnostic::file_level(path, format!("cannot read: {e}")));
            return store;
        }
    };
    if let Ok(md) = host.metadata(path)
        && md.mode & 0o077 != 0
    {
        let message =
            format!("mode {:04o} lets other users read the secrets: run `chmod 600 {}`", md.mode, path.display());
        errors.push(Diagnostic::file_level(path, message));
    }
    let (doc, syntax) = DeTable::parse_recoverable(&src);
    let mut found = Vec::new();
    for e in &syntax {
        found.push(Diagnostic::at(path, &src, e.span().map_or(0, |s| s.start), "", e.message().to_string()));
    }
    if syntax.is_empty() {
        for (key, value) in doc.get_ref().iter() {
            let name = key.get_ref().as_ref();
            let table = match value.get_ref() {
                DeValue::Table(t) if name == "secrets" => t,
                _ => {
                    let message = "unknown key (secrets go under [secrets])".to_string();
                    found.push(Diagnostic::at(path, &src, key.span().start, name, message));
                    continue;
                }
            };
            for (k, v) in table.iter() {
                let secret = k.get_ref().as_ref();
                let key_path = format!("secrets.{secret}");
                let at = |offset: usize, message: String| Diagnostic::at(path, &src, offset, &key_path, message);
                if secret.is_empty() || !secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    found.push(at(k.span().start, "secret names use letters, digits and _ only".to_string()));
                }
                match v.get_ref() {
                    DeValue::String(s) => {
                        store.file.insert(secret.to_string(), s.to_string());
                    }
                    other => {
                        found.push(at(v.span().start, format!("expected a string, found {}", other.type_str())));
                    }
                }
            }
        }
    }
    found.sort_by_key(|d| d.offset);
    errors.append(&mut found);
    store
}

impl Loaded {
    /// The effective configuration as JSON, every secret masked. Settings without a value are left out.
    pub fn effective_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(&self.config).unwrap_or(serde_json::Value::Null);
        self.mask_tree(&mut value);
        value
    }

    /// The effective configuration as TOML in `moi.toml` syntax, every default spelled out and every
    /// secret shown as `***`.
    pub fn effective_toml(&self) -> String {
        let value = json_to_toml(self.effective_json());
        toml::to_string_pretty(&value).unwrap_or_default()
    }

    /// Mask secrets in every string and drop nulls (TOML has none).
    fn mask_tree(&self, value: &mut serde_json::Value) {
        use serde_json::Value;
        match value {
            Value::String(s) => {
                let masked = self.masker.mask(s);
                if masked != *s {
                    *s = if self.masker.is_secret(s) { MASK.to_string() } else { masked };
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.mask_tree(v)),
            Value::Object(map) => {
                map.retain(|_, v| !v.is_null());
                map.values_mut().for_each(|v| self.mask_tree(v));
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

fn json_to_toml(value: serde_json::Value) -> toml::Value {
    use serde_json::Value;
    match value {
        Value::Null => toml::Value::String(String::new()),
        Value::Bool(b) => toml::Value::Boolean(b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => toml::Value::Integer(i),
            None => toml::Value::Float(n.as_f64().unwrap_or_default()),
        },
        Value::String(s) => toml::Value::String(s),
        Value::Array(items) => toml::Value::Array(items.into_iter().map(json_to_toml).collect()),
        Value::Object(map) => toml::Value::Table(map.into_iter().map(|(k, v)| (k, json_to_toml(v))).collect()),
    }
}

const REFERENCE_HEADER: &str = "\
# Every moi.toml setting that has a default, with that default: the effective configuration of an
# empty moi.toml on a server called \"myserver\". `moi config check` prints the same for your own file.
#
# Generated from the code and checked by a test (MOI_UPDATE_GOLDEN=1 cargo test --test examples).
# Reference: docs/spec/01-config.md. Short starting point: examples/minimal.toml.
#
# A feature is off until its section appears in moi.toml: writing `[checks.resources]` is enough to
# switch that check on with the values below (hence `enabled = false` here, in a file with no sections
# of its own). Settings without a default (monitor names, backup.local_dir, monitors.kuma.url,
# credentials, …) are not listed.

";

/// The text of `examples/moi.toml` for an effective configuration (of an empty `moi.toml`).
pub fn reference_toml(defaults: &Loaded) -> String {
    format!("{REFERENCE_HEADER}{}", defaults.effective_toml())
}
