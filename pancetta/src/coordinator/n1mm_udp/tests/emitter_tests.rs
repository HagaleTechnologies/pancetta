// PAN-382. The emitter task: one datagram per completion, nothing for other
// events or skips, lag tolerance, channel-close lifetime, and the replay gate.
use super::contactinfo_tests::{metadata, parse};
use super::*;
use std::time::Duration;
use tokio::net::UdpSocket;

fn completed(meta: QsoMetadata) -> QsoEvent {
    QsoEvent::QsoCompleted {
        qso_id: meta.qso_id,
        metadata: meta,
        state_history: vec![],
        messages: vec![],
    }
}

fn with_call(call: Option<&str>) -> QsoMetadata {
    let mut meta = metadata();
    meta.their_callsign = call.map(str::to_string);
    meta
}

async fn listener() -> (UdpSocket, SocketAddr) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind listener");
    let addr = socket.local_addr().expect("listener addr");
    (socket, addr)
}

async fn recv_within(socket: &UdpSocket, wait: Duration) -> Option<String> {
    let mut buf = vec![0u8; 65_536];
    match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(String::from_utf8(buf[..n].to_vec()).expect("utf-8 datagram")),
        Ok(Err(e)) => panic!("recv_from failed: {e}"),
        Err(_) => None,
    }
}

#[tokio::test]
async fn one_completed_qso_sends_exactly_one_datagram() {
    let (socket, addr) = listener().await;
    let (tx, rx) = broadcast::channel(16);
    let handle = start_n1mm_udp_emitter(addr, 100, false, rx);
    tx.send(completed(metadata())).expect("send event");
    let xml = recv_within(&socket, Duration::from_secs(3))
        .await
        .expect("one datagram");
    assert!(xml.contains("<call>K1ABC</call>"), "{xml}");
    assert_eq!(parse(&xml).value("call"), "K1ABC");
    assert!(
        recv_within(&socket, Duration::from_millis(500))
            .await
            .is_none(),
        "a second datagram arrived"
    );
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn other_qso_events_send_nothing() {
    let (socket, addr) = listener().await;
    let (tx, rx) = broadcast::channel(16);
    let handle = start_n1mm_udp_emitter(addr, 100, false, rx);
    tx.send(QsoEvent::TxOffsetApplied {
        qso_id: pancetta_qso::QsoId::new_v4(),
        offset_hz: 1500.0,
    })
    .expect("send event");
    assert!(
        recv_within(&socket, Duration::from_millis(500))
            .await
            .is_none(),
        "a non-completion event produced a datagram"
    );
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn skipped_completion_sends_nothing_and_the_next_one_still_sends() {
    let (socket, addr) = listener().await;
    let (tx, rx) = broadcast::channel(16);
    let handle = start_n1mm_udp_emitter(addr, 100, false, rx);
    tx.send(completed(with_call(None))).expect("send skipped");
    tx.send(completed(with_call(Some("K1ABC"))))
        .expect("send good");
    let xml = recv_within(&socket, Duration::from_secs(3))
        .await
        .expect("the second completion's datagram");
    assert_eq!(parse(&xml).value("call"), "K1ABC");
    assert!(
        recv_within(&socket, Duration::from_millis(500))
            .await
            .is_none(),
        "the skipped completion produced a datagram"
    );
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn a_lagged_stream_keeps_sending_later_completions() {
    let (socket, addr) = listener().await;
    let (tx, rx) = broadcast::channel(2);
    for call in ["K1AAA", "K1BBB", "K1CCC"] {
        tx.send(completed(with_call(Some(call))))
            .expect("send event");
    }
    let handle = start_n1mm_udp_emitter(addr, 100, false, rx);
    let mut calls = Vec::new();
    while let Some(xml) = recv_within(&socket, Duration::from_secs(2)).await {
        calls.push(parse(&xml).value("call").to_string());
    }
    assert_eq!(calls, vec!["K1BBB".to_string(), "K1CCC".to_string()]);
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn emitter_ends_when_the_event_channel_closes() {
    let (_socket, addr) = listener().await;
    let (tx, rx) = broadcast::channel::<QsoEvent>(16);
    let handle = start_n1mm_udp_emitter(addr, 100, false, rx);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("emitter ended within 2 s")
        .expect("emitter task did not panic");
}

#[test]
fn gate_is_closed_under_replay_or_when_unset() {
    let with_key = LoggingConfig {
        n1mm_udp: Some("127.0.0.1:12060".to_string()),
        ..LoggingConfig::default()
    };
    assert_eq!(n1mm_udp_destination(&with_key, true), None);
    assert_eq!(n1mm_udp_destination(&LoggingConfig::default(), false), None);
    assert_eq!(
        n1mm_udp_destination(&with_key, false),
        Some("127.0.0.1:12060".parse().unwrap())
    );
}
