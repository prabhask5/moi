//! Carrying out a plan: bindings in `moi.db`, and through Kuma's socket API everything that changes
//! Kuma. A step that fails is reported and does not stop the others.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use super::kuma::Kuma;
use super::{Action, Desired, Inventory, Kind, Plan, Step, TAG};
use crate::config::Config;
use crate::docker::Docker;
use crate::error::Error;
use crate::host::{self, Host};
use crate::state::db::Db;

const DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// A step that could not be carried out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub monitor: String,
    pub error: String,
}

/// What applying a plan did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub adopted: usize,
    pub created: usize,
    pub updated: usize,
    pub paused: usize,
    pub deleted: usize,
    pub failed: Vec<Failure>,
}

impl Report {
    /// `4 adopted, 36 created` — or that there was nothing to do.
    pub fn summary(&self) -> String {
        let parts: Vec<String> = [
            (self.adopted, "adopted"),
            (self.created, "created"),
            (self.updated, "updated"),
            (self.paused, "paused"),
            (self.deleted, "deleted"),
            (self.failed.len(), "failed"),
        ]
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} {what}"))
        .collect();
        if parts.is_empty() {
            "nothing to do: Uptime Kuma already has every monitor moi wants".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Record that an owner uses a monitor.
pub fn bind(
    db: &Db,
    owner: &str,
    name: &str,
    id: i64,
    token: Option<&str>,
    created_by_moi: bool,
    now: i64,
) -> Result<(), Error> {
    db.conn()
        .execute(
            "INSERT INTO monitor_bindings (owner, monitor_name, kuma_id, push_token, created_by_moi, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(owner) DO UPDATE SET monitor_name = excluded.monitor_name, kuma_id = excluded.kuma_id, push_token = excluded.push_token,
                                              created_by_moi = excluded.created_by_moi, updated_at = excluded.updated_at",
            (owner, name, id, token, i64::from(created_by_moi), now),
        )
        .map(|_| ())
        .map_err(|e| Error::Db(e.to_string()))
}

/// What creating monitors needs from Kuma, looked up or created once.
struct Context<'a> {
    host: &'a Arc<dyn Host>,
    config: &'a Config,
    docker: &'a Docker,
    inventory: &'a Inventory,
    tag: Option<i64>,
    group: Option<i64>,
    docker_host: Option<i64>,
}

impl Context<'_> {
    fn tag(&mut self, kuma: &mut Kuma) -> Result<i64, String> {
        if self.tag.is_none() {
            let existing = self.inventory.tags.iter().find(|(_, name)| name == TAG).map(|(id, _)| *id);
            self.tag = Some(match existing {
                Some(id) => id,
                None => kuma.add_tag(TAG, "#4B5563")?,
            });
        }
        self.tag.ok_or_else(|| "no tag".to_string())
    }

    /// The group monitor created monitors go into, if one is configured.
    fn group(&mut self, kuma: &mut Kuma) -> Result<Option<i64>, String> {
        let name = &self.config.monitors.kuma.group;
        if name.is_empty() {
            return Ok(None);
        }
        if self.group.is_none() {
            let existing = self.inventory.monitors.iter().find(|m| m.kind == "group" && m.name == *name).map(|m| m.id);
            self.group = Some(match existing {
                Some(id) => id,
                None => kuma.add_monitor(json!({ "type": "group", "name": name, "interval": 60 }))?,
            });
        }
        Ok(self.group)
    }

    /// The docker host container monitors ask; created when Kuma can reach the socket.
    fn docker_host(&mut self, kuma: &mut Kuma) -> Result<i64, String> {
        if let Some(id) = self.docker_host {
            return Ok(id);
        }
        let name = &self.config.monitors.kuma.docker_host;
        let id = match self.inventory.docker_hosts.iter().find(|h| h.name == *name) {
            Some(existing) => existing.id,
            None => {
                let mounts_socket = self.docker.containers_inspect().unwrap_or_default().iter().any(|c| {
                    c["Config"]["Image"].as_str().is_some_and(|image| image.contains("uptime-kuma"))
                        && c["Mounts"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|m| m["Source"] == DOCKER_SOCKET || m["Destination"] == DOCKER_SOCKET)
                });
                if !mounts_socket {
                    return Err(format!(
                        "Uptime Kuma has no docker host \"{name}\" and its container does not mount {DOCKER_SOCKET}: mount the socket into it (read-only), or set monitors.kuma.containers = false"
                    ));
                }
                kuma.add_docker_host(name, DOCKER_SOCKET)?
            }
        };
        self.docker_host = Some(id);
        Ok(id)
    }

    /// A new push token: 32 letters and digits, like the ones Kuma's own form makes.
    fn push_token(&self) -> Result<String, String> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let random = self.host.random_bytes(48).map_err(|e| format!("no randomness for a push token: {e}"))?;
        // bytes that would favour the first letters of the alphabet are skipped
        let limit = u8::try_from(256 - 256 % ALPHABET.len()).unwrap_or(u8::MAX);
        let token: String = random
            .iter()
            .filter(|b| **b < limit)
            .take(32)
            .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
            .collect();
        if token.len() == 32 { Ok(token) } else { Err("not enough randomness for a push token".to_string()) }
    }

    /// Create the monitor; returns its id and, for a push monitor, its token.
    fn create(&mut self, kuma: &mut Kuma, want: &Desired) -> Result<(i64, Option<String>), String> {
        let wanted = &self.config.monitors.kuma.notifications;
        let notifications: serde_json::Map<String, Value> = self
            .inventory
            .notifications
            .iter()
            .filter(|(_, name)| wanted.contains(name))
            .map(|(id, _)| (id.to_string(), json!(true)))
            .collect();
        let mut fields = json!({
            "type": want.kind.as_str(),
            "name": want.name,
            "interval": want.interval,
            "maxretries": want.retries,
            "notificationIDList": notifications,
            "description": format!("Managed by moi ({})", want.owner),
        });
        if let Some(group) = self.group(kuma)? {
            fields["parent"] = json!(group);
        }
        let token = match want.kind {
            Kind::Push => {
                let token = self.push_token()?;
                fields["pushToken"] = json!(token);
                Some(token)
            }
            Kind::Docker => {
                fields["docker_container"] = json!(want.container);
                fields["docker_host"] = json!(self.docker_host(kuma)?);
                None
            }
        };
        let id = kuma.add_monitor(fields)?;
        // the tag is how a human sees in Kuma that the monitor is moi's
        let tag = self.tag(kuma)?;
        kuma.tag_monitor(tag, id)?;
        Ok((id, token))
    }
}

/// Carry out a plan made from `inventory`.
pub fn apply(
    host: &Arc<dyn Host>,
    config: &Config,
    docker: &Docker,
    db: &Db,
    kuma: &mut Kuma,
    plan: &Plan,
    inventory: &Inventory,
) -> Result<Report, Error> {
    let now = host::unix_seconds(host.now());
    let mut cx = Context { host, config, docker, inventory, tag: None, group: None, docker_host: None };
    let mut report = Report::default();
    for step in &plan.steps {
        let outcome: Result<(), String> = (|| {
            let Step { owner: Some(owner), .. } = step else { return Ok(()) };
            match (step.action, step.kuma_id, &step.desired) {
                (Action::Adopt, Some(id), _) => {
                    bind(db, owner, &step.monitor, id, step.token.as_deref(), false, now).map_err(|e| e.to_string())?;
                    report.adopted += 1;
                }
                (Action::Create, _, Some(want)) => {
                    let (id, token) = cx.create(kuma, want)?;
                    bind(db, owner, &want.name, id, token.as_deref(), true, now).map_err(|e| e.to_string())?;
                    report.created += 1;
                }
                (Action::Update, Some(id), Some(want)) => {
                    let mut monitor = kuma.monitor(id)?;
                    if monitor["interval"].as_u64() != Some(want.interval) {
                        monitor["interval"] = json!(want.interval);
                        kuma.edit_monitor(&monitor)?;
                    }
                    if inventory.by_id(id).is_some_and(|m| !m.active) {
                        kuma.resume(id)?;
                    }
                    report.updated += 1;
                }
                (Action::Pause, Some(id), _) => {
                    kuma.pause(id)?;
                    report.paused += 1;
                }
                (Action::Delete, Some(id), _) => {
                    kuma.delete(id)?;
                    db.conn()
                        .execute("DELETE FROM monitor_bindings WHERE owner = ?1", [owner])
                        .map_err(|e| e.to_string())?;
                    report.deleted += 1;
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            report.failed.push(Failure { monitor: step.monitor.clone(), error });
        }
    }
    Ok(report)
}
