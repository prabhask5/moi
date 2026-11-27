//! The staged probe and the fixture network it is tested on (docs/spec/05-checks.md § Probe).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use cestmoi::host::{FixtureHost, Host};
use cestmoi::net::{self, Failure};

/// The `minimal` host with this `net` file.
fn network(test: &str, net: &str) -> Arc<dyn Host> {
    let case = common::scratch(&format!("{test}-case"));
    std::fs::write(case.join("net"), net).unwrap();
    Arc::new(common::minimal().with_case(&case).unwrap())
}

const SITE: &str =
    "dns site.test 192.0.2.7 took=0.004\ntcp 192.0.2.7:443 ok connect=0.012 tls=0.050 first=0.300 rest=0.046\n";
const LIMIT: Duration = Duration::from_secs(15);

#[test]
fn a_working_site_reports_its_status_and_the_time_every_stage_took() {
    let host = network("probe-ok", &format!("{SITE}http site.test/health 200 fine\n"));
    let probe = net::probe(&host, "https://site.test/health", LIMIT);
    assert_eq!((probe.status, probe.failure.clone(), probe.ip.as_deref()), (Some(200), None, Some("192.0.2.7")));
    assert_eq!(probe.total, Duration::from_micros(412_000));
    assert_eq!(probe.message(), "HTTP 200 in 0.412000s");
    // plain http has no TLS stage; a port in the URL is the port connected to
    let host = network("probe-http", "tcp 192.0.2.9:8080 ok connect=0.001\nhttp 192.0.2.9:8080/ 204\n");
    assert_eq!(net::probe(&host, "http://192.0.2.9:8080/", LIMIT).message(), "HTTP 204 in 0.001000s");
}

#[test]
fn an_error_status_is_a_response_and_cloudflare_origin_errors_are_named() {
    for (status, message) in [
        (404, "HTTP 404 in 0.412000s"),
        (502, "HTTP 502 in 0.412000s"),
        (520, "HTTP 520 in 0.412000s (Cloudflare couldn't reach the origin)"),
        (530, "HTTP 530 in 0.412000s (Cloudflare couldn't reach the origin)"),
        (531, "HTTP 531 in 0.412000s"),
    ] {
        let host = network("probe-status", &format!("{SITE}http site.test/ {status}\n"));
        assert_eq!(net::probe(&host, "https://site.test/", LIMIT).message(), message);
    }
}

#[test]
fn every_stage_that_fails_is_named() {
    let cases: [(&str, Failure, &str); 9] = [
        ("dns site.test - took=0.002\n", Failure::Dns, "DNS lookup failed after 0.002000s"),
        ("dns site.test timeout\n", Failure::DnsTimeout, "timed out in DNS lookup after 15.000000s"),
        (
            "dns site.test 192.0.2.7\ntcp 192.0.2.7:443 refused connect=0.001\n",
            Failure::Refused,
            "connection refused by 192.0.2.7 after 0.001000s",
        ),
        (
            "dns site.test 192.0.2.7\ntcp 192.0.2.7:443 timeout\n",
            Failure::ConnectTimeout,
            "timed out connecting to 192.0.2.7 after 15.000000s",
        ),
        (
            "dns site.test 192.0.2.7\ntcp 192.0.2.7:443 tls-timeout\n",
            Failure::TlsTimeout,
            "timed out in TLS handshake with 192.0.2.7 after 15.000000s",
        ),
        (
            "dns site.test 192.0.2.7\ntcp 192.0.2.7:443 tls-error invalid peer certificate: UnknownIssuer\n",
            Failure::Tls("invalid peer certificate: UnknownIssuer".into()),
            "TLS/cert error: invalid peer certificate: UnknownIssuer after 0.000000s",
        ),
        (
            "dns site.test 192.0.2.7\ntcp 192.0.2.7:443 silent\n",
            Failure::NoResponse,
            "connected to 192.0.2.7, no response from origin after 15.000000s",
        ),
        (
            "dns site.test 192.0.2.7\ntcp 192.0.2.7:443 slow first=0.2\nhttp site.test/ 200 partial\n",
            Failure::TooSlow,
            "response too slow after 15.000000s",
        ),
        (
            "dns site.test 192.0.2.7\ntcp 192.0.2.7:443 reset\n",
            Failure::Dropped("Connection reset by peer".into()),
            "connection dropped by 192.0.2.7: Connection reset by peer after 0.000000s",
        ),
    ];
    for (net, failure, message) in cases {
        let host = network("probe-failures", net);
        let probe = net::probe(&host, "https://site.test/", LIMIT);
        assert_eq!(probe.failure, Some(failure), "{net}");
        assert_eq!(probe.message(), message);
    }
    // a name nobody knows, and a URL that is none
    let host = network("probe-unknown", "dns other.test 192.0.2.1\n");
    assert_eq!(net::probe(&host, "https://site.test/", LIMIT).message(), "DNS lookup failed after 0.000000s");
    assert!(net::probe(&host, "ftp://site.test/", LIMIT).message().starts_with("error: "));
}

#[test]
fn the_status_is_kept_when_the_rest_of_the_response_fails() {
    // curl reports the code of a response whose body never finished, and the legacy check judged by it
    let host =
        network("probe-slow-status", "dns site.test 192.0.2.7\ntcp 192.0.2.7:443 slow\nhttp site.test/ 302 moved\n");
    let probe = net::probe(&host, "https://site.test/", LIMIT);
    assert_eq!((probe.status, probe.failure), (Some(302), Some(Failure::TooSlow)));
}

#[test]
fn the_time_limit_covers_the_whole_probe_not_each_stage() {
    // 6 s of lookup, 6 s of connecting: the handshake has 3 s left, not 15
    let host = network("probe-budget", "dns site.test 192.0.2.7 took=6\ntcp 192.0.2.7:443 tls-timeout connect=6\n");
    let probe = net::probe(&host, "https://site.test/", LIMIT);
    assert_eq!(probe.total, LIMIT);
    assert_eq!(probe.message(), "timed out in TLS handshake with 192.0.2.7 after 15.000000s");
}

#[test]
fn the_fixture_network_answers_in_turn_and_keeps_virtual_time_per_thread() {
    let host = network("probe-sequence", &format!("{SITE}http site.test/ 502,503,200\n"));
    let codes: Vec<Option<u16>> = (0..4).map(|_| net::probe(&host, "https://site.test/", LIMIT).status).collect();
    assert_eq!(codes, [Some(502), Some(503), Some(200), Some(200)]);

    // waiting costs no real time, and another thread's clock is its own
    let before = host.now();
    let started = std::time::Instant::now();
    host.sleep(Duration::from_secs(3600));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(host.now().duration_since(before).unwrap() >= Duration::from_secs(3600));
    let other = Arc::clone(&host);
    let elsewhere = std::thread::spawn(move || other.now()).join().unwrap();
    assert!(elsewhere < host.now());
    // names resolve as listed; address literals as themselves
    assert_eq!(host.resolve("site.test", LIMIT).unwrap(), ["192.0.2.7".parse::<std::net::IpAddr>().unwrap()]);
    assert_eq!(host.resolve("10.1.2.3", LIMIT).unwrap(), ["10.1.2.3".parse::<std::net::IpAddr>().unwrap()]);
    assert!(host.resolve("nowhere.test", LIMIT).is_err());
}

#[test]
fn a_bad_net_file_is_an_error_with_its_line() {
    let case = common::scratch("probe-bad-net");
    std::fs::write(case.join("net"), "dns site.test 192.0.2.7\nhttp site.test/ teapot\n").unwrap();
    let err = FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_case(&case).unwrap_err().to_string();
    assert!(err.contains("net:2: expected"), "{err}");
}
