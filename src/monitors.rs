//! Uptime Kuma: what moi wants monitored, what Kuma has, and the plan that reconciles the two without
//! duplicating or severing anything (docs/spec/06-monitors.md).

pub mod apply;
pub mod inventory;
pub mod kuma;
pub mod socket;

use std::collections::BTreeMap;

use serde::Serialize;

pub use inventory::{Inventory, KumaMonitor};

use crate::config::{Config, MonitorMode};
use crate::docker::Docker;
use crate::error::Error;
use crate::host::Host;
use crate::state::StateDir;
use crate::state::db::Db;

/// The tag on every monitor moi created.
pub const TAG: &str = "moi";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Push,
    Docker,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Push => "push",
            Kind::Docker => "docker",
        }
    }
}

/// A monitor moi wants to exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Desired {
    /// What it is for: `check:resources`, `check:backup`, `freshness:snapshot`, `boot:verify`,
    /// `container:jellyfin`. The key of its binding in `moi.db`.
    pub owner: String,
    /// The name it gets when moi creates it.
    pub name: String,
    pub kind: Kind,
    /// Seconds between expected heartbeats.
    pub interval: u64,
    pub retries: u64,
    /// For docker monitors.
    pub container: Option<String>,
}

/// What moi wants monitored, in plan order.
pub fn desired(config: &Config, docker: &Docker) -> Vec<Desired> {
    let kuma = &config.monitors.kuma;
    let server = &config.server.name;
    let mut out = Vec::new();
    let mut push = |owner: String, name: &str, interval: u64| {
        out.push(Desired { owner, name: name.to_string(), kind: Kind::Push, interval, retries: 1, container: None });
    };
    // a check may be late by a fifth of its interval before Kuma calls it down (5 min → 360 s)
    for check in crate::checks::enabled(config) {
        if let Some(monitor) = check.monitor(config) {
            push(format!("check:{}", check.name()), &monitor, check.interval(config).as_secs() * 6 / 5);
        }
    }
    if let Some(monitor) = &config.checks.backup.monitor {
        push("check:backup".to_string(), monitor, 25 * 3600);
    }
    if kuma.freshness {
        push("freshness:snapshot".to_string(), &format!("{server}: snapshot"), 26 * 3600);
    }
    if let Some(monitor) = &config.boot.verify_monitor {
        // pushed once per boot: no heartbeat is expected in between
        push("boot:verify".to_string(), monitor, 30 * 86_400);
    }
    if kuma.containers {
        for container in docker.containers_inspect().unwrap_or_default() {
            let swarm_task =
                container["Config"]["Labels"]["com.docker.swarm.task.id"].as_str().is_some_and(|id| !id.is_empty());
            let restarts =
                !matches!(container["HostConfig"]["RestartPolicy"]["Name"].as_str().unwrap_or_default(), "" | "no");
            let name = container["Name"].as_str().unwrap_or_default().trim_start_matches('/');
            if !swarm_task && restarts && !name.is_empty() {
                out.push(Desired {
                    owner: format!("container:{name}"),
                    name: format!("{server}: {name}"),
                    kind: Kind::Docker,
                    interval: 60,
                    retries: 1,
                    container: Some(name.to_string()),
                });
            }
        }
    }
    out
}

/// What `moi.db` knows about a monitor it uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Binding {
    pub owner: String,
    pub monitor_name: String,
    pub kuma_id: Option<i64>,
    pub push_token: Option<String>,
    pub created_by_moi: bool,
}

/// Every binding, by owner.
pub fn bindings(db: &Db) -> Result<Vec<Binding>, Error> {
    let err = |e: rusqlite::Error| Error::Db(e.to_string());
    let mut stmt = db
        .conn()
        .prepare("SELECT owner, monitor_name, kuma_id, push_token, created_by_moi FROM monitor_bindings ORDER BY owner")
        .map_err(err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Binding {
                owner: r.get(0)?,
                monitor_name: r.get(1)?,
                kuma_id: r.get(2)?,
                push_token: r.get(3)?,
                created_by_moi: r.get::<_, i64>(4)? != 0,
            })
        })
        .map_err(err)?;
    rows.collect::<Result<_, _>>().map_err(err)
}

/// A legacy script that pushes to a Kuma monitor and that a moi check replaces (11): what lets moi
/// adopt that monitor by its token even if the names differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Feeder {
    pub owner: String,
    pub token: String,
    pub file: String,
}

/// The legacy feeders among the push integrations the last snapshot found: a script named like the
/// check, like `<check>-check`, or like the check's journal tag (`resource-check.sh` → the check
/// logging as `resource-check`). Stale copies and scripts nothing calls do not count.
pub fn feeders(host: &dyn Host, config: &Config, state: &StateDir) -> Vec<Feeder> {
    let file = state.snapshot().join("monitoring/integrations.json");
    let found: serde_json::Value =
        host.read(&file).ok().and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or_default();
    let mut names: Vec<(String, String)> = Vec::new();
    for check in crate::checks::enabled(config) {
        let name = check.name();
        names.push((check.journal_tag(config), format!("check:{name}")));
        names.push((format!("{name}-check"), format!("check:{name}")));
        names.push((name.clone(), format!("check:{name}")));
    }
    if config.checks.backup.monitor.is_some() {
        names.push(("backup".to_string(), "check:backup".to_string()));
    }
    let mut out: Vec<Feeder> = Vec::new();
    for item in found.as_array().into_iter().flatten() {
        let live = item["kind"] == "uptime-kuma-push"
            && item["stale_copy"] != true
            && item["called_by"].as_array().is_some_and(|c| !c.is_empty());
        let (Some(path), Some(token)) = (item["file"].as_str(), item["token"].as_str()) else { continue };
        let stem = path.rsplit('/').next().unwrap_or(path).split('.').next().unwrap_or_default();
        if let Some((_, owner)) = names.iter().find(|(name, _)| live && name == stem)
            && !out.iter().any(|f| f.owner == *owner)
        {
            out.push(Feeder { owner: owner.clone(), token: token.to_string(), file: path.to_string() });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Bound already, or not moi's business.
    Keep,
    /// An existing monitor becomes this owner's.
    Adopt,
    Create,
    /// A setting differs from what moi would choose; changed only with `--update`.
    Drift,
    Update,
    /// A monitor of that name exists with another type: nothing is done.
    Conflict,
    /// A monitor moi created whose target is gone.
    Pause,
    Delete,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Keep => "keep",
            Action::Adopt => "adopt",
            Action::Create => "create",
            Action::Drift => "drift",
            Action::Update => "update",
            Action::Conflict => "conflict",
            Action::Pause => "pause",
            Action::Delete => "delete",
        }
    }
}

/// One line of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub action: Action,
    pub monitor: String,
    /// Kuma's type name.
    pub kind: String,
    pub detail: String,
    /// What it is for, when it is moi's.
    pub owner: Option<String>,
    pub kuma_id: Option<i64>,
    /// The push token to bind. Never printed whole and never serialized.
    #[serde(skip)]
    pub token: Option<String>,
    /// What to create or update.
    #[serde(skip)]
    pub desired: Option<Desired>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub warnings: Vec<String>,
}

/// How the plan may change Kuma.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Apply drift.
    pub update: bool,
    /// Delete orphans instead of pausing them.
    pub prune: bool,
}

/// The last four characters of a token: enough to recognise it, useless to push with.
fn token_hint(token: &str) -> String {
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("token …{tail}")
}

/// Reconcile what moi wants with what Kuma has. Pure: nothing is read or changed here.
pub fn plan(
    config: &Config,
    desired: &[Desired],
    inventory: &Inventory,
    bindings: &[Binding],
    feeders: &[Feeder],
    options: Options,
) -> Plan {
    let kuma = &config.monitors.kuma;
    let mut plan = Plan::default();
    let mut claimed: BTreeMap<i64, String> = BTreeMap::new();
    let compatible = |monitor: &KumaMonitor, want: &Desired| monitor.kind == want.kind.as_str();
    for want in desired {
        let bound = bindings.iter().find(|b| b.owner == want.owner).and_then(|b| inventory.by_id(b.kuma_id?));
        let feeder = feeders.iter().find(|f| f.owner == want.owner);
        let by_token = feeder.and_then(|f| {
            inventory.monitors.iter().find(|m| m.kind == "push" && m.push_token.as_deref() == Some(f.token.as_str()))
        });
        let by_name = inventory.monitors.iter().find(|m| m.name == want.name);
        let by_container = want.container.as_ref().and_then(|c| {
            inventory.monitors.iter().find(|m| m.kind == "docker" && m.docker_container.as_ref() == Some(c))
        });
        let token_detail = |m: &KumaMonitor| m.push_token.as_deref().map(token_hint);
        // the first rule that matches wins
        let (action, monitor, detail) = if let Some(m) = bound {
            (Action::Keep, Some(m), format!("bound to {}", want.owner))
        } else if let (Some(m), Some(f)) = (by_token, feeder) {
            (Action::Adopt, Some(m), format!("{} (fed by {} — migration map)", token_hint(&f.token), f.file))
        } else if let Some(m) = by_name.filter(|m| compatible(m, want)) {
            (Action::Adopt, Some(m), token_detail(m).unwrap_or_else(|| "same name".to_string()))
        } else if let Some(m) = by_name {
            (
                Action::Conflict,
                Some(m),
                format!("exists as a {} monitor, moi wants {}: rename one of them", m.kind, want.kind.as_str()),
            )
        } else if let Some(m) = by_container {
            (Action::Adopt, Some(m), format!("watches container {}", want.container.as_deref().unwrap_or_default()))
        } else {
            let detail = match want.kind {
                Kind::Push => format!("interval {}s", want.interval),
                Kind::Docker => format!("host {}", kuma.docker_host),
            };
            (Action::Create, None, detail)
        };
        if let Some(m) = monitor {
            claimed.insert(m.id, want.owner.clone());
        }
        plan.steps.push(Step {
            action,
            monitor: monitor.map_or_else(|| want.name.clone(), |m| m.name.clone()),
            kind: monitor.map_or_else(|| want.kind.as_str().to_string(), |m| m.kind.clone()),
            detail,
            owner: Some(want.owner.clone()),
            kuma_id: monitor.map(|m| m.id),
            token: monitor.and_then(|m| m.push_token.clone()),
            desired: Some(want.clone()),
        });
        // what an existing monitor does differently from moi's defaults
        if let Some(m) = monitor.filter(|_| action != Action::Conflict) {
            let mut differs = Vec::new();
            if m.interval.is_some_and(|have| u64::try_from(have).ok() != Some(want.interval)) {
                differs.push(format!("interval {} (moi default {})", m.interval.unwrap_or_default(), want.interval));
            }
            if !m.active {
                differs.push("paused".to_string());
            }
            if !differs.is_empty() {
                plan.steps.push(Step {
                    action: if options.update { Action::Update } else { Action::Drift },
                    monitor: m.name.clone(),
                    kind: m.kind.clone(),
                    detail: differs.join(", "),
                    owner: Some(want.owner.clone()),
                    kuma_id: Some(m.id),
                    token: None,
                    desired: Some(want.clone()),
                });
            }
        }
    }
    // monitors moi created whose target is gone
    for binding in bindings.iter().filter(|b| b.created_by_moi && !desired.iter().any(|d| d.owner == b.owner)) {
        let Some(m) = binding.kuma_id.and_then(|id| inventory.by_id(id)).filter(|m| m.tags.iter().any(|t| t == TAG))
        else {
            continue;
        };
        claimed.insert(m.id, binding.owner.clone());
        if !options.prune && !m.active {
            continue;
        }
        plan.steps.push(Step {
            action: if options.prune { Action::Delete } else { Action::Pause },
            monitor: m.name.clone(),
            kind: m.kind.clone(),
            detail: format!("{} is gone", binding.owner),
            owner: Some(binding.owner.clone()),
            kuma_id: Some(m.id),
            token: None,
            desired: None,
        });
    }
    // everything else is somebody else's and stays exactly as it is
    let mut watchdog = false;
    for m in inventory.monitors.iter().filter(|m| !claimed.contains_key(&m.id)) {
        watchdog |= m.is_watchdog() && m.active;
        plan.steps.push(Step {
            action: Action::Keep,
            monitor: m.name.clone(),
            kind: m.kind.clone(),
            detail: if m.is_watchdog() { "not managed by moi (watchdog)" } else { "not managed by moi" }.to_string(),
            owner: None,
            kuma_id: Some(m.id),
            token: None,
            desired: None,
        });
    }
    if config.monitors.healthchecks.expect_watchdog && !watchdog {
        plan.warnings.push(
            "no active healthchecks.io watchdog monitor: nothing will notice if Uptime Kuma or this server stops"
                .to_string(),
        );
    }
    for name in &kuma.notifications {
        if !inventory.notifications.iter().any(|(_, have)| have == name) {
            plan.warnings.push(format!(
                "notification \"{name}\" does not exist in Uptime Kuma: created monitors would alert nobody"
            ));
        }
    }
    plan
}

/// Columns padded to their widest cell.
pub fn table(header: [&str; 4], rows: &[[String; 4]]) -> String {
    let mut widths = header.map(|h| h.chars().count());
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; 4]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i == 3 {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}  ", width = widths[i]));
            }
        }
        format!("{}\n", out.trim_end())
    };
    let mut out = line(header);
    for row in rows {
        out.push_str(&line([&row[0], &row[1], &row[2], &row[3]]));
    }
    out
}

impl Plan {
    /// The plan as the table `moi monitors sync --dry-run` prints.
    pub fn render(&self) -> String {
        let rows: Vec<[String; 4]> = self
            .steps
            .iter()
            .map(|s| [s.action.as_str().to_string(), s.monitor.clone(), s.kind.clone(), s.detail.clone()])
            .collect();
        table(["ACTION", "MONITOR", "TYPE", "DETAIL"], &rows)
    }

    /// Whether applying it would change anything in Kuma or in `moi.db`.
    pub fn changes(&self) -> usize {
        self.steps.iter().filter(|s| !matches!(s.action, Action::Keep | Action::Drift | Action::Conflict)).count()
    }
}

/// The inventory without credentials: from Kuma's own database on this machine.
pub fn local_inventory(host: &dyn Host, config: &Config, docker: &Docker) -> Result<Inventory, Error> {
    if !config.monitors.kuma.enabled {
        return Err(Error::Usage(
            "monitors are not configured: add a [monitors.kuma] section with the url of your Uptime Kuma to moi.toml"
                .to_string(),
        ));
    }
    let located = inventory::locate(host, docker).ok_or_else(|| {
        Error::Usage(
            "cannot find Uptime Kuma's database: no container of an uptime-kuma image with a host directory mounted at /app/data (a Kuma on another machine cannot be planned for without it)"
                .to_string(),
        )
    })?;
    inventory::read(host, &located)
        .ok_or_else(|| Error::Usage(format!("cannot read {located} (read-only): is it Uptime Kuma's database?")))
}

/// Log in to Kuma's socket API with the configured user.
pub fn connect(host: &std::sync::Arc<dyn Host>, config: &Config) -> Result<kuma::Kuma, Error> {
    let settings = &config.monitors.kuma;
    let (Some(url), true) = (&settings.url, settings.enabled) else {
        return Err(Error::Usage(
            "monitors are not configured: add a [monitors.kuma] section with the url of your Uptime Kuma to moi.toml"
                .to_string(),
        ));
    };
    let (Some(username), Some(password)) = (&settings.username, &settings.password) else {
        return Err(Error::Usage(
            "changing monitors needs a Uptime Kuma login: set username and password in [monitors.kuma] (as secret:… references) — `moi monitors sync --dry-run` works without".to_string(),
        ));
    };
    let mut kuma = kuma::Kuma::connect(host, url).map_err(Error::Monitors)?;
    kuma.login(username.expose(), password.expose()).map_err(Error::Monitors)?;
    Ok(kuma)
}

/// The plan for an inventory, with everything else gathered from the machine.
pub fn plan_for(
    host: &dyn Host,
    config: &Config,
    docker: &Docker,
    state: &StateDir,
    db: &Db,
    inventory: &Inventory,
    options: Options,
) -> Result<Plan, Error> {
    let wanted = desired(config, docker);
    Ok(plan(config, &wanted, inventory, &bindings(db)?, &feeders(host, config, state), options))
}

/// Push one status and one message to the push monitor bound to `owner`. `Ok` when Kuma took it;
/// otherwise why nothing was pushed — never a reason to fail what was being reported.
pub fn push(
    host: &std::sync::Arc<dyn Host>,
    config: &Config,
    db: &Db,
    owner: &str,
    status: &str,
    message: &str,
) -> Result<(), String> {
    let kuma = &config.monitors.kuma;
    if config.monitors.mode == MonitorMode::Shadow {
        return Err("not pushed: monitors.mode is shadow".to_string());
    }
    let base = if kuma.push_base.is_empty() { kuma.url.clone().unwrap_or_default() } else { kuma.push_base.clone() };
    if !kuma.enabled || base.is_empty() {
        return Err("not pushed: no [monitors.kuma] url".to_string());
    }
    let token: Option<String> = db
        .conn()
        .query_row(
            "SELECT push_token FROM monitor_bindings WHERE owner = ?1 AND push_token IS NOT NULL",
            [owner],
            |r| r.get(0),
        )
        .ok();
    let Some(token) = token else {
        return Err(format!("not pushed: no monitor is bound to {owner} yet (run `moi monitors sync`)"));
    };
    let url = format!(
        "{}/api/push/{}?status={status}&msg={}&ping=",
        base.trim_end_matches('/'),
        crate::net::encode(&token),
        crate::net::encode(message)
    );
    let client = crate::net::Client::new(std::sync::Arc::clone(host)).with_timeout(std::time::Duration::from_secs(15));
    let mut last = String::new();
    // one try and two retries, as `curl --retry 2` did
    for _ in 0..3 {
        match client.send("GET", &url, &[], None) {
            Ok(response) if response.is_success() => return Ok(()),
            Ok(response) => last = format!("HTTP {}", response.status),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("push failed: {last}"))
}

/// What the daemon does on its own when containers come and go (and nothing else): create the docker
/// monitors of new containers, pause moi's monitors of containers that are gone. Returns what was done,
/// or `None` when there was nothing to do or moi has no login.
pub fn sync_containers(
    host: &std::sync::Arc<dyn Host>,
    config: &Config,
    docker: &Docker,
    state: &StateDir,
    db: &Db,
) -> Result<Option<apply::Report>, Error> {
    let settings = &config.monitors.kuma;
    if !settings.enabled || !settings.containers || settings.username.is_none() || settings.password.is_none() {
        return Ok(None);
    }
    // Only once the owner has synced by hand, and never while migrating: the daemon keeps a set of
    // monitors current, it does not introduce one.
    if config.monitors.mode == MonitorMode::Shadow || bindings(db)?.is_empty() {
        return Ok(None);
    }
    let own = |step: &Step| {
        step.owner.as_deref().is_some_and(|o| o.starts_with("container:"))
            && matches!(step.action, Action::Create | Action::Pause)
    };
    // look first without logging in: most changes on a server need no monitor change
    if let Ok(local) = local_inventory(host.as_ref(), config, docker)
        && !plan_for(host.as_ref(), config, docker, state, db, &local, Options::default())?.steps.iter().any(own)
    {
        return Ok(None);
    }
    let mut kuma = connect(host, config)?;
    let live = kuma.inventory().map_err(Error::Monitors)?;
    let mut plan = plan_for(host.as_ref(), config, docker, state, db, &live, Options::default())?;
    plan.steps.retain(own);
    if plan.steps.is_empty() {
        return Ok(None);
    }
    apply::apply(host, config, docker, db, &mut kuma, &plan, &live).map(Some)
}
