//! moi's look at an SSH origin behind a WebSocket, against a real one: a real TLS handshake with the
//! system's trust store and a WebSocket server moi shares no code with (`tests/e2e/ws-bridge.py`,
//! bridging to a real sshd). Run by `tests/e2e/ssh-client-test.sh`; ignored without `MOI_IT=1`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use cestmoi::host::{Host, RealHost};
use cestmoi::net::Client;

#[test]
#[ignore = "needs MOI_IT=1 and MOI_IT_SSH_HOST: a name whose https:// answers WebSocket with an SSH server behind it"]
fn the_public_handshake_reads_the_banner_of_a_real_ssh_server() {
    if std::env::var("MOI_IT").is_err() {
        return;
    }
    let name = std::env::var("MOI_IT_SSH_HOST").expect("MOI_IT_SSH_HOST");
    let host: Arc<dyn Host> = Arc::new(RealHost::new());
    let ms = cestmoi::ssh::apply::handshake(&host, &name).unwrap_or_else(|e| panic!("{}: {}", e.stage, e.error));
    assert!(ms < 5000, "{ms} ms");
    // the banner itself: what sshd says first, in one binary message
    let first = Client::new(host.clone()).websocket_first_message(&format!("https://{name}/")).unwrap();
    let banner = String::from_utf8_lossy(&first);
    assert!(banner.starts_with("SSH-2.0-OpenSSH_"), "{banner}");
    // a name that does not resolve fails at the first stage, and says so
    let failed = cestmoi::ssh::apply::handshake(&host, "nothing.moi-e2e.invalid").unwrap_err();
    assert_eq!(failed.stage, "connect", "{failed:?}");
}
