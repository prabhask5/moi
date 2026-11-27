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
    Ok(message)
}
