//! Scheduled jobs: user crontabs and enabled timers. (`/etc/crontab` and `/etc/cron.*` are captured with
//! `/etc`.)

use std::path::Path;

use super::collect::lines;
use super::{Collector, Ctx, Output, Walk, run_tool, walk_files};

pub(crate) struct Cron;

impl Collector for Cron {
    fn name(&self) -> &'static str {
        "cron"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let mut out = Output::default();
        // Debian keeps user crontabs in crontabs/, RHEL directly in /var/spool/cron
        for dir in ["/var/spool/cron/crontabs", "/var/spool/cron"] {
            for name in host.read_dir(Path::new(dir)).unwrap_or_default() {
                let path = Path::new(dir).join(&name);
                if let Ok(text) = host.read_to_string(&path)
                    && host.metadata(&path).is_ok_and(|m| m.kind == crate::host::FileKind::File)
                {
                    out.stable(&format!("cron/user-{name}.crontab"), text);
                }
            }
        }
        out.grab_all(walk_files(host, Path::new("/var/spool/cron"), &Walk { max_depth: 3, ..Walk::default() }));
        if let Some(timers) = run_tool(
            host,
            "systemctl",
            &["list-unit-files", "--type=timer", "--state=enabled", "--no-legend", "--no-pager"],
        ) {
            out.stable("cron/enabled-timers.txt", lines(timers.lines().filter_map(|l| l.split_whitespace().next())));
        }
        if let Some(schedule) = run_tool(host, "systemctl", &["list-timers", "--all", "--no-pager"]) {
            out.volatile("systemd-timers-schedule.txt", schedule);
        }
        Ok(out)
    }
}
