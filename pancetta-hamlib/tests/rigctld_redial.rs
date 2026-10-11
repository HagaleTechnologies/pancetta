//! PAN-115 contract: the `RigctldClient` behaviour the coordinator's poll-loop
//! redial depends on. `get_status()` only reports the cached connection flag
//! and never redials; an explicit `connect()` does.

use std::net::SocketAddr;
use std::time::Duration;

use pancetta_hamlib::{ConnectionState, RigControl, RigctldClient, RigctldConfig, Vfo};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Minimal fake rigctld: answers `f` with `14074000` and anything else with
/// `RPRT 0`. Serves one connection at a time inside the task, so aborting the
/// task drops both the listener and the live socket.
fn spawn_fake_rigctld(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let reply = if line.trim() == "f" {
                    "14074000\n"
                } else {
                    "RPRT 0\n"
                };
                if write_half.write_all(reply.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    })
}

async fn connection_state(client: &RigctldClient) -> ConnectionState {
    client
        .get_status()
        .await
        .expect("get_status never errors")
        .connection_state
}

#[tokio::test]
async fn get_status_never_redials_but_connect_recovers_the_link() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let server = spawn_fake_rigctld(listener);

    let client = RigctldClient::new(RigctldConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        timeout_ms: 500,
        command_timeout_ms: 300,
        ..RigctldConfig::default()
    });
    client.connect().await.expect("initial connect");
    assert_eq!(connection_state(&client).await, ConnectionState::Connected);
    assert_eq!(
        client.get_frequency(Vfo::Current).await.unwrap(),
        14_074_000
    );

    // rigctld dies: the next command notices and marks the client disconnected.
    server.abort();
    let _ = server.await;
    assert!(client.get_frequency(Vfo::Current).await.is_err());
    assert_eq!(
        connection_state(&client).await,
        ConnectionState::Disconnected
    );

    // rigctld comes back on the same port. Polling the status alone never
    // reconnects (a ~6 s window of 500 ms poll ticks, sped up here).
    let listener = TcpListener::bind(addr).await.expect("rebind same port");
    let server = spawn_fake_rigctld(listener);
    for _ in 0..12 {
        assert_eq!(
            connection_state(&client).await,
            ConnectionState::Disconnected
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // One explicit connect() restores the link.
    client.connect().await.expect("redial");
    assert_eq!(connection_state(&client).await, ConnectionState::Connected);
    assert_eq!(
        client.get_frequency(Vfo::Current).await.unwrap(),
        14_074_000
    );

    server.abort();
}
