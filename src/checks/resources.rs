//! `resources`: CPU, memory, swap, CPU temperature, OOM kills, containers that crashed and restarted.
//! A port of `resource-check.sh`; the message is the script's, byte for byte.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use super::{Check, Cx, Memory, Outcome, kernel_log_matches};
use crate::config::Config;

pub(crate) struct Resources;

/// hwmon drivers that report the CPU's own temperature.
const CPU_SENSORS: [&str; 5] = ["coretemp", "k10temp", "zenpower", "cpu_thermal", "acpitz"];

impl Check for Resources {
    fn name(&self) -> String {
        "resources".to_string()
    }

    fn timeout(&self, config: &Config) -> Duration {
        config.checks.resources.common.timeout.as_duration()
    }

    fn monitor(&self, config: &Config) -> Option<String> {
        config.checks.resources.common.monitor.clone()
    }

    fn journal_tag(&self, config: &Config) -> String {
        config.checks.resources.common.journal_tag.clone()
    }

    fn run(&self, cx: &Cx, memory: &mut Memory) -> Outcome {
        let settings = &cx.config.checks.resources;
        let host = cx.host.as_ref();
        let read = |p: &str| host.read_to_string(Path::new(p)).unwrap_or_default();
        let (mut bad, mut info) = (String::new(), String::new());
        // "for 10+ min": how long a sustained condition has held when it is first reported
        let minutes = u64::from(settings.sustain_runs) * settings.common.interval.as_secs() / 60;

        // CPU: the difference of the kernel's counters since the previous run is the true average
        let stat = read("/proc/stat");
        let fields: Vec<u64> = stat
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .skip(1)
            .take(8)
            .map(|f| f.parse().unwrap_or(0))
            .collect();
        let field = |i: usize| fields.get(i).copied().unwrap_or(0);
        let total: u64 = (0..8).map(field).sum();
        let idle = field(3) + field(4);
        let mut cpu = None;
        if let Some((prev_total, prev_idle)) = memory.values.get("cpu").and_then(|v| {
            let (a, b) = v.split_once(' ')?;
            Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))
        }) && total > prev_total
        {
            let dt = total - prev_total;
            let di = idle.saturating_sub(prev_idle).min(dt);
            let percent = 100 * (dt - di) / dt;
            info.push_str(&format!("CPU {percent}%, "));
            cpu = Some(percent);
        }
        memory.values.insert("cpu".to_string(), format!("{total} {idle}"));

        // RAM and swap
        let meminfo = read("/proc/meminfo");
        let kb = |key: &str| -> u64 {
            meminfo
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0)
        };
        let (mem_total, mem_available) = (kb("MemTotal:"), kb("MemAvailable:"));
        let (swap_total, swap_free) = (kb("SwapTotal:"), kb("SwapFree:"));
        let mem = (100 * mem_total.saturating_sub(mem_available)).checked_div(mem_total).unwrap_or(0);
        info.push_str(&format!("RAM {mem}%, "));
        if let Some(swap) = (100 * swap_total.saturating_sub(swap_free)).checked_div(swap_total) {
            info.push_str(&format!("swap {swap}%, "));
            if swap >= u64::from(settings.swap_limit) {
                bad.push_str(&format!("swap {swap}% used (memory pressure) "));
            }
        }

        // CPU temperature: the hottest reading of the CPU's own sensor driver
        let mut temp: Option<i64> = None;
        for hwmon in host.read_dir(Path::new("/sys/class/hwmon")).unwrap_or_default() {
            let dir = format!("/sys/class/hwmon/{hwmon}");
            if !CPU_SENSORS.contains(&read(&format!("{dir}/name")).trim()) {
                continue;
            }
            for file in host.read_dir(Path::new(&dir)).unwrap_or_default() {
                if file.starts_with("temp")
                    && file.ends_with("_input")
                    && let Ok(milli) = read(&format!("{dir}/{file}")).trim().parse::<i64>()
                {
                    temp = Some(temp.map_or(milli / 1000, |t| t.max(milli / 1000)));
                }
            }
        }
        if let Some(t) = temp {
            info.push_str(&format!("CPU {t}C, "));
        }

        // Sustained conditions alert only once they have held for `sustain_runs` runs in a row
        let runs = settings.sustain_runs;
        let cpu_hot = cpu.is_some_and(|c| c >= u64::from(settings.cpu_limit));
        if memory.sustain("cpu", cpu_hot, runs) {
            bad.push_str(&format!("CPU {}% for {minutes}+ min ", cpu.unwrap_or(0)));
        }
        if memory.sustain("mem", mem >= u64::from(settings.mem_limit), runs) {
            bad.push_str(&format!("RAM {mem}% for {minutes}+ min "));
        }
        if memory.sustain("temp", temp.is_some_and(|t| t >= i64::from(settings.temp_limit)), runs) {
            bad.push_str(&format!("CPU {}C for {minutes}+ min ", temp.unwrap_or(0)));
        }

        // Out-of-memory kills since the last run
        let oom = kernel_log_matches(host, settings.oom_window.as_duration(), |l| {
            l.contains("out of memory") || l.contains("oom-kill") || l.contains("killed process")
        });
        if oom > 0 {
            bad.push_str(&format!(
                "kernel killed processes for lack of memory ({oom} events; journalctl -k --since -10min) "
            ));
        }

        // Containers that crashed and were restarted since the last run
        if settings.container_restarts && cx.docker.ping() {
            let now: BTreeMap<String, u64> = cx
                .docker
                .containers_inspect()
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    (
                        c["Name"].as_str().unwrap_or_default().trim_start_matches('/').to_string(),
                        c["RestartCount"].as_u64().unwrap_or(0),
                    )
                })
                .collect();
            if let Some(before) =
                memory.values.get("restarts").and_then(|v| serde_json::from_str::<BTreeMap<String, u64>>(v).ok())
            {
                // only containers known in both runs; byte order of names, as `sort | join` gave
                let crashed: String = now
                    .iter()
                    .filter_map(|(name, count)| {
                        before.get(name).filter(|old| count > *old).map(|old| format!("{name}(+{}) ", count - old))
                    })
                    .collect();
                if !crashed.is_empty() {
                    bad.push_str(&format!("crashed and restarted: {crashed}"));
                }
            }
            memory.values.insert("restarts".to_string(), serde_json::to_string(&now).unwrap_or_default());
        }

        let info = info.strip_suffix(", ").unwrap_or(&info);
        if bad.is_empty() {
            Outcome::up(format!("OK: {info}"))
        } else {
            Outcome::down(format!("Problem: {bad}| {info}"))
        }
    }
}
