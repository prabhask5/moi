//! Machine identity: hostname, OS, kernel, CPU, memory totals, DMI, timezone, virtualization, locale.

use std::path::Path;

use super::collect::lines;
use super::{Collector, Ctx, Output, run_tool};
use crate::host;

pub(crate) struct Meta;

impl Collector for Meta {
    fn name(&self) -> &'static str {
        "meta"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let read = |p: &str| host.read_to_string(Path::new(p)).unwrap_or_default();
        let mut out = Output::default();

        let hostname = read("/proc/sys/kernel/hostname");
        out.stable("meta/hostname", if hostname.trim().is_empty() { read("/etc/hostname") } else { hostname });
        out.stable("meta/os-release", read("/etc/os-release"));
        let kernel =
            [read("/proc/sys/kernel/ostype"), read("/proc/sys/kernel/osrelease"), read("/proc/sys/kernel/arch")];
        let kernel: Vec<&str> = kernel.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        out.stable("meta/kernel", lines([kernel.join(" ")].iter().filter(|s| !s.is_empty())));
        out.stable("meta/cmdline", read("/proc/cmdline"));
        out.stable("meta/cpu", cpu(&read("/proc/cpuinfo")));
        out.stable(
            "meta/memory",
            lines(read("/proc/meminfo").lines().filter(|l| l.starts_with("MemTotal") || l.starts_with("SwapTotal"))),
        );
        let dmi: Vec<String> = ["sys_vendor", "product_name", "product_version", "board_vendor", "board_name"]
            .iter()
            .filter_map(|f| {
                let value = read(&format!("/sys/class/dmi/id/{f}"));
                (!value.trim().is_empty()).then(|| format!("{f}: {}", value.trim()))
            })
            .collect();
        out.stable("meta/dmi", lines(dmi));
        out.stable("meta/timezone", lines(host::timezone(host)));
        out.stable("meta/virt", run_tool(host, "systemd-detect-virt", &[]).unwrap_or_default());
        let locale = [read("/etc/default/locale"), read("/etc/locale.conf")]
            .iter()
            .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
            .find_map(|l| l.trim().strip_prefix("LANG=").map(|v| v.trim_matches('"').to_string()));
        out.stable("meta/locale", lines(locale));

        out.volatile("uptime", read("/proc/uptime"));
        out.volatile("loadavg", read("/proc/loadavg"));
        out.volatile(
            "free",
            lines(read("/proc/meminfo").lines().filter(|l| {
                ["MemTotal", "MemFree", "MemAvailable", "Buffers", "Cached", "SwapTotal", "SwapFree"]
                    .iter()
                    .any(|k| l.split(':').next() == Some(k))
            })),
        );
        Ok(out)
    }
}

/// Model, sockets, cores and threads from `/proc/cpuinfo`: nothing that changes while running (no MHz,
/// no bogomips).
fn cpu(cpuinfo: &str) -> String {
    let field = |block: &str, key: &str| {
        block.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    let blocks: Vec<&str> = cpuinfo.split("\n\n").filter(|b| b.contains("processor")).collect();
    let Some(first) = blocks.first() else { return String::new() };
    let mut sockets: Vec<String> = blocks.iter().filter_map(|b| field(b, "physical id")).collect();
    sockets.sort();
    sockets.dedup();
    let mut out = Vec::new();
    if let Some(model) =
        field(first, "model name").or_else(|| field(first, "Model")).or_else(|| field(first, "Hardware"))
    {
        out.push(format!("model: {model}"));
    }
    out.push(format!("sockets: {}", sockets.len().max(1)));
    if let Some(cores) = field(first, "cpu cores") {
        out.push(format!("cores per socket: {cores}"));
    }
    out.push(format!("threads: {}", blocks.len()));
    lines(out)
}
