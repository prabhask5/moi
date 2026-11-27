//! Addresses, links, routes, DNS, VPN interfaces — without Docker's own interfaces (veth pairs, bridges),
//! which come and go with containers. Uses iproute2 (`ip`), `ss`, and optionally `resolvectl`,
//! `tailscale`, `wg`.

use std::path::Path;

use super::collect::lines;
use super::{Collector, Ctx, Output, run_tool};

pub(crate) struct Network;

fn docker_interface(name: &str) -> bool {
    name.starts_with("veth") || name.starts_with("docker0") || name.starts_with("br-")
}

impl Collector for Network {
    fn name(&self) -> &'static str {
        "network"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let mut out = Output::default();
        let own = |text: String| lines(text.lines().filter(|l| !docker_interface(l)));
        if let Some(addr) = run_tool(host, "ip", &["-br", "addr"]) {
            out.stable("network/addresses.txt", own(addr));
        }
        if let Some(link) = run_tool(host, "ip", &["-br", "link"]) {
            out.stable("network/links.txt", own(link));
        }
        if let Some(routes) = run_tool(host, "ip", &["route"]) {
            let keep = routes.lines().filter(|l| !l.split_whitespace().any(docker_interface));
            out.stable("network/routes.txt", lines(keep));
        }
        let dns = match run_tool(host, "resolvectl", &["dns"]) {
            // "Link 5 (veth1a2b3c):" lines of container interfaces and links without servers are noise
            Some(text) => lines(text.lines().filter(|l| {
                let interface = l.split_once('(').and_then(|(_, rest)| rest.split_once(')')).map(|(name, _)| name);
                !interface.is_some_and(docker_interface) && !l.trim_end().ends_with(':')
            })),
            None => lines(
                host.read_to_string(Path::new("/etc/resolv.conf"))
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.starts_with("nameserver")),
            ),
        };
        out.stable("network/dns.txt", dns);

        if let Some(sockets) = run_tool(host, "ss", &["-tulpnH"]) {
            // protocol, local address, process name — without pids and fds, which change on every restart
            let mut rows: Vec<String> = sockets
                .lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split_whitespace().collect();
                    let process = f.get(6).map(|p| strip_ids(p)).unwrap_or_default();
                    Some(format!("{} {} {process}", f.first()?, f.get(4)?).trim_end().to_string())
                })
                .collect();
            rows.sort();
            rows.dedup();
            out.volatile("listening.txt", lines(rows));
        }
        if let Some(ip) = run_tool(host, "tailscale", &["ip", "-4"]) {
            out.stable("network/tailscale-ip.txt", ip);
            out.volatile("tailscale-status.txt", run_tool(host, "tailscale", &["status"]).unwrap_or_default());
        }
        if let Some(interfaces) = run_tool(host, "wg", &["show", "interfaces"]) {
            out.stable("network/wireguard-interfaces.txt", interfaces);
        }
        Ok(out)
    }
}

/// `users:(("sshd",pid=812,fd=3))` → `users:(("sshd"))`.
fn strip_ids(process: &str) -> String {
    let mut out = String::new();
    let mut rest = process;
    while let Some(i) = rest.find(",pid=").into_iter().chain(rest.find(",fd=")).min() {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after.find([',', ')']).unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}
