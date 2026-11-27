//! The `moi.toml` schema: one struct per section, each with a `read` that applies defaults and validates.
//!
//! Spec: docs/spec/01-config.md § Schema. A feature section (`[checks.resources]`, `[backup]`,
//! `[monitors.kuma]`, …) is switched on by being present; its `enabled` key can switch it off again
//! without deleting the settings. The effective config always carries every section, so the defaults of
//! features that are off are still visible in `moi config check` and `examples/moi.toml`.

use std::collections::BTreeMap;
use std::ops::Range;

use serde::Serialize;

use super::reader::{Ctx, NOT_A_REFERENCE, Tbl, absolute};
use super::units::{AutoOr, Dur, Secret, Size};
use crate::host::{self, Host};
use crate::redact;

const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;

/// Names custom checks may not take.
const BUILTIN_CHECKS: [&str; 5] = ["resources", "storage", "services", "edge", "backup"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Config {
    pub server: Server,
    pub state: State,
    pub capture: Capture,
    pub data: Data,
    pub context: Context,
    pub checks: Checks,
    pub heal: Vec<HealRule>,
    pub boot: Boot,
    pub firewall: Firewall,
    pub backup: Backup,
    pub restore: Restore,
    pub monitors: Monitors,
}

impl Config {
    pub(crate) fn read(root: &mut Tbl<'_, '_>, cx: &mut Ctx<'_>, host: &dyn Host) -> Self {
        let monitors = Monitors::read(root.table(cx, "monitors"), cx);
        let mut used = MonitorNames { kuma: monitors.kuma.enabled, seen: Vec::new() };
        let config = Self {
            server: Server::read(root.table(cx, "server"), cx, host),
            state: State::read(root.table(cx, "state"), cx),
            capture: Capture::read(root.table(cx, "capture"), cx),
            data: Data::read(root.table(cx, "data"), cx),
            context: Context::read(root.table(cx, "context"), cx),
            checks: Checks::read(root.table(cx, "checks"), cx, &mut used),
            heal: read_heal(root, cx),
            boot: Boot::read(root.table(cx, "boot"), cx, &mut used),
            firewall: Firewall::read(root.table(cx, "firewall"), cx),
            backup: Backup::read(root.table(cx, "backup"), cx),
            restore: Restore::read(root.table(cx, "restore"), cx),
            monitors,
        };
        used.report(cx);
        config
    }
}

/// Monitor names in use, to catch two checks pushing to the same monitor and monitors nobody receives.
struct MonitorNames {
    kuma: bool,
    seen: Vec<(String, String, Range<usize>)>,
}

impl MonitorNames {
    fn read(&mut self, t: &mut Tbl<'_, '_>, cx: &mut Ctx<'_>, key: &'static str) -> Option<String> {
        let name = t.string(cx, key)?;
        self.seen.push((name.value.clone(), t.key(key), name.span));
        Some(name.value)
    }

    fn report(mut self, cx: &mut Ctx<'_>) {
        self.seen.sort_by_key(|(_, _, span)| span.start);
        for (i, (name, key, span)) in self.seen.iter().enumerate() {
            if let Some((_, first, _)) = self.seen[..i].iter().find(|(other, _, _)| other == name) {
                cx.err(span, key, format!("monitor \"{name}\" is already used by {first}"));
            } else if !self.kuma {
                cx.warn(span, key, "no [monitors.kuma] section, so nothing will be pushed to this monitor");
            }
        }
    }
}

// -------------------------------------------------------------------------------------------- server, state

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Server {
    /// Used in monitor names and bundle tags. Default: the short hostname.
    pub name: String,
    /// Informational. Default: the system timezone.
    pub timezone: Option<String>,
}

impl Server {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>, host: &dyn Host) -> Self {
        let name =
            t.checked(cx, "name", |s| if s.trim().is_empty() { Err("must not be empty".to_string()) } else { Ok(()) });
        let this = Self {
            name: name.unwrap_or_else(|| host::short_hostname(host)),
            timezone: t.string_opt(cx, "timezone").or_else(|| host::timezone(host)),
        };
        t.finish(cx);
        this
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct State {
    /// State root: `state/`, `moi.db`, `archive/`.
    pub dir: String,
    /// Squash git history older than this. Default: keep everything.
    pub history_keep: Option<Dur>,
}

impl State {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let this = Self { dir: t.path_or(cx, "dir", "/var/lib/moi"), history_keep: t.dur_opt(cx, "history_keep") };
        t.finish(cx);
        this
    }
}

// -------------------------------------------------------------------------------------------- capture, data, context

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Capture {
    /// Directories whose structure (not contents) is recorded. `"auto"`: discovered (02).
    pub data_roots: AutoOr<String>,
    pub tree_depth: u32,
    pub compose_search: Vec<String>,
    pub script_dirs: Vec<String>,
    /// Relative to each home directory.
    pub home_files: Vec<String>,
    pub extra: Vec<String>,
    pub etc_exclude: Vec<String>,
    /// Larger files are recorded (path, size, hash) but not copied.
    pub max_file: Size,
    pub prune_dirs: Vec<String>,
}

impl Capture {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let this = Self {
            data_roots: t.auto_or_list(cx, "data_roots", absolute),
            tree_depth: u32::try_from(t.int_or(cx, "tree_depth", 4, 1, 32)).unwrap_or(4),
            compose_search: t.paths_or(
                cx,
                "compose_search",
                &["/opt", "/srv", "/home", "/root", "/docker", "/data", "/mnt"],
            ),
            script_dirs: t.paths_or(
                cx,
                "script_dirs",
                &["/usr/local/bin", "/usr/local/sbin", "/opt/scripts", "/root/bin", "/home/*/bin"],
            ),
            home_files: t.checked_list(
                cx,
                "home_files",
                &[".bashrc", ".profile", ".ssh", ".config/rclone", ".config/restic", ".gitconfig"],
                |p| {
                    if p.starts_with('/') || p.split('/').any(|c| c == "..") {
                        Err("must be a path relative to a home directory".to_string())
                    } else {
                        Ok(())
                    }
                },
            ),
            extra: t.paths_or(cx, "extra", &[]),
            etc_exclude: t.paths_or(
                cx,
                "etc_exclude",
                &["/etc/dokploy/applications", "/etc/dokploy/logs", "/etc/dokploy/compose", "/etc/ssl/certs"],
            ),
            max_file: t.size_or(cx, "max_file", Size::from_bytes(MB)),
            prune_dirs: t.strings_or(
                cx,
                "prune_dirs",
                &["node_modules", ".git", "cache", "logs", "data", "db", "postgres", "pgdata", "media"],
            ),
        };
        t.finish(cx);
        this
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Data {
    /// Never backed up, never reported as a gap.
    pub fungible: Vec<String>,
    /// Why, per fungible path (shown in CONTEXT.md and the audit).
    pub fungible_reason: BTreeMap<String, String>,
}

impl Data {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let fungible = t.paths_or(cx, "fungible", &[]);
        let mut fungible_reason = BTreeMap::new();
        for (path, reason, _) in t.string_map(cx, "fungible_reason") {
            if !fungible.contains(&path.value) {
                let key = format!("{}.\"{}\"", t.key("fungible_reason"), path.value);
                cx.err(&path.span, &key, "not listed in data.fungible");
            }
            fungible_reason.insert(path.value, reason.value);
        }
        t.finish(cx);
        Self { fungible, fungible_reason }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Context {
    /// Readable copies of CONTEXT.md.
    pub copies: Vec<String>,
    /// Maintain a marked block in `~/.claude/CLAUDE.md` of each copy owner and root.
    pub claude_pointer: bool,
    /// `moi context share`: expiring links from a private bucket (docs/spec/03-context.md § Sharing).
    pub share: ContextShare,
}

impl Context {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let this = Self {
            copies: t.paths_or(cx, "copies", &[]),
            claude_pointer: t.bool_or(cx, "claude_pointer", true),
            share: ContextShare::read(t.table(cx, "share"), cx),
        };
        t.finish(cx);
        this
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextShare {
    pub enabled: bool,
    /// S3 API endpoint of the private bucket. Required when enabled.
    pub endpoint: Option<Secret>,
    /// Private bucket (no public access, no custom domain).
    pub bucket: String,
    /// API token scoped to this bucket. Required when enabled.
    pub access_key_id: Option<Secret>,
    /// Required when enabled.
    pub secret_access_key: Option<Secret>,
    /// Link lifetime when `--ttl` is not given.
    pub default_ttl: Dur,
    /// Longer links are refused.
    pub max_ttl: Dur,
    /// Push a message to the freshness monitor when a link is created.
    pub notify: bool,
}

impl ContextShare {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let enabled = t.bool_or(cx, "enabled", t.present());
        if enabled {
            for key in ["endpoint", "access_key_id", "secret_access_key"] {
                if !t.has(key) {
                    t.missing(cx, key);
                }
            }
        }
        let this = Self {
            enabled,
            endpoint: t.secret(cx, "endpoint", false),
            bucket: t.string_or(cx, "bucket", "moi-share"),
            access_key_id: t.secret(cx, "access_key_id", false),
            secret_access_key: t.secret(cx, "secret_access_key", true),
            default_ttl: t.dur_or(cx, "default_ttl", Dur::from_secs(3600)),
            max_ttl: t.dur_or(cx, "max_ttl", Dur::from_secs(24 * 3600)),
            notify: t.bool_or(cx, "notify", true),
        };
        // The endpoint is masked in output, so say what is wrong without quoting it.
        if this.endpoint.as_ref().is_some_and(|e| http_url(e.expose()).is_err()) {
            cx.err(&t.span_of("endpoint"), &t.key("endpoint"), "must start with http:// or https://");
        }
        if this.default_ttl > this.max_ttl {
            let message = format!("is longer than max_ttl ({})", this.max_ttl);
            cx.err(&t.span_of("default_ttl"), &t.key("default_ttl"), message);
        }
        t.finish(cx);
        this
    }
}

// -------------------------------------------------------------------------------------------- checks

/// Settings every check accepts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckCommon {
    pub enabled: bool,
    pub interval: Dur,
    pub timeout: Dur,
    /// Uptime Kuma push monitor to adopt or create.
    pub monitor: Option<String>,
    /// `SYSLOG_IDENTIFIER` of the check's journal entries.
    pub journal_tag: String,
}

impl CheckCommon {
    fn read(t: &mut Tbl<'_, '_>, cx: &mut Ctx<'_>, name: &str, interval: Dur, used: &mut MonitorNames) -> Self {
        Self {
            enabled: t.bool_or(cx, "enabled", t.present()),
            interval: t.dur_or(cx, "interval", interval),
            timeout: t.dur_or(cx, "timeout", Dur::from_secs(60)),
            monitor: used.read(t, cx, "monitor"),
            journal_tag: t.string_or(cx, "journal_tag", &format!("moi-check-{name}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Checks {
    /// Interval of every check that does not set its own.
    pub default_interval: Dur,
    pub resources: ResourcesCheck,
    pub storage: StorageCheck,
    pub services: ServicesCheck,
    pub edge: EdgeCheck,
    pub backup: BackupCheck,
    pub custom: Vec<CustomCheck>,
}

impl Checks {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>, used: &mut MonitorNames) -> Self {
        let default_interval = t.dur_or(cx, "default_interval", Dur::from_secs(300));
        let resources = ResourcesCheck::read(t.table(cx, "resources"), cx, default_interval, used);
        let storage = StorageCheck::read(t.table(cx, "storage"), cx, default_interval, used);
        let services = ServicesCheck::read(t.table(cx, "services"), cx, default_interval, used);
        let edge = EdgeCheck::read(t.table(cx, "edge"), cx, used);
        let mut backup = t.table(cx, "backup");
        let backup_check = BackupCheck { monitor: used.read(&mut backup, cx, "monitor") };
        backup.finish(cx);

        let mut custom: Vec<CustomCheck> = Vec::new();
        let mut names: Vec<(String, String)> = Vec::new();
        for mut c in t.tables(cx, "custom") {
            let name = c.string_req(cx, "name");
            let span = c.span_of("name");
            if BUILTIN_CHECKS.contains(&name.as_str()) {
                cx.err(&span, &c.key("name"), format!("\"{name}\" is the name of a built-in check"));
            } else if let Some((_, first)) = names.iter().find(|(n, _)| !n.is_empty() && *n == name) {
                cx.err(&span, &c.key("name"), format!("duplicate name \"{name}\" (also {first})"));
            }
            names.push((name.clone(), c.path.clone()));
            if !c.has("run") {
                c.missing(cx, "run");
            }
            let run = c.path_opt(cx, "run").unwrap_or_default();
            let common = CheckCommon::read(&mut c, cx, &name, default_interval, used);
            c.finish(cx);
            custom.push(CustomCheck { name, run, common });
        }
        t.finish(cx);
        Self { default_interval, resources, storage, services, edge, backup: backup_check, custom }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourcesCheck {
    #[serde(flatten)]
    pub common: CheckCommon,
    /// % average since the previous run.
    pub cpu_limit: u32,
    /// % used excluding reclaimable memory.
    pub mem_limit: u32,
    pub swap_limit: u32,
    /// °C of the hottest CPU sensor.
    pub temp_limit: u32,
    /// cpu/mem/temp alert only after this many consecutive runs over the limit.
    pub sustain_runs: u32,
    pub oom_window: Dur,
    pub container_restarts: bool,
}

impl ResourcesCheck {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>, interval: Dur, used: &mut MonitorNames) -> Self {
        let this = Self {
            common: CheckCommon::read(&mut t, cx, "resources", interval, used),
            cpu_limit: t.percent_or(cx, "cpu_limit", 90),
            mem_limit: t.percent_or(cx, "mem_limit", 90),
            swap_limit: t.percent_or(cx, "swap_limit", 80),
            temp_limit: t.u32_or(cx, "temp_limit", 85, 1),
            sustain_runs: t.u32_or(cx, "sustain_runs", 2, 1),
            oom_window: t.dur_or(cx, "oom_window", Dur::from_secs(360)),
            container_restarts: t.bool_or(cx, "container_restarts", true),
        };
        t.finish(cx);
        this
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StorageCheck {
    #[serde(flatten)]
    pub common: CheckCommon,
    /// Checked for space, inodes and read-only remounts.
    pub mounts: Vec<String>,
    /// Must be mounted and writable.
    pub writable: Vec<String>,
    pub disk_limit: u32,
    pub inode_limit: u32,
    pub kernel_error_window: Dur,
    pub nvme_wear_limit: u32,
    pub allowed_bad_sectors: u64,
    /// `"auto"`: every physical disk, type detected.
    pub disks: AutoOr<Disk>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Disk {
    pub device: String,
    /// smartctl device type (`nvme`, `sat`, … or `auto`).
    #[serde(rename = "type")]
    pub kind: String,
    /// °C. Default: by disk type (05).
    pub temp_max: Option<u32>,
}

impl StorageCheck {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>, interval: Dur, used: &mut MonitorNames) -> Self {
        let common = CheckCommon::read(&mut t, cx, "storage", interval, used);
        let disks = match t.auto_or_tables(cx, "disks") {
            None => AutoOr::Auto,
            Some(tables) => AutoOr::List(
                tables
                    .into_iter()
                    .map(|mut d| {
                        if !d.has("device") {
                            d.missing(cx, "device");
                        }
                        let disk = Disk {
                            device: d.path_opt(cx, "device").unwrap_or_default(),
                            kind: d.string_or(cx, "type", "auto"),
                            temp_max: d.has("temp_max").then(|| d.u32_or(cx, "temp_max", 0, 1)),
                        };
                        d.finish(cx);
                        disk
                    })
                    .collect(),
            ),
        };
        let this = Self {
            common,
            mounts: t.paths_or(cx, "mounts", &["/"]),
            writable: t.paths_or(cx, "writable", &[]),
            disk_limit: t.percent_or(cx, "disk_limit", 85),
            inode_limit: t.percent_or(cx, "inode_limit", 85),
            kernel_error_window: t.dur_or(cx, "kernel_error_window", Dur::from_secs(360)),
            nvme_wear_limit: t.percent_or(cx, "nvme_wear_limit", 90),
            allowed_bad_sectors: t.int_or(cx, "allowed_bad_sectors", 0, 0, u64::from(u32::MAX)),
            disks,
        };
        t.finish(cx);
        this
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServicesCheck {
    #[serde(flatten)]
    pub common: CheckCommon,
    /// Containers allowed to be stopped.
    pub ignore: Vec<String>,
    /// Docker network used to reach containers. Default: the first shared network.
    pub network: Option<String>,
    pub http: Vec<HttpCheck>,
    pub exec: Vec<ExecCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HttpCheck {
    pub name: String,
    /// Container name, or `localhost`.
    pub target: String,
    /// `port/path`.
    pub path: String,
    pub api_key: Option<Secret>,
    /// Text in the response body that means trouble.
    pub trouble: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExecCheck {
    pub name: String,
    pub container: String,
    pub cmd: String,
    /// Text the output must contain.
    pub expect: Option<String>,
}

impl ServicesCheck {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>, interval: Dur, used: &mut MonitorNames) -> Self {
        let common = CheckCommon::read(&mut t, cx, "services", interval, used);
        let mut names: Vec<(String, String)> = Vec::new();
        let mut unique = |cx: &mut Ctx<'_>, item: &Tbl<'_, '_>, name: &str| {
            if let Some((_, first)) = names.iter().find(|(n, _)| !n.is_empty() && n == name) {
                cx.err(&item.span_of("name"), &item.key("name"), format!("duplicate name \"{name}\" (also {first})"));
            }
            names.push((name.to_string(), item.path.clone()));
        };
        let mut http = Vec::new();
        for mut h in t.tables(cx, "http") {
            let check = HttpCheck {
                name: h.string_req(cx, "name"),
                target: h.string_req(cx, "target"),
                path: h.string_req(cx, "path"),
                api_key: h.secret(cx, "api_key", true),
                trouble: h.string_opt(cx, "trouble"),
            };
            unique(cx, &h, &check.name);
            h.finish(cx);
            http.push(check);
        }
        let mut exec = Vec::new();
        for mut e in t.tables(cx, "exec") {
            let check = ExecCheck {
                name: e.string_req(cx, "name"),
                container: e.string_req(cx, "container"),
                cmd: e.string_req(cx, "cmd"),
                expect: e.string_opt(cx, "expect"),
            };
            unique(cx, &e, &check.name);
            e.finish(cx);
            exec.push(check);
        }
        let this =
            Self { common, ignore: t.strings_or(cx, "ignore", &[]), network: t.string_opt(cx, "network"), http, exec };
        t.finish(cx);
        this
    }
}

/// Edge probes; part of the services message (same monitor) unless `monitor` is set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EdgeCheck {
    pub enabled: bool,
    pub monitor: Option<String>,
    pub proxy_container: Option<String>,
    /// `Host:` headers sent to `http://127.0.0.1/`.
    pub lan_hosts: Vec<String>,
    pub public_urls: Vec<String>,
    /// `2xx`-style classes or exact codes.
    pub ok_status: Vec<String>,
    pub public_retry_after: Dur,
    pub public_log: String,
    pub proxy_error_window: Dur,
    /// Text that must appear in `iptables -S DOCKER-USER`.
    pub firewall_required: Vec<String>,
}

impl EdgeCheck {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>, used: &mut MonitorNames) -> Self {
        let this = Self {
            enabled: t.bool_or(cx, "enabled", t.present()),
            monitor: used.read(&mut t, cx, "monitor"),
            proxy_container: t.string_opt(cx, "proxy_container"),
            lan_hosts: t.strings_or(cx, "lan_hosts", &[]),
            public_urls: t.checked_list(cx, "public_urls", &[], http_url),
            ok_status: t.checked_list(cx, "ok_status", &["2xx", "3xx", "401"], |s| {
                let b = s.as_bytes();
                let class = b.len() == 3 && (b'1'..=b'5').contains(&b[0]) && &s[1..] == "xx";
                let code = b.len() == 3 && s.parse::<u16>().is_ok_and(|c| (100..600).contains(&c));
                if class || code {
                    Ok(())
                } else {
                    Err("must be a status class like \"2xx\" or a code like \"401\"".into())
                }
            }),
            public_retry_after: t.dur_or(cx, "public_retry_after", Dur::from_secs(3)),
            public_log: t.path_or(cx, "public_log", "/var/log/moi/public-probes.log"),
            proxy_error_window: t.dur_or(cx, "proxy_error_window", Dur::from_secs(300)),
            firewall_required: t.strings_or(cx, "firewall_required", &[]),
        };
        t.finish(cx);
        this
    }
}

/// The backup monitor is pushed by the backup run itself (07).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackupCheck {
    pub monitor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CustomCheck {
    pub name: String,
    /// Exit 0 = up, anything else = down; the first stdout line is the message.
    pub run: String,
    #[serde(flatten)]
    pub common: CheckCommon,
}

// -------------------------------------------------------------------------------------------- heal

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealRule {
    pub name: String,
    pub when: When,
    /// Only act when this does not hold.
    pub check: Option<HealCheck>,
    pub action: Action,
    pub cooldown: Option<Dur>,
    /// Appended to the check message when the action ran.
    pub report: Option<String>,
    /// Appended instead when the action was skipped because of the cooldown.
    pub report_cooldown: Option<String>,
    /// Use the action's stdout as the report.
    pub report_stdout: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    DockerEvent {
        container: String,
        action: String,
    },
    PathChanged(String),
    /// Names from the services results.
    Failing(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealCheck {
    ContainerOnNetwork(ContainerNetwork),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContainerNetwork {
    pub container: String,
    pub network: String,
}

/// What a heal rule does. Serialized the way it is written: `{ run = "…" }`, `{ restart = […], … }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Action {
    NetworkConnect { network_connect: ContainerNetwork },
    Run { run: String },
    ComposeUp { compose_up: String },
    Restart { restart: Vec<String>, wait_healthy: Option<WaitHealthy>, then_restart: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WaitHealthy {
    pub container: String,
    pub timeout: Dur,
}

fn container_network(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> ContainerNetwork {
    let this = ContainerNetwork { container: t.string_req(cx, "container"), network: t.string_req(cx, "network") };
    t.finish(cx);
    this
}

fn read_heal(root: &mut Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Vec<HealRule> {
    let mut rules: Vec<HealRule> = Vec::new();
    let mut names: Vec<(String, String)> = Vec::new();
    for mut t in root.tables(cx, "heal") {
        let name = t.string_req(cx, "name");
        if let Some((_, first)) = names.iter().find(|(n, _)| !n.is_empty() && *n == name) {
            cx.err(&t.span_of("name"), &t.key("name"), format!("duplicate name \"{name}\" (also {first})"));
        }
        names.push((name.clone(), t.path.clone()));

        let when = read_when(&mut t, cx);
        let check = t.has("check").then(|| {
            let mut c = t.table(cx, "check");
            if !c.has("container_on_network") {
                c.missing(cx, "container_on_network");
            }
            let check = HealCheck::ContainerOnNetwork(container_network(c.table(cx, "container_on_network"), cx));
            c.finish(cx);
            check
        });
        let action = read_action(&mut t, cx);
        let rule = HealRule {
            name,
            when,
            check,
            action,
            cooldown: t.dur_opt(cx, "cooldown"),
            report: t.string_opt(cx, "report"),
            report_cooldown: t.string_opt(cx, "report_cooldown"),
            report_stdout: t.bool_or(cx, "report_stdout", false),
        };
        t.finish(cx);
        rules.push(rule);
    }
    rules
}

fn read_when(rule: &mut Tbl<'_, '_>, cx: &mut Ctx<'_>) -> When {
    let span = rule.span_of("when");
    let mut w = rule.table(cx, "when");
    let present = [w.has("docker_event"), w.has("path_changed"), w.has("failing")];
    if present.iter().filter(|p| **p).count() != 1 {
        cx.err(&span, &w.path, "exactly one of docker_event, path_changed, failing is required");
    }
    let when = if present[0] {
        let mut e = w.table(cx, "docker_event");
        let when = When::DockerEvent { container: e.string_req(cx, "container"), action: e.string_req(cx, "action") };
        e.finish(cx);
        when
    } else if present[1] {
        When::PathChanged(w.path_opt(cx, "path_changed").unwrap_or_default())
    } else {
        When::Failing(w.strings_or(cx, "failing", &[]))
    };
    w.finish(cx);
    when
}

fn read_action(rule: &mut Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Action {
    let span = rule.span_of("action");
    let mut a = rule.table(cx, "action");
    let present = [a.has("network_connect"), a.has("run"), a.has("compose_up"), a.has("restart")];
    if present.iter().filter(|p| **p).count() != 1 {
        cx.err(&span, &a.path, "exactly one of network_connect, run, compose_up, restart is required");
    }
    let action = if present[0] {
        Action::NetworkConnect { network_connect: container_network(a.table(cx, "network_connect"), cx) }
    } else if present[1] {
        Action::Run { run: a.path_opt(cx, "run").unwrap_or_default() }
    } else if present[2] {
        Action::ComposeUp { compose_up: a.path_opt(cx, "compose_up").unwrap_or_default() }
    } else {
        let wait_healthy = a.has("wait_healthy").then(|| {
            let mut w = a.table(cx, "wait_healthy");
            let wait = WaitHealthy {
                container: w.string_req(cx, "container"),
                timeout: w.dur_or(cx, "timeout", Dur::from_secs(120)),
            };
            w.finish(cx);
            wait
        });
        Action::Restart {
            restart: a.strings_or(cx, "restart", &[]),
            wait_healthy,
            then_restart: a.strings_or(cx, "then_restart", &[]),
        }
    };
    // Read in every form so they count as known keys; outside a restart they are a mistake, not a typo.
    let restart_only = [a.has("wait_healthy"), a.has("then_restart")].contains(&true);
    if restart_only && !present[3] && present.iter().filter(|p| **p).count() == 1 {
        cx.err(&span, &a.path, "wait_healthy and then_restart only apply to a restart action");
    }
    a.finish(cx);
    action
}

// -------------------------------------------------------------------------------------------- boot, firewall

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Boot {
    /// Waited for before stacks are started.
    pub wait_for_mounts: Vec<String>,
    /// Stacks to `up -d` at boot, in this order (globs sorted).
    pub compose_up: Vec<String>,
    /// Run `moi verify` this long after boot.
    pub verify_after: Dur,
    pub verify_monitor: Option<String>,
}

impl Boot {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>, used: &mut MonitorNames) -> Self {
        let this = Self {
            wait_for_mounts: t.paths_or(cx, "wait_for_mounts", &[]),
            compose_up: t.paths_or(cx, "compose_up", &[]),
            verify_after: t.dur_or(cx, "verify_after", Dur::from_secs(240)),
            verify_monitor: used.read(&mut t, cx, "verify_monitor"),
        };
        t.finish(cx);
        this
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Firewall {
    /// moi applies the rules files at boot.
    pub manage: bool,
    /// The only chains moi owns.
    pub chains: Vec<String>,
    pub rules_v4: String,
    pub rules_v6: String,
}

impl Firewall {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let this = Self {
            manage: t.bool_or(cx, "manage", false),
            chains: t.checked_list(cx, "chains", &["INPUT", "DOCKER-USER"], |c| {
                if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
                    Ok(())
                } else {
                    Err("not a chain name".to_string())
                }
            }),
            rules_v4: t.path_or(cx, "rules_v4", "/etc/moi/firewall/rules4.txt"),
            rules_v6: t.path_or(cx, "rules_v6", "/etc/moi/firewall/rules6.txt"),
        };
        t.finish(cx);
        this
    }
}

// -------------------------------------------------------------------------------------------- backup

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Backup {
    pub enabled: bool,
    /// Daily, systemd `OnCalendar` syntax; missed runs happen 5 min after boot.
    pub schedule: String,
    /// Required when backups are enabled.
    pub local_dir: Option<String>,
    /// Never write backups elsewhere if this is not mounted.
    pub require_mount: Option<String>,
    pub keep_local: Dur,
    /// Total local backups above this = failed run.
    pub max_local: Size,
    /// Identity data beyond auto-discovery.
    pub paths: Vec<String>,
    pub exclude: Vec<String>,
    /// `"auto"`: every running Postgres/MySQL/MariaDB container; or container names.
    pub databases: AutoOr<String>,
    /// `"auto"`: every named volume that is neither a DB data dir nor fungible; or volume names.
    pub volumes: AutoOr<String>,
    /// Image substring → dump user.
    pub special_users: BTreeMap<String, String>,
    /// Legacy-format log lines are also appended here.
    pub log_file: Option<String>,
    pub offsite: Offsite,
}

impl Backup {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let enabled = t.bool_or(cx, "enabled", t.present());
        if enabled && !t.has("local_dir") {
            t.missing(cx, "local_dir");
        }
        let this = Self {
            enabled,
            schedule: t.checked(cx, "schedule", schedule).unwrap_or_else(|| "03:30".to_string()),
            local_dir: t.path_opt(cx, "local_dir"),
            require_mount: t.path_opt(cx, "require_mount"),
            keep_local: t.dur_or(cx, "keep_local", Dur::from_secs(7 * 86_400)),
            max_local: t.size_or(cx, "max_local", Size::from_bytes(20 * GB)),
            paths: t.paths_or(cx, "paths", &[]),
            exclude: t.strings_or(cx, "exclude", &["*/cache", "*/Cache", "*/transcodes", "*/logs", "*/log"]),
            databases: t.auto_or_list(cx, "databases", |_| Ok(())),
            volumes: t.auto_or_list(cx, "volumes", |_| Ok(())),
            special_users: t.string_map(cx, "special_users").into_iter().map(|(k, v, _)| (k.value, v.value)).collect(),
            log_file: t.path_opt(cx, "log_file"),
            offsite: Offsite::read(t.table(cx, "offsite"), cx),
        };
        t.finish(cx);
        this
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Offsite {
    pub enabled: bool,
    /// restic repository URL. Required when enabled.
    pub repository: Option<Secret>,
    /// Required when enabled.
    pub password: Option<Secret>,
    /// Environment for restic (cloud credentials).
    pub env: BTreeMap<String, Secret>,
    pub tag: String,
    /// Skip the upload if the last one is more recent.
    pub min_interval: Dur,
    pub pack_size: Size,
    pub keep: Keep,
    /// Weekday of the weekly prune.
    pub prune_on: String,
    /// Share of the repository `restic check` reads each run.
    pub check_subset: String,
    /// Repository size above this = failed run.
    pub max_repo: Size,
    pub r2_guard: R2Guard,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Keep {
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
}

impl Offsite {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let enabled = t.bool_or(cx, "enabled", t.present());
        if enabled {
            for key in ["repository", "password"] {
                if !t.has(key) {
                    t.missing(cx, key);
                }
            }
        }
        let mut env = BTreeMap::new();
        for (name, value, was_ref) in t.string_map(cx, "env") {
            if !was_ref && redact::is_sensitive_key(&name.value) {
                cx.err(&value.span, &format!("{}.{}", t.key("env"), name.value), NOT_A_REFERENCE);
            }
            env.insert(name.value, Secret::new(value.value));
        }
        let mut keep = t.table(cx, "keep");
        let keep_counts = Keep {
            daily: keep.u32_or(cx, "daily", 7, 0),
            weekly: keep.u32_or(cx, "weekly", 4, 0),
            monthly: keep.u32_or(cx, "monthly", 6, 0),
        };
        keep.finish(cx);
        let this = Self {
            enabled,
            repository: t.secret(cx, "repository", false),
            password: t.secret(cx, "password", true),
            env,
            tag: t.string_or(cx, "tag", "nightly"),
            min_interval: t.dur_or(cx, "min_interval", Dur::from_secs(20 * 3600)),
            pack_size: t.size_or(cx, "pack_size", Size::from_bytes(64 * MB)),
            keep: keep_counts,
            prune_on: t.checked(cx, "prune_on", weekday).unwrap_or_else(|| "Sun".to_string()),
            check_subset: t.checked(cx, "check_subset", subset).unwrap_or_else(|| "2%".to_string()),
            max_repo: t.size_or(cx, "max_repo", Size::from_bytes(8 * GB)),
            r2_guard: R2Guard::read(t.table(cx, "r2_guard"), cx),
        };
        t.finish(cx);
        this
    }
}

/// Refuses to upload past half the Cloudflare R2 free tier; fails closed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct R2Guard {
    pub enabled: bool,
    pub account_id: Option<Secret>,
    pub api_token: Option<Secret>,
    pub max_class_a: u64,
    pub max_class_b: u64,
    pub max_storage: Size,
}

impl R2Guard {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let enabled = t.bool_or(cx, "enabled", t.present());
        if enabled {
            for key in ["account_id", "api_token"] {
                if !t.has(key) {
                    t.missing(cx, key);
                }
            }
        }
        let this = Self {
            enabled,
            account_id: t.secret(cx, "account_id", false),
            api_token: t.secret(cx, "api_token", true),
            max_class_a: t.int_or(cx, "max_class_a", 500_000, 0, u64::MAX),
            max_class_b: t.int_or(cx, "max_class_b", 5_000_000, 0, u64::MAX),
            max_storage: t.size_or(cx, "max_storage", Size::from_bytes(8 * GB)),
        };
        t.finish(cx);
        this
    }
}

// -------------------------------------------------------------------------------------------- restore, monitors

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Restore {
    /// Added to the built-in list of `/etc` paths put back on a rebuild.
    pub etc_paths: Vec<String>,
    /// Put back package config files the owner changed (diff shown).
    pub restore_modified_conffiles: bool,
}

impl Restore {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let this = Self {
            etc_paths: t.paths_or(cx, "etc_paths", &[]),
            restore_modified_conffiles: t.bool_or(cx, "restore_modified_conffiles", true),
        };
        t.finish(cx);
        this
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorMode {
    /// Push results.
    Live,
    /// Run checks, push nothing (migration, 11).
    Shadow,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Monitors {
    pub mode: MonitorMode,
    pub kuma: Kuma,
    pub healthchecks: Healthchecks,
}

impl Monitors {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let mode = match t.string(cx, "mode") {
            None => MonitorMode::Live,
            Some(s) if s.value == "live" => MonitorMode::Live,
            Some(s) if s.value == "shadow" => MonitorMode::Shadow,
            Some(s) => {
                cx.err(&s.span, &t.key("mode"), "must be \"live\" or \"shadow\"");
                MonitorMode::Live
            }
        };
        let kuma = Kuma::read(t.table(cx, "kuma"), cx);
        let mut hc = t.table(cx, "healthchecks");
        let healthchecks = Healthchecks { expect_watchdog: hc.bool_or(cx, "expect_watchdog", false) };
        hc.finish(cx);
        t.finish(cx);
        Self { mode, kuma, healthchecks }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Kuma {
    pub enabled: bool,
    /// Required when enabled.
    pub url: Option<String>,
    pub username: Option<Secret>,
    pub password: Option<Secret>,
    /// Existing notifications attached to monitors moi creates.
    pub notifications: Vec<String>,
    /// Group monitor for created monitors; empty = none.
    pub group: String,
    /// One Kuma `docker` monitor per container.
    pub containers: bool,
    /// Kuma docker host to use or create.
    pub docker_host: String,
    /// Push monitors for snapshot and backup freshness.
    pub freshness: bool,
    /// Base URL for pushes; empty = `url`.
    pub push_base: String,
}

impl Kuma {
    fn read(mut t: Tbl<'_, '_>, cx: &mut Ctx<'_>) -> Self {
        let enabled = t.bool_or(cx, "enabled", t.present());
        if enabled && !t.has("url") {
            t.missing(cx, "url");
        }
        let this = Self {
            enabled,
            url: t.checked(cx, "url", http_url),
            username: t.secret(cx, "username", false),
            password: t.secret(cx, "password", true),
            notifications: t.strings_or(cx, "notifications", &[]),
            group: t.string_or(cx, "group", ""),
            containers: t.bool_or(cx, "containers", true),
            docker_host: t.string_or(cx, "docker_host", "local"),
            freshness: t.bool_or(cx, "freshness", true),
            push_base: t
                .checked(cx, "push_base", |s| if s.is_empty() { Ok(()) } else { http_url(s) })
                .unwrap_or_default(),
        };
        t.finish(cx);
        this
    }
}

/// Informational: moi detects and preserves a Kuma → healthchecks.io watchdog; it never edits it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Healthchecks {
    pub expect_watchdog: bool,
}

// -------------------------------------------------------------------------------------------- value checks

fn http_url(s: &str) -> Result<(), String> {
    let rest = s.strip_prefix("http://").or_else(|| s.strip_prefix("https://"));
    match rest {
        Some(r) if !r.is_empty() && !r.starts_with('/') => Ok(()),
        _ => Err("must start with http:// or https://".to_string()),
    }
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

fn is_weekday(s: &str) -> bool {
    WEEKDAYS.iter().any(|d| s.len() >= 3 && s.is_char_boundary(3) && d.eq_ignore_ascii_case(&s[..3]))
        && ["", "day", "sday", "nesday", "rsday", "urday"].iter().any(|rest| s[3..].eq_ignore_ascii_case(rest))
}

fn weekday(s: &str) -> Result<(), String> {
    if is_weekday(s) { Ok(()) } else { Err("must be a weekday (Mon, Tue, Wed, Thu, Fri, Sat, Sun)".to_string()) }
}

/// A percentage (`"2%"`) or a size (`"500MB"`) of the repository.
fn subset(s: &str) -> Result<(), String> {
    let percent = s.strip_suffix('%').is_some_and(|n| n.parse::<f64>().is_ok_and(|p| p > 0.0 && p <= 100.0));
    if percent || s.parse::<Size>().is_ok() {
        Ok(())
    } else {
        Err("must be a percentage like \"2%\" or a size like \"500MB\"".to_string())
    }
}

/// The subset of systemd `OnCalendar` syntax moi schedules with: `[weekdays] [date] time` or a
/// shorthand. The scheduler (10) is the authority; this catches typos at config time.
fn schedule(s: &str) -> Result<(), String> {
    let err = || {
        "not a valid schedule: use systemd OnCalendar syntax (e.g. \"03:30\", \"Sun 04:00\", \"*-*-* 03:30:00\")"
            .to_string()
    };
    if ["minutely", "hourly", "daily", "weekly", "monthly", "quarterly", "yearly", "annually"].contains(&s) {
        return Ok(());
    }
    let mut parts: Vec<&str> = s.split_whitespace().collect();
    let field = |f: &str, sep: char, min: usize, max: usize| {
        let items: Vec<&str> = f.split(sep).collect();
        (min..=max).contains(&items.len())
            && items.iter().all(|item| {
                !item.is_empty()
                    && item.split(',').all(|alt| {
                        let (range, step) = alt.split_once('/').unwrap_or((alt, "1"));
                        let ends: Vec<&str> = range.split("..").collect();
                        step.parse::<u32>().is_ok()
                            && ends.len() <= 2
                            && ends
                                .iter()
                                .all(|e| *e == "*" || (!e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())))
                    })
            })
    };
    let Some(time) = parts.pop() else { return Err(err()) };
    if !field(time, ':', 2, 3) {
        return Err(err());
    }
    let has_date = parts.last().is_some_and(|p| p.contains('-') && !p.chars().any(|c| c.is_ascii_alphabetic()));
    if has_date && !parts.pop().is_some_and(|date| field(date, '-', 2, 3)) {
        return Err(err());
    }
    match parts.as_slice() {
        [] => Ok(()),
        [days] if days.split(',').all(|d| d.split("..").all(is_weekday)) => Ok(()),
        _ => Err(err()),
    }
}
