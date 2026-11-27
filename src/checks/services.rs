//! `services`: containers, swarm services, each app over HTTP, each database, and the edge (reverse
//! proxy routing on the LAN and from the Internet, proxy errors, firewall rules), with the heal rules
//! these results trigger. A port of `service-check.sh`; the message is the script's, byte for byte.
//!
//! The script asked one thing after another. Here the HTTP checks, the database checks and the public
//! probes each run side by side (plain threads: a few dozen blocking requests), because the moment
//! they matter most — the network is down and every one of them runs into its timeout — is the moment
//! a sequential run would take longer than the check may. Results are reported in configured order.

use std::path::Path;
use std::thread;
use std::time::Duration;

use super::{Check, Cx, Memory, Mode, Outcome};
use crate::config::{Config, EdgeCheck, ExecCheck, HealCheck, HttpCheck, When};
use crate::docker::{Docker, ExecOptions, LogOptions};
use crate::heal::Healer;
use crate::host::{self, Cmd, Host};
use crate::net::{self, Client, Url};

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const EXEC_TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Which sections a check covers: all of them, unless `[checks.edge]` reports to a monitor of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    All,
    Internal,
    Edge,
}

pub(crate) struct Services {
    pub part: Part,
}

/// What is failing, in report order: the name a heal rule can refer to, and the text for the message.
#[derive(Default)]
struct Failing(Vec<(String, String)>);

impl Failing {
    fn add(&mut self, name: &str, text: String) {
        self.0.push((name.to_string(), text));
    }

    /// `{name} ({reason}) `
    fn entry(&mut self, name: &str, reason: &str) {
        self.add(name, format!("{name} ({reason}) "));
    }

    /// A heal rule's bracketed report.
    fn report(&mut self, report: Option<String>) {
        if let Some(report) = report {
            self.add("", format!("{report} "));
        }
    }

    fn any(&self, names: &[String]) -> Option<&str> {
        self.0.iter().map(|(name, _)| name.as_str()).find(|name| names.iter().any(|n| n == name))
    }
}

/// Run a job per item on its own thread; results in item order.
fn side_by_side<T: Sync, R: Send>(items: &[T], job: impl Fn(&T) -> R + Sync) -> Vec<R> {
    thread::scope(|scope| {
        let handles: Vec<_> = items.iter().map(|item| scope.spawn(|| job(item))).collect();
        // a job that panicked takes the whole check down with it, which the engine reports
        handles.into_iter().map(|h| h.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))).collect()
    })
}

/// Whether a status is one of `ok_status` (`2xx` classes or exact codes).
fn status_ok(ok: &[String], code: &str) -> bool {
    ok.iter().any(|want| match want.strip_suffix("xx") {
        Some(class) => code.len() == 3 && code.starts_with(class),
        None => want == code,
    })
}

/// The address of a container on a network (any of its networks when none is named).
fn address(docker: &Docker, container: &str, network: Option<&str>) -> Option<String> {
    let inspected = docker.container(container).ok()?;
    let networks = inspected["NetworkSettings"]["Networks"].as_object()?;
    let ip = |n: &serde_json::Value| n["IPAddress"].as_str().filter(|ip| !ip.is_empty()).map(str::to_string);
    match network {
        Some(name) => networks.get(name).and_then(ip),
        None => networks.values().find_map(ip),
    }
}

/// Why an app's HTTP check fails, if it does.
fn http_check(cx: &Cx, check: &HttpCheck) -> Option<String> {
    let address = if check.target == "localhost" {
        "127.0.0.1".to_string()
    } else {
        match address(&cx.docker, &check.target, cx.config.checks.services.network.as_deref()) {
            Some(address) => address,
            None => return Some("no IP".to_string()),
        }
    };
    let headers: Vec<(String, String)> =
        check.api_key.iter().map(|key| ("X-Api-Key".to_string(), key.expose().to_string())).collect();
    let client = Client::new(cx.host.clone()).with_timeout(HTTP_TIMEOUT);
    let response = match client.send("GET", &format!("http://{address}:{}", check.path), &headers, None) {
        Ok(response) if response.status < 500 => response,
        _ => return Some("no response".to_string()),
    };
    if check.api_key.is_some() && response.status != 200 {
        return Some(format!("HTTP {}, check API key", response.status));
    }
    check
        .trouble
        .as_ref()
        .filter(|trouble| response.text().contains(trouble.as_str()))
        .map(|_| "reports problem".to_string())
}

/// Whether a database answers: its command exits 0 and prints what is expected.
fn exec_check(cx: &Cx, check: &ExecCheck) -> bool {
    let words: Vec<&str> = check.cmd.split_whitespace().collect();
    let docker = cx.docker.clone().with_timeout(EXEC_TIMEOUT);
    match docker.exec(&check.container, &words, &ExecOptions::default()) {
        Ok(out) => {
            let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            out.success() && check.expect.as_ref().is_none_or(|expect| said.contains(expect.as_str()))
        }
        Err(_) => false,
    }
}

/// One public URL: probe it, and if that fails say where, try once more and note what DNS says now.
/// Returns the entry for the log and whether it counts as failing (the retry failed too).
fn public_check(host: &std::sync::Arc<dyn Host>, edge: &EdgeCheck, url: &str) -> Option<(String, bool)> {
    let ok = |probe: &net::Probe| probe.status.is_some_and(|code| status_ok(&edge.ok_status, &code.to_string()));
    let first = net::probe(host, url, PROBE_TIMEOUT);
    if ok(&first) {
        return None;
    }
    host.sleep(edge.public_retry_after.as_duration());
    let second = net::probe(host, url, PROBE_TIMEOUT);
    let again = if ok(&second) {
        format!("retry OK in {}s", net::seconds(second.total))
    } else {
        format!("retry failed: {}", second.message())
    };
    let name = Url::parse(url).map(|u| u.host).unwrap_or_default();
    let now = host.resolve(&name, DNS_TIMEOUT).ok().and_then(|found| found.into_iter().find(std::net::IpAddr::is_ipv4));
    let now = now.map_or_else(|| "NOT RESOLVING".to_string(), |ip| ip.to_string());
    let shown = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    Some((format!("public:{shown} ({}; {again}; DNS now: {now})", first.message()), !ok(&second)))
}

impl Services {
    fn internal(cx: &Cx, bad: &mut Failing, healer: &Healer) -> String {
        let settings = &cx.config.checks.services;
        // 1. containers: stopped, exited, restarting or unhealthy (swarm tasks are judged by their service)
        let mut total = 0;
        for container in cx.docker.containers_inspect().unwrap_or_default() {
            if container["Config"]["Labels"]["com.docker.swarm.task.id"].as_str().is_some_and(|id| !id.is_empty()) {
                continue;
            }
            total += 1;
            let name = container["Name"].as_str().unwrap_or_default().trim_start_matches('/');
            if settings.ignore.iter().any(|ignored| ignored == name) {
                continue;
            }
            let state = container["State"]["Status"].as_str().unwrap_or_default();
            let health = container["State"]["Health"]["Status"].as_str().unwrap_or_default();
            if state != "running" || health == "unhealthy" {
                let health = if health.is_empty() { String::new() } else { format!(", {health}") };
                bad.entry(name, &format!("{state}{health}"));
            }
        }
        // 1b. swarm services: every replica running
        let mut services: Vec<(String, Option<(u64, u64)>)> = cx
            .docker
            .services_status()
            .unwrap_or_default()
            .iter()
            .map(|s| {
                let status = &s["ServiceStatus"];
                (
                    s["Spec"]["Name"].as_str().unwrap_or_default().to_string(),
                    status["RunningTasks"].as_u64().zip(status["DesiredTasks"].as_u64()),
                )
            })
            .collect();
        services.sort();
        for (name, counts) in &services {
            if let Some((running, wanted)) = counts.filter(|(running, wanted)| running != wanted) {
                bad.entry(name, &format!("swarm {running}/{wanted}"));
            }
        }
        // 2. does each app answer, and does it report problems
        for (check, failure) in settings.http.iter().zip(side_by_side(&settings.http, |check| http_check(cx, check))) {
            if let Some(reason) = failure {
                bad.entry(&check.name, &reason);
            }
        }
        // 3. does each database accept connections
        for (check, ok) in settings.exec.iter().zip(side_by_side(&settings.exec, |check| exec_check(cx, check))) {
            if !ok {
                bad.entry(&check.name, "not accepting connections");
            }
        }
        // 4a. containers that must be on a network (a proxy that lost it answers 502 for everything)
        for rule in &cx.config.heal {
            if let Some(HealCheck::ContainerOnNetwork(want)) = &rule.check
                && address(&cx.docker, &want.container, Some(&want.network)).is_none()
            {
                bad.report(healer.fire(rule, &format!("{} is not on the {} network", want.container, want.network)));
            }
        }
        format!(
            "{total} containers + {} swarm services running, {} app, {} database",
            services.len(),
            settings.http.len(),
            settings.exec.len()
        )
    }

    fn edge(cx: &Cx, bad: &mut Failing) {
        let edge = &cx.config.checks.edge;
        let host = cx.host.as_ref();
        // 4b. LAN routing through the proxy
        let client = Client::new(cx.host.clone()).with_timeout(HTTP_TIMEOUT);
        for site in &edge.lan_hosts {
            let code = client
                .send_as("GET", "http://127.0.0.1/", Some(site), &[], None)
                .map_or_else(|_| "000".to_string(), |r| r.status.to_string());
            if !status_ok(&edge.ok_status, &code) {
                bad.entry(&format!("traefik-lan:{site}"), &format!("HTTP {code}"));
            }
        }
        // 4c. public routing end to end
        let entries: Vec<(String, bool)> =
            side_by_side(&edge.public_urls, |url| public_check(&cx.host, edge, url)).into_iter().flatten().collect();
        if !entries.is_empty() {
            let stamp = host::format_rfc3339(host.now());
            let log = Path::new(&edge.public_log);
            let mut text = host.read_to_string(log).unwrap_or_default();
            for (entry, failing) in &entries {
                text.push_str(&format!("{stamp} {entry}\n"));
                if *failing {
                    let name = entry.split(" (").next().unwrap_or(entry);
                    bad.add(name, format!("{entry} "));
                }
            }
            // the log is a convenience: a check must not fail because it cannot be written
            let _ = host.write(log, text.as_bytes(), 0o644);
        }
        // 4d. errors the proxy logged lately
        if let Some(proxy) = &edge.proxy_container {
            let since = host::unix_seconds(host.now()) - i64::try_from(edge.proxy_error_window.as_secs()).unwrap_or(0);
            let (stdout, stderr) =
                cx.docker.logs(proxy, &LogOptions { since: Some(since), tail: None }).unwrap_or_default();
            let said =
                format!("{}{}", String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr)).to_lowercase();
            let errors = said.lines().filter(|l| l.contains(" err ") || l.contains("level=error")).count();
            if errors > 0 {
                bad.entry("traefik", &format!("{errors} errors in {}: docker logs {proxy}", edge.proxy_error_window));
            }
        }
        // 4e. firewall rules that must exist
        if !edge.firewall_required.is_empty() {
            let rules = host
                .run(&Cmd::new("iptables").args(["-S", "DOCKER-USER"]).timeout(Duration::from_secs(20)))
                .map(|out| out.stdout_str().into_owned())
                .unwrap_or_default();
            for want in &edge.firewall_required {
                if !rules.contains(want.as_str()) {
                    bad.add("firewall", format!("firewall (missing: {want}) "));
                }
            }
        }
    }
}

impl Check for Services {
    fn name(&self) -> String {
        if self.part == Part::Edge { "edge" } else { "services" }.to_string()
    }

    fn timeout(&self, config: &Config) -> Duration {
        config.checks.services.common.timeout.as_duration()
    }

    fn monitor(&self, config: &Config) -> Option<String> {
        if self.part == Part::Edge {
            config.checks.edge.monitor.clone()
        } else {
            config.checks.services.common.monitor.clone()
        }
    }

    fn journal_tag(&self, config: &Config) -> String {
        if self.part == Part::Edge {
            "moi-check-edge".to_string()
        } else {
            config.checks.services.common.journal_tag.clone()
        }
    }

    fn run(&self, cx: &Cx, _memory: &mut Memory) -> Outcome {
        let healer = Healer {
            host: cx.host.clone(),
            docker: cx.docker.clone(),
            state: cx.state.clone(),
            act: cx.mode == Mode::Record,
        };
        let edge = &cx.config.checks.edge;
        let mut bad = Failing::default();
        let mut counted = Vec::new();
        if self.part != Part::Edge {
            counted.push(Self::internal(cx, &mut bad, &healer));
        }
        if self.part != Part::Internal && edge.enabled {
            Self::edge(cx, &mut bad);
            counted.push(format!("{} edge", edge.lan_hosts.len() + edge.public_urls.len()));
        }
        if self.part != Part::Edge {
            // 5. rules for named things failing (restart the VPN stack, …), in configured order
            for rule in &cx.config.heal {
                if let When::Failing(names) = &rule.when
                    && let Some(name) = bad.any(names)
                {
                    let why = format!("{name} is failing");
                    bad.report(healer.fire(rule, &why));
                }
            }
        }
        if bad.0.is_empty() {
            Outcome::up(format!("All {} checks OK", counted.join(", ")))
        } else {
            Outcome::down(format!("Failing: {}", bad.0.iter().map(|(_, text)| text.as_str()).collect::<String>()))
        }
    }
}
