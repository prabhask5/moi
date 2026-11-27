//! Docker Engine API client (docs/spec/02-snapshot.md § Docker), replayed from the `docker` fixture host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use cestmoi::docker::{Docker, DockerError, ExecOptions, LogOptions};
use cestmoi::host::FixtureHost;

const WEB: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

fn engine() -> (Arc<FixtureHost>, Docker) {
    let host = Arc::new(FixtureHost::open(&common::fixture_dir("docker")).unwrap());
    (host.clone(), Docker::new(host))
}

#[test]
fn a_host_without_docker_is_reported_as_unavailable_not_as_a_crash() {
    let docker = Docker::new(Arc::new(common::minimal()));
    assert!(!docker.ping());
    let err = docker.version().unwrap_err();
    assert!(matches!(err, DockerError::Unavailable { .. }), "{err:?}");
    assert!(err.to_string().starts_with("docker is not reachable at /var/run/docker.sock"), "{err}");
}

#[test]
fn ping_version_and_info() {
    let (_, docker) = engine();
    assert!(docker.ping());
    let v = docker.version().unwrap();
    assert_eq!((v.version.as_str(), v.api_version.as_str()), ("29.1.3", "1.52"));
    assert_eq!(docker.info().unwrap()["DockerRootDir"], "/var/lib/docker");
    assert!(docker.swarm_active().unwrap());
}

#[test]
fn containers_are_listed_with_stopped_ones_and_inspected_in_full() {
    let (host, docker) = engine();
    let list = docker.containers(true).unwrap();
    assert_eq!(list.len(), 4);
    let all = docker.containers_inspect().unwrap();
    let names: Vec<&str> = all.iter().map(|c| c["Name"].as_str().unwrap()).collect();
    assert_eq!(names, ["/web", "/db", "/sidecar", "/api.1.k3x9q0v7m2p8w1z5"]);
    assert_eq!(all[0]["Config"]["Env"][1], "NGINX_PORT=8080");
    assert_eq!(all[2]["HostConfig"]["NetworkMode"], format!("container:{WEB}"));
    // one list call, then one inspect per container
    let requests = host.docker_requests();
    assert_eq!(requests.iter().filter(|r| r.starts_with("GET /containers/json?all=true")).count(), 2);
    assert_eq!(requests.iter().filter(|r| r.ends_with("/json") && r.contains("/containers/")).count(), 4);
}

#[test]
fn a_container_can_be_inspected_by_name() {
    let (_, docker) = engine();
    assert_eq!(docker.container("db").unwrap()["Config"]["Hostname"], "database");
    let err = docker.container("nope").unwrap_err();
    assert!(err.is_not_found(), "{err:?}");
    assert!(err.to_string().starts_with("docker: GET /containers/nope/json: 404 "), "{err}");
}

#[test]
fn networks_volumes_images_and_swarm_objects() {
    let (_, docker) = engine();
    let nets: Vec<String> =
        docker.networks().unwrap().iter().map(|n| n["Name"].as_str().unwrap().to_string()).collect();
    assert_eq!(nets, ["bridge", "homelab", "ingress", "app-net"]);
    assert_eq!(docker.volumes().unwrap().len(), 3, "anonymous volumes are included; the normalizer drops them");
    assert_eq!(docker.images().unwrap().len(), 2);
    let services = docker.services().unwrap();
    assert_eq!(services[0]["Spec"]["Name"], "api");
    assert_eq!(docker.nodes().unwrap()[0]["Description"]["Hostname"], "dockerhost");
    assert_eq!(docker.secrets().unwrap()[0]["Spec"]["Name"], "api_signing_key");
    assert!(docker.configs().unwrap().is_empty());
}

#[test]
fn events_stream_is_decoded_from_chunks_and_ends_when_the_engine_closes_it() {
    let (host, docker) = engine();
    let mut filters = BTreeMap::new();
    filters.insert("type".to_string(), vec!["container".to_string(), "network".to_string(), "service".to_string()]);
    let events: Vec<_> = docker.events(&filters, Some(1_772_366_000), None).unwrap().map(Result::unwrap).collect();
    assert_eq!(events.len(), 4);
    assert_eq!(
        (events[0].kind.as_str(), events[0].action.as_str(), events[0].name()),
        ("container", "die", Some("web"))
    );
    assert_eq!(events[0].attributes["exitCode"], "137");
    assert_eq!((events[1].action.as_str(), events[1].id.as_str(), events[1].time), ("start", WEB, 1_772_366_005));
    assert_eq!((events[3].kind.as_str(), events[3].name()), ("service", Some("api")));
    // filters are sent as percent-encoded JSON
    let sent = host.docker_requests().pop().unwrap();
    assert_eq!(
        sent,
        "GET /events?filters=%7B%22type%22%3A%5B%22container%22%2C%22network%22%2C%22service%22%5D%7D&since=1772366000"
    );
}

#[test]
fn exec_returns_exit_code_and_separates_stdout_from_stderr() {
    let (host, docker) = engine();
    let ok = docker.exec("db", &["pg_isready", "-q"], &ExecOptions::default()).unwrap();
    assert!(ok.success());
    assert!(ok.stdout.is_empty() && ok.stderr.is_empty());

    let failed = docker.exec("db", &["psql", "-c", "select 1"], &ExecOptions::default()).unwrap();
    assert_eq!(failed.exit_code, 2);
    assert_eq!(failed.stderr, b"psql: error: connection refused\n");

    let out = docker
        .exec(WEB, &["cat", "/etc/hostname"], &ExecOptions { user: Some("root".into()), ..Default::default() })
        .unwrap();
    assert_eq!(out.stdout_str(), "a1a1a1a1a1a1\n");
    let version = docker.exec("web", &["nginx", "-v"], &ExecOptions::default()).unwrap();
    assert_eq!((version.stdout.len(), version.stderr.as_slice()), (0, b"nginx version: nginx/1.29.0\n".as_slice()));

    // create, start, inspect — in that order
    let requests = host.docker_requests();
    assert_eq!(requests[0], "POST /containers/db/exec");
    assert!(requests[1].starts_with("POST /exec/") && requests[1].ends_with("/start"), "{requests:?}");
    assert!(requests[2].starts_with("GET /exec/") && requests[2].ends_with("/json"), "{requests:?}");
}

#[test]
fn exec_of_a_command_the_fixture_does_not_know_names_the_file_to_add() {
    let (_, docker) = engine();
    let err = docker.exec("db", &["rm", "-rf", "/"], &ExecOptions::default()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no canned exec output for `rm -rf /` in db"), "{msg}");
    assert!(msg.contains(&format!("exec/db/{}.out", FixtureHost::command_key("rm -rf /"))), "{msg}");
}

#[test]
fn logs_are_demultiplexed() {
    let (host, docker) = engine();
    let (stdout, stderr) = docker.logs("web", &LogOptions { since: Some(1_772_366_100), tail: Some(50) }).unwrap();
    assert_eq!(String::from_utf8(stdout).unwrap(), "GET / 200\nGET /health 200\n");
    assert_eq!(String::from_utf8(stderr).unwrap(), "2026/03/01 11:59:58 [error] upstream timed out\n");
    assert_eq!(host.docker_requests(), ["GET /containers/web/logs?stdout=true&stderr=true&since=1772366100&tail=50"]);
}

#[test]
fn a_test_can_change_what_the_engine_reports() {
    let (host, docker) = engine();
    host.set_docker("/volumes", r#"{"Volumes": []}"#);
    assert!(docker.volumes().unwrap().is_empty());
    host.set_docker("/services", "not json");
    let err = docker.services().unwrap_err();
    assert!(matches!(err, DockerError::Protocol { .. }), "{err:?}");
}

#[test]
fn reading_everything_sends_only_get_requests() {
    let (host, docker) = engine();
    docker.containers_inspect().unwrap();
    docker.networks().unwrap();
    docker.volumes().unwrap();
    docker.services().unwrap();
    docker.logs("web", &LogOptions::default()).unwrap();
    assert!(host.docker_requests().iter().all(|r| r.starts_with("GET ")), "{:?}", host.docker_requests());
}
