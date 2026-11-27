//! `moi context share` (docs/spec/03-context.md § Sharing): signatures against AWS's published examples,
//! the whole flow against an in-memory bucket, and — in the integration job — against a real
//! S3-compatible server.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cestmoi::cli;
use cestmoi::context::share::{Signer, hmac_sha256};
use cestmoi::host::{FixtureHost, Host, HttpHandler};
use cestmoi::net::{Client, Url};
use cestmoi::state::StateDir;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------ primitives

#[test]
fn hmac_sha256_matches_rfc_4231() {
    assert_eq!(
        hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    assert_eq!(
        hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    // a key longer than the block size is hashed first
    assert_eq!(
        hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

/// The credentials AWS uses in its documentation examples (built in pieces so secret scanners that look
/// for the shape of a real key do not flag a documented example).
fn aws_example<'a>(access: &'a str, secret: &'a str) -> Signer<'a> {
    Signer { access_key: access, secret_key: secret, region: "us-east-1", amz_date: "20130524T000000Z" }
}

#[test]
fn signatures_match_the_examples_in_the_aws_documentation() {
    let access = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
    let secret = format!("{}{}", "wJalrXUtnFEMI/K7MDENG/", "bPxRfiCYEXAMPLEKEY");
    let signer = aws_example(&access, &secret);

    // "Authenticating Requests: Using Query Parameters" — a presigned GET valid for 24 hours
    let query = signer.presign_get("examplebucket.s3.amazonaws.com", "/test.txt", 86_400);
    assert_eq!(
        query,
        format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={access}%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        )
    );

    // "Authenticating Requests: Using the Authorization Header" — GET Object with a Range header
    let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let headers = [
        ("Range".to_string(), "bytes=0-9".to_string()),
        ("x-amz-content-sha256".to_string(), empty.to_string()),
        ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
    ];
    assert_eq!(
        signer.authorization("GET", "examplebucket.s3.amazonaws.com", "/test.txt", "", &headers, empty),
        format!(
            "AWS4-HMAC-SHA256 Credential={access}/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        )
    );
    // …and the bucket lifecycle example (a sub-resource in the query)
    assert_eq!(
        signer.authorization("GET", "examplebucket.s3.amazonaws.com", "/", "lifecycle=", &headers[1..], empty),
        format!(
            "AWS4-HMAC-SHA256 Credential={access}/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;x-amz-content-sha256;x-amz-date,\
             Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        )
    );
    assert!(!format!("{signer:?}").contains("EXAMPLEKEY"));
}

#[test]
fn urls_are_parsed_into_what_a_request_needs() {
    let u = Url::parse("https://acct.r2.example.com/bucket/ctx/a.md?x=1").unwrap();
    assert_eq!(
        (u.https, u.host.as_str(), u.port, u.target.as_str(), u.authority().as_str()),
        (true, "acct.r2.example.com", 443, "/bucket/ctx/a.md?x=1", "acct.r2.example.com")
    );
    let u = Url::parse("http://localhost:9000").unwrap();
    assert_eq!((u.https, u.port, u.target.as_str(), u.authority().as_str()), (false, 9000, "/", "localhost:9000"));
    assert_eq!(Url::parse("http://kuma.lan?x").unwrap().target, "/?x");
    // credentials in a URL are dropped, never sent
    assert_eq!(Url::parse("http://user:pw@kuma.lan:3001/api").unwrap().authority(), "kuma.lan:3001");
    for bad in ["ftp://x", "kuma.lan", "http://", "http://:80/"] {
        assert!(Url::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_http_client_speaks_plain_http_through_the_host() {
    let host = Arc::new(common::minimal());
    let client = Client::new(host.clone());
    // a host without a network refuses instead of reaching the real one
    let err = client.send("GET", "http://example.com/", &[], None).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);

    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let log = seen.clone();
    host.set_http(Arc::new(move |authority, raw| {
        log.lock().unwrap().push((authority.to_string(), String::from_utf8_lossy(raw).into_owned()));
        b"HTTP/1.1 201 Created\r\nX-Thing: yes\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".to_vec()
    }));
    let response = client
        .send("POST", "http://api.test:8080/v1/items?a=b", &[("X-Token".into(), "t".into())], Some(b"{}"))
        .unwrap();
    assert_eq!(
        (response.status, response.text().as_str(), response.header("x-thing")),
        (201, "hello world", Some("yes"))
    );
    assert!(response.is_success());
    let (authority, raw) = seen.lock().unwrap()[0].clone();
    assert_eq!(authority, "api.test:8080");
    assert!(raw.starts_with("POST /v1/items?a=b HTTP/1.1\r\nHost: api.test:8080\r\n"), "{raw}");
    assert!(
        raw.contains("\r\nX-Token: t\r\n") && raw.contains("\r\nContent-Length: 2\r\n") && raw.ends_with("\r\n\r\n{}"),
        "{raw}"
    );
}

// ------------------------------------------------------------------ the flow, against an in-memory bucket

/// (method, target, headers, body) of a request the fake bucket received.
type Recorded = (String, String, BTreeMap<String, String>, Vec<u8>);

#[derive(Default)]
struct FakeS3 {
    objects: BTreeMap<String, Vec<u8>>,
    lifecycle: Option<String>,
    requests: Vec<Recorded>,
}

fn respond(status: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        .into_bytes()
}

fn fake_s3(state: Arc<Mutex<FakeS3>>) -> HttpHandler {
    Arc::new(move |_authority, raw| {
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let body = raw[split + 4..].to_vec();
        let mut lines = head.lines();
        let mut first = lines.next().unwrap().split(' ');
        let (method, target) = (first.next().unwrap().to_string(), first.next().unwrap().to_string());
        let headers: BTreeMap<String, String> =
            lines.filter_map(|l| l.split_once(": ")).map(|(k, v)| (k.to_lowercase(), v.to_string())).collect();
        let mut s3 = state.lock().unwrap();
        s3.requests.push((method.clone(), target.clone(), headers, body.clone()));
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let key = path.strip_prefix("/moi-share").unwrap_or(path).trim_start_matches('/').to_string();
        match (method.as_str(), key.is_empty(), query) {
            ("GET", true, "lifecycle=") => match &s3.lifecycle {
                Some(xml) => respond("200 OK", xml),
                None => respond(
                    "404 Not Found",
                    "<Error><Code>NoSuchLifecycleConfiguration</Code><Message>none</Message></Error>",
                ),
            },
            ("PUT", true, "lifecycle=") => {
                s3.lifecycle = Some(String::from_utf8(body).unwrap());
                respond("200 OK", "")
            }
            ("GET", true, q) if q.contains("list-type=2") => {
                let keys: String = s3
                    .objects
                    .keys()
                    .filter(|k| k.starts_with("ctx/"))
                    .map(|k| format!("<Contents><Key>{k}</Key></Contents>"))
                    .collect();
                respond(
                    "200 OK",
                    &format!("<ListBucketResult><IsTruncated>false</IsTruncated>{keys}</ListBucketResult>"),
                )
            }
            ("PUT", false, _) => {
                s3.objects.insert(key, body);
                respond("200 OK", "")
            }
            ("DELETE", false, _) => {
                s3.objects.remove(&key);
                respond("204 No Content", "")
            }
            ("GET", false, _) => match s3.objects.get(&key) {
                Some(data) => respond("200 OK", &String::from_utf8_lossy(data)),
                None => respond(
                    "404 Not Found",
                    "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>",
                ),
            },
            _ => respond(
                "400 Bad Request",
                "<Error><Code>Unexpected</Code><Message>the fake does not know this request</Message></Error>",
            ),
        }
    })
}

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    state: StateDir,
    s3: Arc<Mutex<FakeS3>>,
}

/// The cestmoi fixture with its share bucket at `endpoint` and `extra` appended to `[context.share]`.
fn fixture(test: &str, endpoint: &str, access: &str, secret: &str, region: &str) -> (Arc<FixtureHost>, PathBuf) {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    let secrets = fixture.read_to_string(Path::new("/etc/moi/secrets.toml")).unwrap();
    let secrets: String = secrets
        .lines()
        .map(|l| match l.split_once(" = ") {
            Some(("share_endpoint", _)) => format!("share_endpoint = \"{endpoint}\"\n"),
            Some(("share_key_id", _)) => format!("share_key_id = \"{access}\"\n"),
            Some(("share_secret", _)) => format!("share_secret = \"{secret}\"\n"),
            _ => format!("{l}\n"),
        })
        .collect();
    fixture.write(Path::new("/etc/moi/secrets.toml"), secrets.as_bytes(), 0o600).unwrap();
    let config = fixture
        .read_to_string(Path::new("/etc/moi/moi.toml"))
        .unwrap()
        .replace("[context.share]", &format!("[context.share]\nregion = \"{region}\""));
    fixture.write(Path::new("/etc/moi/moi.toml"), config.as_bytes(), 0o644).unwrap();
    (fixture, overlay)
}

fn server(test: &str) -> Server {
    let (fixture, _) = fixture(test, "http://s3.test:9000", "FAKEshareaccesskey", "FAKEsharesecretkey000", "auto");
    let s3: Arc<Mutex<FakeS3>> = Arc::default();
    fixture.set_http(fake_s3(s3.clone()));
    Server { host: fixture.clone(), fixture, state: StateDir::new("/var/lib/moi"), s3 }
}

impl Server {
    fn moi(&self, args: &[&str]) -> Result<String, String> {
        let mut out = Vec::new();
        cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out)
            .map(|()| String::from_utf8(out).unwrap())
            .map_err(|e| e.to_string())
    }

    fn requests(&self) -> Vec<String> {
        self.s3.lock().unwrap().requests.iter().map(|(m, t, _, _)| format!("{m} {t}")).collect()
    }
}

#[test]
fn a_share_uploads_the_context_file_under_a_random_key_and_prints_a_presigned_link() {
    let s = server("share-basic");
    let out = s.moi(&["context", "share"]).unwrap();
    let (url, rest) = out.split_once('\n').unwrap();
    assert_eq!(rest, "expires 2026-11-26 18:00 UTC (in 1h); `moi context share --revoke` ends it now\n");
    let (location, query) = url.split_once('?').unwrap();
    let key = location.strip_prefix("http://s3.test:9000/moi-share/").unwrap();
    assert!(key.starts_with("ctx/") && key.ends_with(".md") && key.len() == 4 + 32 + 3, "{key}");
    assert!(key[4..36].bytes().all(|b| b.is_ascii_hexdigit()), "128 random bits as hex: {key}");
    assert!(query.starts_with("X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=FAKEshareaccesskey%2F20261126%2Fauto%2Fs3%2Faws4_request&X-Amz-Date=20261126T170000Z&X-Amz-Expires=3600&X-Amz-SignedHeaders=host&X-Amz-Signature="), "{query}");
    assert!(!url.contains("FAKEsharesecretkey"));

    let s3 = s.s3.lock().unwrap();
    // exactly what is on disk, and nothing a secret
    let uploaded = String::from_utf8(s3.objects[key].clone()).unwrap();
    assert_eq!(uploaded, s.host.read_to_string(&s.state.context()).unwrap());
    assert!(uploaded.starts_with("# Server context: cestmoi\n"));
    for secret in std::fs::read_to_string(common::fixture_dir("cestmoi").join("secrets"))
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        assert!(!uploaded.contains(secret), "{secret}");
    }
    assert!(!uploaded.contains("FAKEshare"));
    // the upload is signed, typed, uncacheable — and the only object written
    let (_, _, headers, _) =
        s3.requests.iter().find(|(m, t, _, _)| m == "PUT" && t.starts_with("/moi-share/ctx/")).unwrap();
    assert_eq!(headers["content-type"], "text/markdown; charset=utf-8");
    assert_eq!(headers["cache-control"], "no-store");
    assert!(headers["authorization"].starts_with("AWS4-HMAC-SHA256 Credential=FAKEshareaccesskey/20261126/auto/s3/aws4_request,SignedHeaders=cache-control;content-type;host;x-amz-content-sha256;x-amz-date,Signature="), "{}", headers["authorization"]);
    assert_eq!(headers["host"], "s3.test:9000");
    assert_eq!(s3.objects.len(), 1);
    let puts: Vec<&String> = s3.requests.iter().filter(|(m, ..)| m == "PUT").map(|(_, t, _, _)| t).collect();
    assert_eq!(puts.len(), 2, "the lifecycle rule and the context file: {puts:?}");
}

#[test]
fn the_lifecycle_safety_net_is_added_once_and_never_replaces_the_buckets_own_rules() {
    let s = server("share-lifecycle");
    s.moi(&["context", "share"]).unwrap();
    let rule = s.s3.lock().unwrap().lifecycle.clone().unwrap();
    assert_eq!(
        rule,
        "<LifecycleConfiguration><Rule><ID>moi-ctx-expire</ID><Filter><Prefix>ctx/</Prefix></Filter><Status>Enabled</Status><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>"
    );
    let (_, _, headers, body) =
        s.s3.lock()
            .unwrap()
            .requests
            .iter()
            .find(|(m, t, _, _)| m == "PUT" && t == "/moi-share?lifecycle=")
            .cloned()
            .unwrap();
    assert_eq!(headers["content-md5"].len(), 24, "S3 requires Content-MD5 on lifecycle configuration");
    assert_eq!(String::from_utf8(body).unwrap(), rule);
    // not asked about again
    let before = s.requests().len();
    s.moi(&["context", "share"]).unwrap();
    assert!(!s.requests()[before..].iter().any(|r| r.contains("lifecycle")), "{:?}", &s.requests()[before..]);

    // a bucket that already has rules keeps them
    let other = server("share-lifecycle-foreign");
    let foreign =
        "<LifecycleConfiguration><Rule><ID>keep-logs-90d</ID><Status>Enabled</Status></Rule></LifecycleConfiguration>";
    other.s3.lock().unwrap().lifecycle = Some(foreign.to_string());
    let v: serde_json::Value = serde_json::from_str(&other.moi(&["context", "share", "--json"]).unwrap()).unwrap();
    assert_eq!(v["share"]["lifecycle"], "skipped: the bucket has lifecycle rules of its own");
    assert_eq!(other.s3.lock().unwrap().lifecycle.as_deref(), Some(foreign));
    assert!(v["share"]["url"].as_str().unwrap().contains("/moi-share/ctx/"));
}

#[test]
fn ttl_is_capped_and_parsed_as_a_duration() {
    let s = server("share-ttl");
    let out = s.moi(&["context", "share", "--ttl", "30m"]).unwrap();
    assert!(out.contains("X-Amz-Expires=1800&") && out.contains("expires 2026-11-26 17:30 UTC (in 30m)"), "{out}");
    // max_ttl is 24h by default
    assert_eq!(
        s.moi(&["context", "share", "--ttl", "2d"]).unwrap_err(),
        "--ttl 2d is longer than context.share.max_ttl (1d): links must stay short-lived"
    );
    assert_eq!(
        s.moi(&["context", "share", "--ttl", "0s"]).unwrap_err(),
        "--ttl 0s is not possible: a link lives between 1s and 7d"
    );
    assert!(cli::parse(&["context", "share", "--ttl", "soon"]).unwrap_err().contains("invalid duration \"soon\""));
    assert!(cli::parse(&["context", "share", "--revoke", "--ttl", "1h"]).is_err(), "--revoke takes no --ttl");
    // refused shares upload nothing
    assert_eq!(s.s3.lock().unwrap().objects.len(), 1);
}

#[test]
fn list_shows_active_shares_and_expired_ones_are_deleted() {
    let s = server("share-list");
    assert_eq!(s.moi(&["context", "share", "--list"]).unwrap(), "no active shares\n");
    s.moi(&["context", "share", "--ttl", "30m"]).unwrap();
    s.fixture.advance(Duration::from_secs(600));
    s.moi(&["context", "share", "--ttl", "2h"]).unwrap();
    let listed = s.moi(&["context", "share", "--list"]).unwrap();
    let lines: Vec<&str> = listed.lines().collect();
    assert_eq!(lines.len(), 2, "{listed}");
    assert!(lines[0].starts_with("ctx/") && lines[0].ends_with(".md  expires 2026-11-26 17:30 UTC"), "{listed}");
    assert!(lines[1].ends_with(".md  expires 2026-11-26 19:10 UTC"), "{listed}");
    assert_eq!(s.s3.lock().unwrap().objects.len(), 2);

    // half an hour later the first link is dead: its object goes, the other stays
    s.fixture.advance(Duration::from_secs(1800));
    let v: serde_json::Value =
        serde_json::from_str(&s.moi(&["context", "share", "--list", "--json"]).unwrap()).unwrap();
    assert_eq!(v["shares"].as_array().unwrap().len(), 1);
    assert_eq!(v["shares"][0]["expires"], "2026-11-26T19:10:00Z");
    let remaining: Vec<String> = s.s3.lock().unwrap().objects.keys().cloned().collect();
    assert_eq!(remaining, [v["shares"][0]["key"].as_str().unwrap()]);
}

#[test]
fn revoke_deletes_every_shared_copy_even_ones_this_machine_forgot_and_nothing_else() {
    let s = server("share-revoke");
    s.moi(&["context", "share"]).unwrap();
    s.moi(&["context", "share"]).unwrap();
    {
        let mut s3 = s.s3.lock().unwrap();
        s3.objects.insert("ctx/left-over-from-a-lost-state-dir.md".into(), b"old".to_vec());
        s3.objects.insert("other/keep.txt".into(), b"not ours".to_vec());
    }
    assert_eq!(
        s.moi(&["context", "share", "--revoke"]).unwrap(),
        "3 shared copies deleted: their links no longer work\n"
    );
    assert_eq!(s.s3.lock().unwrap().objects.keys().cloned().collect::<Vec<_>>(), ["other/keep.txt"]);
    assert_eq!(s.moi(&["context", "share", "--list"]).unwrap(), "no active shares\n");
    assert_eq!(s.moi(&["context", "share", "--revoke"]).unwrap(), "nothing was shared\n");
}

#[test]
fn every_share_is_recorded_in_the_journal() {
    let s = server("share-journal");
    s.fixture.set_env("JOURNAL_STREAM", "8:1");
    s.moi(&["context", "share"]).unwrap();
    s.moi(&["context", "share", "--revoke"]).unwrap();
    let entries = s.fixture.journal_entries();
    let messages: Vec<&str> = entries.iter().map(|e| e[0].1.as_str()).collect();
    assert_eq!(messages, ["context shared, expires 2026-11-26 18:00 UTC", "context shares revoked"]);
    assert_eq!(entries[0][1], ("PRIORITY".to_string(), "5".to_string()));
    assert!(entries[0][3].0 == "MOI_KEY" && entries[0][3].1.starts_with("ctx/"));
}

#[test]
fn errors_say_what_is_wrong() {
    // not configured
    let overlay = common::scratch("share-not-configured");
    let host: Arc<dyn Host> =
        Arc::new(FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_overlay(&overlay).unwrap());
    let mut out = Vec::new();
    let err = cli::execute(&cli::parse(&["context", "share"]).unwrap(), host, &mut out).unwrap_err();
    assert_eq!(
        err.to_string(),
        "context share: not configured: add a [context.share] section to moi.toml (endpoint, bucket, access_key_id, secret_access_key)"
    );
    assert_eq!(err.exit_code(), 2);

    // the bucket refuses
    let s = server("share-refused");
    s.fixture.set_http(Arc::new(|_, _| {
        respond("403 Forbidden", "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>")
    }));
    assert_eq!(
        s.moi(&["context", "share"]).unwrap_err(),
        "context share: upload failed: HTTP 403 AccessDenied Access Denied"
    );
    assert_eq!(s.moi(&["context", "share", "--list"]).unwrap(), "no active shares\n", "a failed upload is not a share");

    // the bucket is unreachable
    let (unreachable, _) = fixture("share-unreachable", "http://s3.test:9000", "FAKEid", "FAKEsecret", "auto");
    let host: Arc<dyn Host> = unreachable;
    let mut out = Vec::new();
    let err = cli::execute(&cli::parse(&["context", "share"]).unwrap(), host, &mut out).unwrap_err().to_string();
    assert!(err.starts_with("context share: cannot reach the share bucket: fixture host has no network"), "{err}");
}

// ------------------------------------------------------------------ against a real S3-compatible server (CI)

/// Forward the fixture's network to the real one: only the integration job does this, against the
/// service container it started.
fn real_network() -> HttpHandler {
    Arc::new(|authority, raw| {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(authority).unwrap();
        stream.write_all(raw).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    })
}

#[test]
#[ignore = "needs MOI_IT services (an S3-compatible server at MOI_IT_S3_ENDPOINT)"]
fn presigned_links_work_against_a_real_s3_server_until_revoked() {
    let endpoint = std::env::var("MOI_IT_S3_ENDPOINT").expect("MOI_IT_S3_ENDPOINT");
    let access = std::env::var("MOI_IT_S3_ACCESS_KEY").expect("MOI_IT_S3_ACCESS_KEY");
    let secret = std::env::var("MOI_IT_S3_SECRET_KEY").expect("MOI_IT_S3_SECRET_KEY");
    let (fixture, _) = fixture("share-integration", &endpoint, &access, &secret, "us-east-1");
    fixture.set_http(real_network());
    // real signatures are checked against real time
    fixture.set_now(std::time::SystemTime::now());
    let host: Arc<dyn Host> = fixture.clone();
    let moi = |args: &[&str]| -> Result<String, String> {
        let mut out = Vec::new();
        cli::execute(&cli::parse(args).unwrap(), host.clone(), &mut out)
            .map(|()| String::from_utf8(out).unwrap())
            .map_err(|e| e.to_string())
    };

    // the private bucket (created with the same signer the feature uses)
    let loaded =
        cestmoi::config::load(host.as_ref(), &cestmoi::config::Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let bucket = cestmoi::context::share::Bucket::new(&host, &loaded.config.context.share).unwrap();
    let created = bucket.request("PUT", "", "", &[], b"").unwrap();
    assert!(created.is_success() || created.status == 409, "create bucket: {} {}", created.status, created.text());

    let out = moi(&["context", "share", "--ttl", "5m"]).unwrap();
    let url = out.lines().next().unwrap().to_string();
    let client = Client::new(host.clone());
    let fetched = client.send("GET", &url, &[], None).unwrap();
    assert_eq!(fetched.status, 200, "{}", fetched.text());
    assert_eq!(fetched.text(), host.read_to_string(Path::new("/var/lib/moi/state/CONTEXT.md")).unwrap());
    assert!(fetched.text().starts_with("# Server context: cestmoi\n"));

    // the bucket is private: no signature, a wrong signature or another key do not get the file
    let unsigned = url.split('?').next().unwrap().to_string();
    assert_eq!(client.send("GET", &unsigned, &[], None).unwrap().status, 403);
    let tampered = format!("{}0000", &url[..url.len() - 4]);
    assert_eq!(client.send("GET", &tampered, &[], None).unwrap().status, 403);

    assert_eq!(moi(&["context", "share", "--list"]).unwrap().lines().count(), 1);
    assert_eq!(moi(&["context", "share", "--revoke"]).unwrap(), "1 shared copy deleted: its link no longer works\n");
    assert_eq!(client.send("GET", &url, &[], None).unwrap().status, 404, "a revoked link is dead before it expires");
}
