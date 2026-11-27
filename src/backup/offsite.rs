//! The off-site copy: the same data into a restic repository — encrypted, deduplicated — behind the
//! guards `offsite-backup.sh` had: at most one upload per interval, real usage of the R2 account
//! checked first (and no upload if it cannot be read), a ceiling on the repository's size.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{Config, Offsite};
use crate::daemon::zone::Zone;
use crate::error::Error;
use crate::host::{self, Cmd, CmdOutput, Host};
use crate::net::Client;
use crate::state::StateDir;
use crate::state::db::Db;

const GB: u64 = 1 << 30;
const GRAPHQL_URL: &str = "https://api.cloudflare.com/client/v4/graphql";
/// Operations Cloudflare bills as Class A …
const CLASS_A: [&str; 16] = [
    "ListBuckets",
    "PutBucket",
    "ListObjects",
    "ListObjectsV2",
    "PutObject",
    "CopyObject",
    "CreateMultipartUpload",
    "UploadPart",
    "UploadPartCopy",
    "CompleteMultipartUpload",
    "ListMultipartUploads",
    "ListParts",
    "PutBucketEncryption",
    "PutBucketCors",
    "PutBucketLifecycleConfiguration",
    "LifecycleStorageTierTransition",
];
/// … and the ones that are free. Everything else is Class B.
const FREE: [&str; 4] = ["DeleteObject", "DeleteObjects", "DeleteBucket", "AbortMultipartUpload"];
/// The query of `offsite-backup.sh`, verbatim.
const USAGE_QUERY: &str = "query($a:String!,$s:Time!,$e:Time!){viewer{accounts(filter:{accountTag:$a}){
     ops:r2OperationsAdaptiveGroups(limit:10000,filter:{datetime_geq:$s,datetime_leq:$e}){sum{requests} dimensions{actionType}}
     sto:r2StorageAdaptiveGroups(limit:10000,filter:{datetime_geq:$s,datetime_leq:$e}){max{payloadSize metadataSize} dimensions{bucketName}}
  }}}";
pub const BUNDLE_TAG: &str = "moi-bundle";

/// restic, pointed at the configured repository.
#[derive(Debug, Clone)]
pub struct Restic {
    host: Arc<dyn Host>,
    /// Repository, password and the cloud credentials: handed to restic in its environment only.
    env: Vec<(String, String)>,
}

impl Restic {
    pub fn new(host: &Arc<dyn Host>, offsite: &Offsite) -> Result<Self, Error> {
        let (Some(repository), Some(password)) = (&offsite.repository, &offsite.password) else {
            return Err(Error::Usage(
                "off-site backup is not configured: [backup.offsite] needs repository and password".to_string(),
            ));
        };
        let mut env = vec![
            ("RESTIC_REPOSITORY".to_string(), repository.expose().to_string()),
            ("RESTIC_PASSWORD".to_string(), password.expose().to_string()),
        ];
        env.extend(offsite.env.iter().map(|(key, value)| (key.clone(), value.expose().to_string())));
        Ok(Self { host: Arc::clone(host), env })
    }

    /// With an environment given directly (pulling on a machine that has no configuration yet).
    pub fn with_env(host: &Arc<dyn Host>, env: Vec<(String, String)>) -> Self {
        Self { host: Arc::clone(host), env }
    }

    /// Run `restic <args>`. Uploads can take hours: no timeout unless one is given.
    pub fn run(&self, args: &[&str], timeout: Option<Duration>) -> std::io::Result<CmdOutput> {
        let mut cmd = Cmd::new("restic").args(args.iter().copied());
        for (key, value) in &self.env {
            cmd = cmd.env(key, value);
        }
        if let Some(timeout) = timeout {
            cmd = cmd.timeout(timeout);
        }
        self.host.run(&cmd)
    }

    pub fn installed(&self) -> bool {
        self.host
            .run(&Cmd::new("restic").arg("version").timeout(Duration::from_secs(20)))
            .is_ok_and(|out| out.status == 0)
    }
}

fn setting(db: &Db, key: &str) -> Option<String> {
    db.conn().query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).ok()
}

fn set(db: &Db, key: &str, value: &str) {
    let _ = db.conn().execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    );
}

/// Real usage of the whole R2 account over the last 30 days: (Class A operations, Class B, bytes stored).
pub fn r2_usage(host: &Arc<dyn Host>, account: &str, token: &str) -> Result<(u64, u64, u64), String> {
    let now = host.now();
    let since = now - Duration::from_secs(30 * 86_400);
    let body = json!({ "query": USAGE_QUERY, "variables": { "a": account, "s": host::format_rfc3339(since), "e": host::format_rfc3339(now) } }).to_string();
    let url = host.env("MOI_CF_GRAPHQL_URL").unwrap_or_else(|| GRAPHQL_URL.to_string());
    let headers = [
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    let unreadable = || "cannot read R2 usage from Cloudflare (refusing to upload)".to_string();
    let response = Client::new(Arc::clone(host))
        .with_timeout(Duration::from_secs(30))
        .send("POST", &url, &headers, Some(body.as_bytes()))
        .map_err(|_| unreadable())?;
    if !response.is_success() {
        return Err(unreadable());
    }
    let answer: Value = serde_json::from_slice(&response.body).map_err(|_| unreadable())?;
    let account = &answer["data"]["viewer"]["accounts"][0];
    if !answer["errors"].is_null() || account.is_null() {
        let errors: String = answer["errors"].to_string().chars().take(300).collect();
        return Err(format!("R2 usage query failed (refusing to upload): {errors}"));
    }
    let (mut class_a, mut class_b) = (0u64, 0u64);
    for group in account["ops"].as_array().into_iter().flatten() {
        let action = group["dimensions"]["actionType"].as_str().unwrap_or_default();
        let requests = group["sum"]["requests"].as_u64().unwrap_or(0);
        if CLASS_A.contains(&action) {
            class_a += requests;
        } else if !FREE.contains(&action) {
            class_b += requests;
        }
    }
    // per bucket the largest size seen in the period; the account stores their sum
    let mut buckets: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for group in account["sto"].as_array().into_iter().flatten() {
        let size =
            group["max"]["payloadSize"].as_u64().unwrap_or(0) + group["max"]["metadataSize"].as_u64().unwrap_or(0);
        let bucket =
            buckets.entry(group["dimensions"]["bucketName"].as_str().unwrap_or_default().to_string()).or_insert(0);
        *bucket = (*bucket).max(size);
    }
    Ok((class_a, class_b, buckets.values().sum()))
}

/// The off-site step failed; why is in the log already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failed;

/// What the upload needs to know about the run.
#[derive(Debug, Clone)]
pub struct Upload {
    /// The run's dump directory, and every path of the identity set.
    pub paths: Vec<String>,
    /// Paths and patterns restic must skip.
    pub excludes: Vec<String>,
}

/// The off-site step of a backup run. Lines go to `log` (prefixed by the caller); `Err` means the run
/// has failed, with the error already logged.
pub fn run(
    host: &Arc<dyn Host>,
    config: &Config,
    state: &StateDir,
    db: &Db,
    upload: &Upload,
    log: &mut dyn FnMut(&str),
) -> Result<(), Failed> {
    let offsite = &config.backup.offsite;
    let fail = |log: &mut dyn FnMut(&str), message: &str| -> Result<(), Failed> {
        log(&format!("ERROR: {message}"));
        Err(Failed)
    };
    let restic = match Restic::new(host, offsite) {
        Ok(restic) => restic,
        Err(e) => return fail(log, &e.to_string()),
    };
    if !restic.installed() {
        return fail(log, "restic not installed");
    }
    let now = host::unix_seconds(host.now());
    // ---- 1. never more than one upload per interval, whatever calls this
    let min_hours = offsite.min_interval.as_secs() / 3600;
    if let Some(last) = setting(db, "offsite.last_upload").and_then(|v| v.parse::<i64>().ok()) {
        let age = (now - last).max(0) / 3600;
        if (now - last) < i64::try_from(offsite.min_interval.as_secs()).unwrap_or(i64::MAX) {
            log(&format!("skipped: last upload was {age}h ago (minimum {min_hours}h)"));
            return Ok(());
        }
    }
    // ---- 2. the free tier: real usage of the account, and no upload if it cannot be read
    let guard = &offsite.r2_guard;
    if guard.enabled {
        let (Some(account), Some(token)) = (&guard.account_id, &guard.api_token) else {
            return fail(log, "the R2 guard has no account_id or api_token (refusing to upload)");
        };
        match r2_usage(host, account.expose(), token.expose()) {
            Err(why) => return fail(log, &why),
            Ok((class_a, class_b, stored)) => {
                let (storage_gb, max_gb) = (stored / GB, guard.max_storage.bytes() / GB);
                log(&format!(
                    "R2 usage, last 30 days: Class A {class_a}/{}, Class B {class_b}/{}, storage {storage_gb}/{max_gb} GB",
                    guard.max_class_a, guard.max_class_b
                ));
                if class_a >= guard.max_class_a || class_b >= guard.max_class_b || stored >= guard.max_storage.bytes() {
                    return fail(log, "R2 usage past the safety limit, refusing to upload (check the R2 dashboard)");
                }
            }
        }
    }
    // ---- 3. upload: the raw paths, so only what changed is sent
    let exclude_file: PathBuf = state.root().join("staging/restic-excludes");
    let listed = upload.excludes.iter().map(|e| format!("{e}\n")).collect::<String>();
    if host.create_dir_all(&state.root().join("staging"), 0o700).is_err()
        || host.write(&exclude_file, listed.as_bytes(), 0o600).is_err()
    {
        return fail(log, &format!("cannot write {}", exclude_file.display()));
    }
    let _ = restic.run(&["unlock", "--quiet"], Some(Duration::from_secs(120)));
    let pack = (offsite.pack_size.bytes() / (1 << 20)).max(1).to_string();
    let exclude_arg = exclude_file.to_string_lossy().into_owned();
    let mut args = vec![
        "backup",
        "--quiet",
        "--tag",
        offsite.tag.as_str(),
        "--pack-size",
        pack.as_str(),
        "--exclude-file",
        exclude_arg.as_str(),
        "--exclude-caches",
    ];
    args.extend(upload.paths.iter().map(String::as_str));
    let mut attempt = 1;
    let code = loop {
        log(&format!("uploading {} paths (attempt {attempt})", upload.paths.len()));
        let code = restic.run(&args, None).map_or(127, |out| out.status);
        if code == 0 || code == 3 {
            break code;
        }
        if attempt >= 2 {
            return fail(log, &format!("restic backup failed ({code})"));
        }
        attempt += 1;
        log(&format!("backup failed ({code}), retrying in 10 minutes"));
        host.sleep(Duration::from_secs(600));
    };
    if code == 3 {
        log("warning: some files could not be read (snapshot still saved)");
    }
    set(db, "offsite.last_upload", &host::unix_seconds(host.now()).to_string());
    // ---- 4. retention: forgetting is cheap; pruning rewrites data and happens once a week
    let keep = &offsite.keep;
    let (daily, weekly, monthly) = (keep.daily.to_string(), keep.weekly.to_string(), keep.monthly.to_string());
    let mut forget = vec![
        "forget",
        "--quiet",
        "--tag",
        offsite.tag.as_str(),
        "--keep-daily",
        daily.as_str(),
        "--keep-weekly",
        weekly.as_str(),
        "--keep-monthly",
        monthly.as_str(),
    ];
    let zone = Zone::load(host.as_ref());
    // 1970-01-01 was a Thursday
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]
        [usize::try_from(zone.to_local(now).div_euclid(86_400).rem_euclid(7)).unwrap_or(0)];
    let ok = |out: std::io::Result<CmdOutput>| out.is_ok_and(|o| o.status == 0);
    if offsite.prune_on.get(..3).is_some_and(|day| day.eq_ignore_ascii_case(weekday)) {
        log("retention + prune");
        forget.push("--prune");
        if !ok(restic.run(&forget, None)) {
            return fail(log, "restic forget/prune failed");
        }
        log(&format!("integrity check (re-reads {} of the data)", offsite.check_subset));
        let subset = format!("--read-data-subset={}", offsite.check_subset);
        if !ok(restic.run(&["check", "--quiet", subset.as_str()], None)) {
            return fail(log, "restic check failed");
        }
    } else if !ok(restic.run(&forget, None)) {
        return fail(log, "restic forget failed");
    }
    // ---- 5. the repository must stay under its ceiling
    let stats = restic.run(&["stats", "--quiet", "--mode", "raw-data", "--json"], Some(Duration::from_secs(600)));
    let bytes = stats
        .ok()
        .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok())
        .and_then(|v| v["total_size"].as_u64())
        .unwrap_or(0);
    log(&format!("repository size: {} MB (limit {} GB)", bytes / (1 << 20), offsite.max_repo.bytes() / GB));
    if bytes >= offsite.max_repo.bytes() {
        return fail(log, "repository near the free tier");
    }
    // ---- 6. what rebuilding the server starts from
    match crate::bundle::push(host, config, state, db, true) {
        Ok(_) => log("state bundle saved"),
        Err(e) => return fail(log, &format!("state bundle not saved: {e}")),
    }
    log("done");
    Ok(())
}

/// Whether a path exists, for building the upload's list.
pub fn existing(host: &dyn Host, paths: impl IntoIterator<Item = String>) -> Vec<String> {
    paths.into_iter().filter(|p| host.exists(Path::new(p))).collect()
}
