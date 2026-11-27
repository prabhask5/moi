//! Comparing what moi's checks say in the shadow with what the legacy scripts told Uptime Kuma at
//! the same moments: the evidence that a switch will not change a single message.

use serde::Serialize;

use crate::config::Config;
use crate::docker::Docker;
use crate::host::Host;
use crate::monitors::inventory;
use crate::state::StateDir;
use crate::state::db::Db;

/// How many of the latest runs are compared, and how many `switch` wants to see agree.
pub const WINDOW: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// The same status and the same message, byte for byte.
    Identical,
    /// The same once every number is replaced by `N`: the two looked a moment apart.
    SameShape,
    Different,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pair {
    /// When moi's run was (unix seconds).
    pub at: i64,
    pub verdict: Verdict,
    pub moi: String,
    pub legacy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Compared {
    pub check: String,
    pub monitor: String,
    pub pairs: Vec<Pair>,
    /// Why there is nothing to compare, when there is not.
    pub note: Option<String>,
}

impl Compared {
    pub fn count(&self, verdict: Verdict) -> usize {
        self.pairs.iter().filter(|p| p.verdict == verdict).count()
    }

    /// Enough runs, none of them different: the check can be switched.
    pub fn clean(&self) -> bool {
        self.pairs.len() >= WINDOW && self.count(Verdict::Different) == 0
    }
}

/// Every number (`12`, `3.5`) as `N`.
pub fn mask_numbers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if !c.is_ascii_digit() {
            out.push(c);
            continue;
        }
        out.push('N');
        let mut fraction = false;
        while let Some(&next) = chars.peek() {
            let mut ahead = chars.clone();
            ahead.next();
            if next.is_ascii_digit() {
                chars.next();
            } else if next == '.' && !fraction && ahead.peek().is_some_and(char::is_ascii_digit) {
                fraction = true;
                chars.next();
            } else {
                break;
            }
        }
    }
    out
}

pub fn verdict(moi: &str, legacy: &str) -> Verdict {
    if moi == legacy {
        Verdict::Identical
    } else if mask_numbers(moi) == mask_numbers(legacy) {
        Verdict::SameShape
    } else {
        Verdict::Different
    }
}

/// Compare the latest shadow runs of every check that has a legacy feeder.
pub fn compare(host: &dyn Host, config: &Config, state: &StateDir, db: &Db, docker: &Docker) -> Vec<Compared> {
    let kuma_db = inventory::locate(host, docker);
    let checks = crate::checks::enabled(config);
    let mut out = Vec::new();
    for feeder in crate::monitors::feeders(host, config, state) {
        let name = feeder.owner.strip_prefix("check:").unwrap_or(&feeder.owner).to_string();
        let Some(check) = checks.iter().find(|c| c.name() == name) else { continue };
        let monitor = check.monitor(config).unwrap_or_default();
        let mut compared = Compared { check: name.clone(), monitor, pairs: Vec::new(), note: None };
        let runs: Vec<(i64, String, String)> = db
            .conn()
            .prepare("SELECT at, status, message FROM check_results WHERE check_name = ?1 ORDER BY at DESC, id DESC LIMIT ?2")
            .and_then(|mut s| s.query_map((&name, i64::try_from(WINDOW).unwrap_or(12)), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|rows| rows.flatten().collect()))
            .unwrap_or_default();
        let Some(kuma_db) = &kuma_db else {
            compared.note = Some("Uptime Kuma's database is not on this machine: nothing to compare with".to_string());
            out.push(compared);
            continue;
        };
        // enough of the monitor's history to find a neighbour for each run
        let beats = inventory::heartbeats(host, kuma_db, &feeder.token, WINDOW * 6);
        let near = i64::try_from(check.interval(config).as_secs()).unwrap_or(300).max(60);
        for (at, status, message) in runs {
            // the legacy result closest in time, and no further away than one interval
            let Some(beat) = beats.iter().min_by_key(|b| (b.at - at).abs()).filter(|b| (b.at - at).abs() <= near)
            else {
                continue;
            };
            let (moi, legacy) = (format!("{status}: {message}"), format!("{}: {}", beat.status, beat.message));
            compared.pairs.push(Pair { at, verdict: verdict(&moi, &legacy), moi, legacy });
        }
        if compared.pairs.is_empty() {
            compared.note = Some(if beats.is_empty() {
                "the legacy script's monitor has no results in Uptime Kuma's database".to_string()
            } else {
                "no shadow run of moi near a legacy result yet: let both run for an hour".to_string()
            });
        }
        out.push(compared);
    }
    out
}

/// The comparison as people read it.
pub fn render(all: &[Compared]) -> String {
    let mut out = String::new();
    if all.is_empty() {
        out.push_str("no check has a legacy script to compare with\n");
    }
    for compared in all {
        let (identical, shape, different) = (
            compared.count(Verdict::Identical),
            compared.count(Verdict::SameShape),
            compared.count(Verdict::Different),
        );
        let ready = if compared.clean() {
            "ready to switch".to_string()
        } else if different > 0 {
            "NOT the same".to_string()
        } else {
            format!("{} of {WINDOW} runs so far", compared.pairs.len())
        };
        out.push_str(&format!(
            "{} (\"{}\"): {identical} identical, {shape} same shape, {different} different — {ready}\n",
            compared.check, compared.monitor
        ));
        if let Some(note) = &compared.note {
            out.push_str(&format!("  {note}\n"));
        }
        for pair in compared.pairs.iter().filter(|p| p.verdict == Verdict::Different) {
            out.push_str(&format!(
                "  {}\n    - legacy  {}\n    + moi     {}\n",
                crate::state::history::display_time(&crate::host::format_rfc3339(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(pair.at).unwrap_or(0))
                )),
                pair.legacy,
                pair.moi
            ));
        }
    }
    out
}
