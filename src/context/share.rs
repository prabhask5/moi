//! `moi context share`: an expiring link to CONTEXT.md, for pasting into an AI chat without publishing
//! anything.
//!
//! Spec: docs/spec/03-context.md § Sharing. The file is uploaded to a *private* S3-compatible bucket
//! under a random 128-bit key; the link is an S3 presigned GET URL, computed locally. Deleting the
//! object kills the link before it expires. Only CONTEXT.md — already redacted — is ever uploaded.

use std::sync::Arc;

use md5::Md5;
use sha2::{Digest, Sha256};

use crate::config::{Config, ContextShare, Dur};
use crate::error::Error;
use crate::host::{self, Host};
use crate::net::{self, Client, Response};
use crate::redact::Masker;
use crate::state::StateDir;
use crate::state::db::Db;

/// Where shared copies live in the bucket.
const PREFIX: &str = "ctx/";
/// S3 refuses presigned URLs valid for longer than a week.
const S3_MAX_TTL: u64 = 7 * 86_400;
/// The lifecycle rule moi adds as a safety net.
const LIFECYCLE_RULE: &str = "moi-ctx-expire";

fn fail(message: impl Into<String>) -> Error {
    Error::Share(message.into())
}

// ---------------------------------------------------------------------------------------------- SigV4

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Credentials and place of an AWS Signature Version 4 signature.
#[derive(Clone)]
pub struct Signer<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

impl std::fmt::Debug for Signer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never the keys
        write!(f, "Signer({}, {})", self.region, self.amz_date)
    }
}

impl Signer<'_> {
    fn scope(&self) -> String {
        format!("{}/{}/s3/aws4_request", &self.amz_date[..8], self.region)
    }

    fn signature(&self, canonical_request: &str) -> String {
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            self.amz_date,
            self.scope(),
            sha256_hex(canonical_request.as_bytes())
        );
        let date = hmac_sha256(format!("AWS4{}", self.secret_key).as_bytes(), &self.amz_date.as_bytes()[..8]);
        let region = hmac_sha256(&date, self.region.as_bytes());
        let service = hmac_sha256(&region, b"s3");
        let signing = hmac_sha256(&service, b"aws4_request");
        hex(&hmac_sha256(&signing, string_to_sign.as_bytes()))
    }

    /// The query string of a presigned GET for `path` on `host`, valid for `expires` seconds.
    pub fn presign_get(&self, host: &str, path: &str, expires: u64) -> String {
        let query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={}&X-Amz-Expires={expires}&X-Amz-SignedHeaders=host",
            net::encode(&format!("{}/{}", self.access_key, self.scope())),
            self.amz_date
        );
        let canonical = format!("GET\n{path}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
        format!("{query}&X-Amz-Signature={}", self.signature(&canonical))
    }

    /// The `Authorization` header for a request. `headers` are the lower-cased names and values to sign
    /// (besides `host`); `query` must already be in canonical form (sorted, encoded).
    pub fn authorization(
        &self,
        method: &str,
        host: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
        payload_sha256: &str,
    ) -> String {
        let mut signed: Vec<(String, String)> =
            headers.iter().map(|(k, v)| (k.to_lowercase(), v.trim().to_string())).collect();
        signed.push(("host".to_string(), host.to_string()));
        signed.sort();
        let names: Vec<&str> = signed.iter().map(|(k, _)| k.as_str()).collect();
        let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let canonical =
            format!("{method}\n{path}\n{query}\n{canonical_headers}\n{}\n{payload_sha256}", names.join(";"));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{},SignedHeaders={},Signature={}",
            self.access_key,
            self.scope(),
            names.join(";"),
            self.signature(&canonical)
        )
    }
}

/// `2026-11-26T17:00:00Z` → `20261126T170000Z`.
fn amz_date(now: std::time::SystemTime) -> String {
    host::format_rfc3339(now).replace(['-', ':'], "")
}

// ---------------------------------------------------------------------------------------------- bucket

/// The share bucket.
pub struct Bucket<'a> {
    client: Client,
    host: Arc<dyn Host>,
    share: &'a ContextShare,
    endpoint: String,
}

impl std::fmt::Debug for Bucket<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bucket({})", self.share.bucket)
    }
}

impl<'a> Bucket<'a> {
    pub fn new(host: &Arc<dyn Host>, share: &'a ContextShare) -> Result<Self, Error> {
        if !share.enabled {
            return Err(fail(
                "not configured: add a [context.share] section to moi.toml (endpoint, bucket, access_key_id, secret_access_key)",
            ));
        }
        let endpoint =
            share.endpoint.as_ref().map(|e| e.expose().trim_end_matches('/').to_string()).unwrap_or_default();
        Ok(Self { client: Client::new(Arc::clone(host)), host: Arc::clone(host), share, endpoint })
    }

    fn credentials(&self) -> (&str, &str) {
        (
            self.share.access_key_id.as_ref().map_or("", |s| s.expose()),
            self.share.secret_access_key.as_ref().map_or("", |s| s.expose()),
        )
    }

    /// `/bucket/key`, each segment percent-encoded.
    fn path(&self, key: &str) -> String {
        let mut path = format!("/{}", net::encode(&self.share.bucket));
        for segment in key.split('/').filter(|s| !s.is_empty()) {
            path.push('/');
            path.push_str(&net::encode(segment));
        }
        path
    }

    /// A signed request. `query` is canonical (sorted `name=value` pairs, values encoded; a sub-resource
    /// such as `lifecycle` is written `lifecycle=`).
    pub fn request(
        &self,
        method: &str,
        key: &str,
        query: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Response, Error> {
        let url = net::Url::parse(&self.endpoint).map_err(fail)?;
        let date = amz_date(self.host.now());
        let (access_key, secret_key) = self.credentials();
        let signer = Signer { access_key, secret_key, region: &self.share.region, amz_date: &date };
        let payload = sha256_hex(body);
        let mut signed: Vec<(String, String)> = headers.to_vec();
        signed.push(("x-amz-content-sha256".to_string(), payload.clone()));
        signed.push(("x-amz-date".to_string(), date.clone()));
        let path = self.path(key);
        let authorization = signer.authorization(method, &url.authority(), &path, query, &signed, &payload);
        signed.push(("Authorization".to_string(), authorization));
        let target = if query.is_empty() { path } else { format!("{path}?{query}") };
        let full = format!("{}{target}", self.endpoint);
        self.client
            .send(method, &full, &signed, (!body.is_empty() || method == "PUT").then_some(body))
            .map_err(|e| fail(format!("cannot reach the share bucket: {e}")))
    }

    fn expect(&self, what: &str, response: Response) -> Result<Response, Error> {
        if response.is_success() {
            return Ok(response);
        }
        // S3 errors are XML with a <Code> and a <Message>
        let body = response.text();
        let tag = |name: &str| {
            body.split(&format!("<{name}>"))
                .nth(1)
                .and_then(|rest| rest.split(&format!("</{name}>")).next())
                .unwrap_or_default()
        };
        Err(fail(
            format!("{what} failed: HTTP {} {} {}", response.status, tag("Code"), tag("Message"))
                .trim_end()
                .to_string(),
        ))
    }

    pub fn put(&self, key: &str, content: &[u8]) -> Result<(), Error> {
        let headers = [
            ("cache-control".to_string(), "no-store".to_string()),
            ("content-type".to_string(), "text/markdown; charset=utf-8".to_string()),
        ];
        self.expect("upload", self.request("PUT", key, "", &headers, content)?).map(|_| ())
    }

    pub fn delete(&self, key: &str) -> Result<(), Error> {
        self.expect("delete", self.request("DELETE", key, "", &[], b"")?).map(|_| ())
    }

    /// Every key under `ctx/`.
    pub fn list(&self) -> Result<Vec<String>, Error> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = String::new();
            if let Some(token) = &token {
                query.push_str(&format!("continuation-token={}&", net::encode(token)));
            }
            query.push_str(&format!("list-type=2&prefix={}", net::encode(PREFIX)));
            let body = self.expect("listing shares", self.request("GET", "", &query, &[], b"")?)?.text();
            for part in body.split("<Key>").skip(1) {
                if let Some(key) = part.split("</Key>").next() {
                    keys.push(key.replace("&amp;", "&"));
                }
            }
            token = body
                .split("<NextContinuationToken>")
                .nth(1)
                .and_then(|rest| rest.split("</NextContinuationToken>").next())
                .map(str::to_string);
            if token.is_none() || !body.contains("<IsTruncated>true</IsTruncated>") {
                return Ok(keys);
            }
        }
    }

    /// Make the bucket delete `ctx/` objects after a day, unless it already has lifecycle rules of its
    /// own (those are never replaced). Returns what was done, for the log.
    pub fn ensure_lifecycle(&self) -> Result<&'static str, Error> {
        let current = self.request("GET", "", "lifecycle=", &[], b"")?;
        if current.is_success() {
            let body = current.text();
            if body.contains(LIFECYCLE_RULE) {
                return Ok("already present");
            }
            if body.contains("<Rule>") {
                return Ok("skipped: the bucket has lifecycle rules of its own");
            }
        } else if current.status != 404 {
            self.expect("reading the bucket's lifecycle rules", current)?;
        }
        let xml = format!(
            "<LifecycleConfiguration><Rule><ID>{LIFECYCLE_RULE}</ID><Filter><Prefix>{PREFIX}</Prefix></Filter><Status>Enabled</Status><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>"
        );
        let headers = [("content-md5".to_string(), base64(&Md5::digest(xml.as_bytes())))];
        self.expect("adding the lifecycle rule", self.request("PUT", "", "lifecycle=", &headers, xml.as_bytes())?)?;
        Ok("added")
    }

    /// A link to `key` that works for `ttl` seconds from now.
    pub fn presigned_url(&self, key: &str, ttl: u64) -> Result<String, Error> {
        let url = net::Url::parse(&self.endpoint).map_err(fail)?;
        let date = amz_date(self.host.now());
        let (access_key, secret_key) = self.credentials();
        let signer = Signer { access_key, secret_key, region: &self.share.region, amz_date: &date };
        let path = self.path(key);
        Ok(format!("{}{path}?{}", self.endpoint, signer.presign_get(&url.authority(), &path, ttl)))
    }
}

// ---------------------------------------------------------------------------------------------- commands

/// A link that was just created.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Shared {
    pub url: String,
    pub key: String,
    /// RFC 3339, UTC.
    pub expires: String,
    pub ttl: String,
    /// What happened to the bucket's safety-net lifecycle rule on this run (`added`, …), if it was looked at.
    pub lifecycle: Option<String>,
}

/// A share that has not expired.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Active {
    pub key: String,
    pub created: String,
    pub expires: String,
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Db(e.to_string())
}

fn rfc3339(seconds: i64) -> String {
    host::format_rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(seconds).unwrap_or(0)))
}

/// Delete the objects of shares whose time is up. Returns how many were removed.
pub fn expire(host: &Arc<dyn Host>, config: &Config, db: &Db) -> Result<usize, Error> {
    let now = host::unix_seconds(host.now());
    let expired: Vec<String> = db
        .conn()
        .prepare("SELECT key FROM context_shares WHERE expires_at <= ?1")
        .and_then(|mut s| s.query_map([now], |r| r.get(0)).map(|rows| rows.flatten().collect()))
        .map_err(db_err)?;
    if expired.is_empty() {
        return Ok(0);
    }
    let bucket = Bucket::new(host, &config.context.share)?;
    for key in &expired {
        bucket.delete(key)?;
        db.conn().execute("DELETE FROM context_shares WHERE key = ?1", [key]).map_err(db_err)?;
    }
    Ok(expired.len())
}

/// Upload the current CONTEXT.md and return a link valid for `ttl` (default `default_ttl`).
pub fn share(
    host: &Arc<dyn Host>,
    config: &Config,
    state: &StateDir,
    db: &Db,
    masker: &Masker,
    ttl: Option<Dur>,
) -> Result<Shared, Error> {
    let settings = &config.context.share;
    let bucket = Bucket::new(host, settings)?;
    let ttl = ttl.unwrap_or(settings.default_ttl);
    if ttl > settings.max_ttl {
        return Err(Error::Usage(format!(
            "--ttl {ttl} is longer than context.share.max_ttl ({}): links must stay short-lived",
            settings.max_ttl
        )));
    }
    if ttl.as_secs() == 0 || ttl.as_secs() > S3_MAX_TTL {
        return Err(Error::Usage(format!("--ttl {ttl} is not possible: a link lives between 1s and 7d")));
    }
    expire(host, config, db)?;

    // always the current description, masked like every other copy of it
    let written = super::write(host.as_ref(), config, state, masker)?;
    let content =
        host.read(&written.path).map_err(|e| Error::host(format!("cannot read {}", written.path.display()), e))?;

    let lifecycle_done: bool = db
        .conn()
        .query_row("SELECT 1 FROM settings WHERE key = 'context_share_lifecycle'", [], |_| Ok(true))
        .unwrap_or(false);
    let mut lifecycle = None;
    if !lifecycle_done {
        // a safety net only: failing to set it must not stop a share whose object moi deletes itself
        let outcome = bucket.ensure_lifecycle().map_or_else(|e| format!("not set ({e})"), str::to_string);
        if !outcome.starts_with("not set") {
            db.conn()
                .execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES ('context_share_lifecycle', ?1)",
                    [&outcome],
                )
                .map_err(db_err)?;
        }
        lifecycle = Some(outcome);
    }

    let random = host.random_bytes(16).map_err(|e| Error::host("cannot get random bytes for the link", e))?;
    let key = format!("{PREFIX}{}.md", hex(&random));
    bucket.put(&key, &content)?;
    let now = host::unix_seconds(host.now());
    let expires_at = now + i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
    db.conn()
        .execute(
            "INSERT OR REPLACE INTO context_shares (key, created_at, expires_at) VALUES (?1, ?2, ?3)",
            (&key, now, expires_at),
        )
        .map_err(db_err)?;
    Ok(Shared {
        url: bucket.presigned_url(&key, ttl.as_secs())?,
        key,
        expires: rfc3339(expires_at),
        ttl: ttl.to_string(),
        lifecycle,
    })
}

/// Shares that are still valid (expired ones are deleted first).
pub fn list(host: &Arc<dyn Host>, config: &Config, db: &Db) -> Result<Vec<Active>, Error> {
    Bucket::new(host, &config.context.share)?;
    expire(host, config, db)?;
    let mut stmt = db
        .conn()
        .prepare("SELECT key, created_at, expires_at FROM context_shares ORDER BY expires_at")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |r| Ok(Active { key: r.get(0)?, created: rfc3339(r.get(1)?), expires: rfc3339(r.get(2)?) }))
        .map_err(db_err)?;
    Ok(rows.flatten().collect())
}

/// Delete every shared copy in the bucket — also ones this state dir does not know about. Every link
/// stops working at once. Returns how many objects were deleted.
pub fn revoke(host: &Arc<dyn Host>, config: &Config, db: &Db) -> Result<usize, Error> {
    let bucket = Bucket::new(host, &config.context.share)?;
    let keys = bucket.list()?;
    for key in &keys {
        bucket.delete(key)?;
    }
    db.conn().execute("DELETE FROM context_shares", []).map_err(db_err)?;
    Ok(keys.len())
}
