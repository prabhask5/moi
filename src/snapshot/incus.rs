//! Incus: the daemon's preseed and each instance's configuration and snapshot names, through the
//! `incus` client. Absent client = no Incus. A client whose daemon does not answer is a failure, so the
//! previous capture is kept rather than deleted.

use std::time::Duration;

use super::collect::lines;
use super::{Collector, Ctx, Output, run_tool};
use crate::host::Cmd;

pub(crate) struct Incus;

impl Collector for Incus {
    fn name(&self) -> &'static str {
        "incus"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let mut out = Output::default();
        let list = Cmd::new("incus").args(["list", "-c", "n", "-f", "csv"]).timeout(Duration::from_secs(20));
        let names = match host.run(&list) {
            Err(_) => return Ok(out),
            Ok(result) if result.success() => result.stdout_str().into_owned(),
            Ok(result) => return Err(format!("incus is installed but not answering: {}", result.stderr_str().trim())),
        };
        out.stable("incus/preseed.yaml", run_tool(host, "incus", &["admin", "init", "--dump"]).unwrap_or_default());
        out.volatile(
            "incus-list.txt",
            run_tool(host, "incus", &["list", "-c", "nstS4", "-f", "compact"]).unwrap_or_default(),
        );
        for name in names.lines().map(str::trim).filter(|n| !n.is_empty()) {
            if let Some(config) = run_tool(host, "incus", &["config", "show", name]) {
                // volatile.* keys are runtime state (MAC addresses, last power state, …)
                let stable = config.lines().filter(|l| !l.trim_start().starts_with("volatile."));
                out.stable(&format!("incus/instances/{name}.yaml"), lines(stable));
            }
            let snapshots = run_tool(host, "incus", &["snapshot", "list", name, "-f", "csv", "-c", "n"]);
            out.stable(&format!("incus/instances/{name}.snapshots"), snapshots.unwrap_or_default());
        }
        Ok(out)
    }
}
