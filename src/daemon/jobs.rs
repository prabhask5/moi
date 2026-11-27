//! What the daemon's jobs do.

use std::sync::Arc;
use std::time::Instant;

use super::{Core, Outcome, Work};
use crate::checks::{self, Mode, Status};
use crate::error::Error;
use crate::log::Logger;
use crate::monitors;

/// Run a job to its end. Never fails: a job that could not do its work says so in its outcome.
pub(crate) fn run(core: &Arc<Core>, work: &Work) -> Outcome {
    let started = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match work {
        Work::Check(check) => self::check(core, check),
        Work::Daily => daily(core).map(|message| ("ok".to_string(), message)),
        Work::Snapshot => snapshot(core).map(|message| ("ok".to_string(), message)),
        Work::Backup => {
            let db = core.state.open_db(core.host.as_ref())?;
            let report =
                crate::backup::run::run(&core.host, &core.config, &core.state, &db, &core.masker, &mut |_| {})?;
            let message = report.error.clone().or(report.dir.clone()).unwrap_or_default();
            Ok((if report.ok { "ok" } else { "failed" }.to_string(), message))
        }
        Work::Drill => {
            let db = core.state.open_db(core.host.as_ref())?;
            let report = crate::backup::drill::run(&core.host, &core.config, &core.state, &db)?;
            let failed = report.lines.iter().find(|l| l.contains("FAILED")).cloned();
            Ok((
                if report.ok { "ok" } else { "failed" }.to_string(),
                failed.unwrap_or_else(|| format!("{} checks on {}", report.lines.len(), report.dir)),
            ))
        }
        Work::Boot => boot(core),
        Work::Firewall => firewall(core, &core.config.firewall.chains).map(|message| ("ok".to_string(), message)),
        Work::Heal(rule, why) => {
            let healer = crate::heal::Healer {
                host: Arc::clone(&core.host),
                docker: crate::docker::Docker::new(Arc::clone(&core.host)),
                state: core.state.clone(),
                act: true,
            };
            // a rule with a precondition acts only when it does not hold (the proxy is still on its network: nothing to do)
            let report = if healer.needed(rule) { healer.fire_and_queue(rule, why) } else { None };
            Ok(("ok".to_string(), report.unwrap_or_else(|| "nothing to do".to_string())))
        }
    }));
    let (status, message) = match result {
        Ok(Ok(done)) => done,
        Ok(Err(e)) => ("failed".to_string(), e.to_string()),
        Err(_) => ("failed".to_string(), "the job crashed".to_string()),
    };
    Outcome { status, message, duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX) }
}

/// One scheduled run of a check: recorded, pushed to its monitor, written to the journal.
fn check(core: &Arc<Core>, check: &Arc<dyn checks::Check>) -> Result<(String, String), Error> {
    let db = core.state.open_db(core.host.as_ref())?;
    let mut result = checks::run(&core.host, &core.config, &db, check, Mode::Record)?;
    let log = Logger::new(Arc::clone(&core.host), &check.journal_tag(&core.config)).with_masker(core.masker.clone());
    let line = format!("{}: {}", result.status.as_str(), result.message);
    match result.status {
        Status::Up => log.info(&line, &[("check", &result.name)]),
        Status::Down => log.warn(&line, &[("check", &result.name)]),
    }
    if result.monitor.is_some() {
        let pushed = checks::push(&core.host, &core.config, &db, &result);
        // a monitor that cannot be reached is worth a line; "not bound yet" and shadow mode are states
        if pushed.starts_with("push failed") {
            log.warn(&pushed, &[("check", &result.name)]);
        }
        result.pushed = Some(pushed);
    }
    Ok((result.status.as_str().to_string(), result.message))
}

/// The daily job: a full snapshot and audit whatever the watcher saw, housekeeping, and a word to the
/// freshness monitor that the server still describes itself.
fn daily(core: &Arc<Core>) -> Result<String, Error> {
    let host = &core.host;
    let db = core.state.open_db(host.as_ref())?;
    let log = Logger::new(Arc::clone(host), "moi").with_masker(core.masker.clone());
    let done = (|| -> Result<String, Error> {
        let (report, _) = crate::snapshot::capture(host, &core.config, &core.state, &db, &core.masker)?;
        let audit = crate::audit::run(host, &core.config, &core.state)?;
        crate::context::write(host.as_ref(), &core.config, &core.state, &core.masker)?;
        // what changed since yesterday's run: the commits of the last day
        let since = crate::host::unix_seconds(host.now()) - 86_400;
        let history = crate::state::history::History::open(host.as_ref(), &core.state)?;
        let changes = history
            .log(1000)?
            .iter()
            .filter(|e| crate::host::parse_rfc3339(&e.time).is_some_and(|t| crate::host::unix_seconds(t) > since))
            .count();
        let stale: Vec<&str> = report.failed().map(|c| c.name.as_str()).collect();
        let mut message = format!("snapshot OK, {changes} changes today");
        if !stale.is_empty() {
            message.push_str(&format!(" (stale: {})", stale.join(", ")));
        }
        if audit.gaps() > 0 {
            log.warn(&format!("audit: {} gaps (moi audit)", audit.gaps()), &[("job", "daily")]);
        }
        Ok(message)
    })();
    // links that have expired are deleted even if their bucket rule did not
    if core.config.context.share.enabled
        && let Err(e) = crate::context::share::expire(host, &core.config, &db)
    {
        log.warn(&format!("cannot delete expired shares: {e}"), &[("job", "daily")]);
    }
    // monitors that a sync would change: said once a day, never changed from here
    if core.config.monitors.kuma.enabled {
        let docker = crate::docker::Docker::new(Arc::clone(host));
        let plan = monitors::local_inventory(host.as_ref(), &core.config, &docker).and_then(|inventory| {
            monitors::plan_for(
                host.as_ref(),
                &core.config,
                &docker,
                &core.state,
                &db,
                &inventory,
                monitors::Options::default(),
            )
        });
        if let Ok(plan) = plan {
            let drift = plan
                .steps
                .iter()
                .filter(|s| matches!(s.action, monitors::Action::Drift | monitors::Action::Conflict))
                .count();
            if plan.changes() + drift > 0 {
                log.warn(
                    &format!(
                        "monitors: {} changes and {drift} differences pending (moi monitors sync --dry-run)",
                        plan.changes()
                    ),
                    &[("job", "daily")],
                );
            }
            for warning in &plan.warnings {
                log.warn(&format!("monitors: {warning}"), &[("job", "daily")]);
            }
        }
    }
    let (status, message) = match &done {
        Ok(message) => ("up", message.clone()),
        Err(e) => ("down", format!("snapshot failed: {e}")),
    };
    let _ = monitors::push(host, &core.config, &db, "freshness:snapshot", status, &message);
    done
}

/// The snapshot after a change, and the container monitors that follow from it.
fn snapshot(core: &Arc<Core>) -> Result<String, Error> {
    let host = &core.host;
    let db = core.state.open_db(host.as_ref())?;
    let (report, commit) = crate::snapshot::capture(host, &core.config, &core.state, &db, &core.masker)?;
    let mut message = match &commit {
        Some(entry) => format!("{} files changed: {}", report.changed.len(), entry.subject),
        None => "nothing changed".to_string(),
    };
    let docker = crate::docker::Docker::new(Arc::clone(host));
    match monitors::sync_containers(host, &core.config, &docker, &core.state, &db) {
        Ok(Some(synced)) => {
            let log = Logger::new(Arc::clone(host), "moi").with_masker(core.masker.clone());
            log.info(&format!("container monitors: {}", synced.summary()), &[("job", "snapshot")]);
            for failure in &synced.failed {
                log.warn(&format!("container monitor {}: {}", failure.monitor, failure.error), &[("job", "snapshot")]);
            }
            message.push_str(&format!("; monitors: {}", synced.summary()));
        }
        Ok(None) => {}
        Err(e) => message.push_str(&format!("; monitors not synced: {e}")),
    }
    // what changed is off-site within the hour (the nightly backup pushes it in any case)
    if commit.is_some()
        && crate::bundle::push_if_due(host, &core.config, &core.state, &db) == Some(crate::bundle::Pushed::Saved)
    {
        message.push_str("; state bundle pushed");
    }
    Ok(message)
}

/// This boot's id, as the kernel gives it.
pub(crate) fn boot_id(host: &dyn crate::host::Host) -> Option<String> {
    host.read_to_string(std::path::Path::new("/proc/sys/kernel/random/boot_id"))
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// Replace these managed chains, for both address families.
fn firewall(core: &Arc<Core>, chains: &[String]) -> Result<String, Error> {
    use crate::firewall::{self, Family};
    let db = core.state.open_db(core.host.as_ref())?;
    let mut rules = 0;
    for family in Family::ALL {
        rules += firewall::apply(core.host.as_ref(), &core.config, &db, family, chains)?.rules;
    }
    Ok(format!("firewall: {rules} rules in {}", chains.join(", ")))
}

/// Compose project directories a `boot.compose_up` entry names: the directory itself, or with `/**`
/// every directory below it (three levels) that has a compose file — in sorted order.
pub fn compose_dirs(host: &dyn crate::host::Host, entry: &str) -> Vec<String> {
    use std::path::{Path, PathBuf};
    let has_compose = |dir: &Path| {
        ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"]
            .iter()
            .any(|f| host.exists(&dir.join(f)))
    };
    let Some(top) = entry.strip_suffix("/**") else {
        return if has_compose(Path::new(entry)) { vec![entry.to_string()] } else { Vec::new() };
    };
    let mut found = Vec::new();
    let mut level: Vec<PathBuf> = vec![PathBuf::from(top)];
    for _ in 0..3 {
        let mut below = Vec::new();
        for dir in &level {
            for name in host.read_dir(dir).unwrap_or_default() {
                let child = dir.join(name);
                if host.metadata(&child).is_ok_and(|m| m.kind == crate::host::FileKind::Dir) {
                    if has_compose(&child) {
                        found.push(child.to_string_lossy().into_owned());
                    } else {
                        below.push(child);
                    }
                }
            }
        }
        level = below;
    }
    found.sort();
    found
}

/// What the first start after a boot does: the firewall's built-in chains at once, then wait for the
/// data mounts and for Docker, the rest of the firewall, and the compose stacks that are not up yet.
fn boot(core: &Arc<Core>) -> Result<(String, String), Error> {
    use std::time::Duration;
    let host = &core.host;
    let config = &core.config;
    let log = Logger::new(Arc::clone(host), "moi").with_masker(core.masker.clone());
    let (mut done, mut problems): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let wait = |ready: &dyn Fn() -> bool, limit: u64| -> bool {
        let mut waited = 0;
        while !ready() && waited < limit {
            host.sleep(Duration::from_secs(2));
            waited += 2;
        }
        ready()
    };
    // 1. the host's own firewall must not wait for anything
    let (own, later): (Vec<String>, Vec<String>) =
        config.firewall.chains.iter().cloned().partition(|chain| crate::firewall::builtin(chain));
    if config.firewall.manage && !own.is_empty() {
        match firewall(core, &own) {
            Ok(message) => done.push(message),
            Err(e) => problems.push(format!("firewall not applied: {e}")),
        }
    }
    // 2. data that stacks need
    let missing: Vec<&String> = config
        .boot
        .wait_for_mounts
        .iter()
        .filter(|mount| !wait(&|| crate::snapshot::mounts(host.as_ref()).iter().any(|m| m.target == **mount), 300))
        .collect();
    for mount in &missing {
        problems.push(format!("{mount} is not mounted after 5 minutes"));
    }
    // 3. Docker, and the chains it has to exist for
    let docker = crate::docker::Docker::new(Arc::clone(host));
    let needs_docker = !later.is_empty() && config.firewall.manage || !config.boot.compose_up.is_empty();
    if needs_docker {
        if wait(&|| docker.ping(), 120) {
            if config.firewall.manage && !later.is_empty() {
                match firewall(core, &config.firewall.chains) {
                    Ok(message) => done.push(message),
                    Err(e) => problems.push(format!("firewall not applied: {e}")),
                }
            }
            // 4. stacks, in the configured order — unless what they keep their data on is missing
            if missing.is_empty() {
                let running: Vec<String> = docker
                    .containers(false)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|c| c["Labels"]["com.docker.compose.project.working_dir"].as_str().map(str::to_string))
                    .collect();
                let healer = crate::heal::Healer {
                    host: Arc::clone(host),
                    docker: docker.clone(),
                    state: core.state.clone(),
                    act: true,
                };
                let mut started = 0;
                for dir in config.boot.compose_up.iter().flat_map(|entry| compose_dirs(host.as_ref(), entry)) {
                    if running.contains(&dir) {
                        continue;
                    }
                    match healer.compose_up(&dir) {
                        Ok(()) => started += 1,
                        Err(e) => problems.push(e),
                    }
                }
                done.push(format!("{started} stacks started"));
            } else if !config.boot.compose_up.is_empty() {
                problems.push("stacks not started: their data is not mounted".to_string());
            }
        } else {
            problems.push("Docker did not come up within 2 minutes".to_string());
        }
    }
    // this boot is dealt with, whatever came of it: a restart of the service must not do it again
    if let Some(id) = boot_id(host.as_ref()) {
        let db = core.state.open_db(host.as_ref())?;
        db.conn()
            .execute("INSERT INTO settings (key, value) VALUES ('daemon.boot_id', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [id])
            .map_err(|e| Error::Db(e.to_string()))?;
    }
    for problem in &problems {
        log.warn(&format!("boot: {problem}"), &[("job", "boot")]);
    }
    let said: Vec<String> = done.into_iter().chain(problems.iter().cloned()).collect();
    let message = if said.is_empty() { "nothing to do".to_string() } else { said.join("; ") };
    Ok((if problems.is_empty() { "ok" } else { "failed" }.to_string(), message))
}
