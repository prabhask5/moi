//! What Uptime Kuma has, read from its database without credentials and without ever writing to it.
//! Schemas differ between Kuma 1.23 and 2.x, so every column and table is optional.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::docker::Docker;
use crate::host::Host;

/// One monitor as Kuma stores it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct KumaMonitor {
    pub id: i64,
    pub name: String,
    /// Kuma's type: `push`, `docker`, `http`, `keyword`, `group`, …
    pub kind: String,
    /// `false` when paused.
    pub active: bool,
    pub interval: Option<i64>,
    pub retries: Option<i64>,
    pub push_token: Option<String>,
    pub docker_container: Option<String>,
    pub docker_host: Option<i64>,
    pub url: Option<String>,
    /// The group monitor it is in.
    pub parent: Option<i64>,
    pub tags: Vec<String>,
    pub notifications: Vec<String>,
    /// Latest heartbeat: `up`, `down`, `pending`, `maintenance`.
    pub last_status: Option<String>,
}

impl KumaMonitor {
    /// A monitor that pings healthchecks.io: the watchdog that notices when Kuma itself is gone.
    pub fn is_watchdog(&self) -> bool {
        let host = self
            .url
            .as_deref()
            .and_then(|u| u.split("://").nth(1))
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        host == "hc-ping.com" || host.contains("healthchecks")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DockerHost {
    pub id: i64,
    pub name: String,
    /// `/var/run/docker.sock` or a TCP address.
    pub daemon: String,
}

/// Kuma's monitors and what they can be attached to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Inventory {
    /// Host path of the database this was read from.
    pub db: String,
    pub monitors: Vec<KumaMonitor>,
    pub notifications: Vec<(i64, String)>,
    pub tags: Vec<(i64, String)>,
    pub docker_hosts: Vec<DockerHost>,
}

impl Inventory {
    pub fn by_id(&self, id: i64) -> Option<&KumaMonitor> {
        self.monitors.iter().find(|m| m.id == id)
    }
}

/// Open Kuma's database without ever writing to it: read-only first, and if SQLite cannot even do that
/// (WAL files it may not create), as an immutable file.
pub(crate) fn open_read_only(real: &Path) -> Option<Connection> {
    for uri in [format!("file:{}?mode=ro", real.display()), format!("file:{}?mode=ro&immutable=1", real.display())] {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        // an empty monitor table is still a Kuma database: preparing the query is the test
        if let Ok(conn) = Connection::open_with_flags(&uri, flags)
            && conn.prepare("SELECT 1 FROM monitor LIMIT 1").is_ok()
        {
            return Some(conn);
        }
    }
    None
}

/// Where the Kuma container keeps its database on the host: the source of its `/app/data` mount.
pub fn locate(host: &dyn Host, docker: &Docker) -> Option<String> {
    docker
        .containers_inspect()
        .ok()?
        .iter()
        .filter(|c| c["Config"]["Image"].as_str().is_some_and(|image| image.contains("uptime-kuma")))
        .flat_map(|c| c["Mounts"].as_array().into_iter().flatten())
        .filter(|m| m["Destination"] == "/app/data")
        .filter_map(|m| m["Source"].as_str())
        .map(|source| format!("{}/kuma.db", source.trim_end_matches('/')))
        .find(|db| host.exists(Path::new(db)))
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(1)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default()
}

/// Read the inventory from a database at this host path.
pub fn read(host: &dyn Host, db: &str) -> Option<Inventory> {
    let conn = open_read_only(&host.real_path(Path::new(db)).ok()?)?;
    let have = columns(&conn, "monitor");
    // a column this Kuma version does not have reads as NULL
    let column = |name: &str| if have.iter().any(|c| c == name) { name.to_string() } else { format!("NULL AS {name}") };
    let wanted = [
        "id",
        "name",
        "type",
        "active",
        "interval",
        "maxretries",
        "push_token",
        "docker_container",
        "docker_host",
        "url",
        "parent",
    ];
    let sql = format!("SELECT {} FROM monitor ORDER BY id", wanted.map(column).join(", "));
    let text = |r: &rusqlite::Row<'_>, i: usize| r.get::<_, Option<String>>(i).ok().flatten().filter(|s| !s.is_empty());
    let int = |r: &rusqlite::Row<'_>, i: usize| r.get::<_, Option<i64>>(i).ok().flatten();
    let mut monitors: Vec<KumaMonitor> = conn
        .prepare(&sql)
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok(KumaMonitor {
                    id: r.get(0)?,
                    name: text(r, 1).unwrap_or_default(),
                    kind: text(r, 2).unwrap_or_default(),
                    active: int(r, 3).unwrap_or(1) != 0,
                    interval: int(r, 4),
                    retries: int(r, 5),
                    push_token: text(r, 6),
                    docker_container: text(r, 7),
                    docker_host: int(r, 8).filter(|id| *id != 0),
                    url: text(r, 9),
                    parent: int(r, 10).filter(|id| *id != 0),
                    ..KumaMonitor::default()
                })
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    let pairs = |sql: &str| -> Vec<(i64, String)> {
        conn.prepare(sql)
            .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()))
            .unwrap_or_default()
    };
    let attached = pairs(
        "SELECT mn.monitor_id, n.name FROM monitor_notification mn JOIN notification n ON n.id = mn.notification_id ORDER BY n.name",
    );
    let tagged =
        pairs("SELECT mt.monitor_id, t.name FROM monitor_tag mt JOIN tag t ON t.id = mt.tag_id ORDER BY t.name");
    for monitor in &mut monitors {
        let of =
            |list: &[(i64, String)]| list.iter().filter(|(id, _)| *id == monitor.id).map(|(_, v)| v.clone()).collect();
        monitor.notifications = of(&attached);
        monitor.tags = of(&tagged);
        monitor.last_status = conn
            .query_row(
                "SELECT status FROM heartbeat WHERE monitor_id = ?1 ORDER BY time DESC LIMIT 1",
                [monitor.id],
                |r| r.get::<_, i64>(0),
            )
            .ok()
            .map(|status| {
                ["down", "up", "pending", "maintenance"]
                    .get(usize::try_from(status).unwrap_or(usize::MAX))
                    .copied()
                    .unwrap_or("unknown")
                    .to_string()
            });
    }
    let docker_hosts = conn
        .prepare("SELECT id, name, docker_daemon FROM docker_host ORDER BY id")
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok(DockerHost {
                    id: r.get(0)?,
                    name: text(r, 1).unwrap_or_default(),
                    daemon: text(r, 2).unwrap_or_default(),
                })
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    Some(Inventory {
        db: db.to_string(),
        monitors,
        notifications: pairs("SELECT id, name FROM notification ORDER BY id"),
        tags: pairs("SELECT id, name FROM tag ORDER BY id"),
        docker_hosts,
    })
}
