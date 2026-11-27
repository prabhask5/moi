//! Pushing a result to its Uptime Kuma push monitor: one status, one message.

use std::sync::Arc;

use super::CheckResult;
use crate::config::Config;
use crate::host::Host;
use crate::state::db::Db;

/// Push a result. Returns `"ok"` or why nothing was pushed; never an error — a monitor that cannot be
/// reached must not stop the check from being reported locally.
pub fn push(host: &Arc<dyn Host>, config: &Config, db: &Db, result: &CheckResult) -> String {
    let Some(monitor) = &result.monitor else { return "not pushed: the check has no monitor".to_string() };
    let owner = format!("check:{}", result.name);
    match crate::monitors::push(host, config, db, &owner, result.status.as_str(), &result.message) {
        Ok(()) => "ok".to_string(),
        Err(why) if why.contains("is bound to") => {
            format!("not pushed: monitor \"{monitor}\" is not bound to this check yet (run `moi monitors sync`)")
        }
        Err(why) => why,
    }
}
