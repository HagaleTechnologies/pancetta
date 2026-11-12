//! Interactive, session-cached QRZ lookups. Network work never holds up TUI dispatch.
use pancetta_dx::{QrzLookup, QrzXmlClient};
use pancetta_tui::tui_runner::TuiMessage;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};

pub(super) struct QrzLookupCtx {
    config: Arc<RwLock<pancetta_config::Config>>,
    tx: crossbeam_channel::Sender<TuiMessage>,
    base_url: Option<String>,
    client: Mutex<Option<Arc<QrzXmlClient>>>,
    cache: Arc<Mutex<HashMap<String, Option<QrzLookup>>>>,
    in_flight: Arc<AtomicBool>,
}

impl QrzLookupCtx {
    pub(super) fn new(
        config: Arc<RwLock<pancetta_config::Config>>,
        tx: crossbeam_channel::Sender<TuiMessage>,
        base_url: Option<String>,
    ) -> Self {
        Self {
            config,
            tx,
            base_url,
            client: Mutex::new(None),
            cache: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

fn reply(tx: &crossbeam_channel::Sender<TuiMessage>, message: TuiMessage) {
    if tx.try_send(message).is_err() {
        tracing::debug!("QRZ TUI reply could not be delivered");
    }
}

fn status(
    tx: &crossbeam_channel::Sender<TuiMessage>,
    text: String,
    level: pancetta_core::DiagnosticLevel,
) {
    reply(
        tx,
        TuiMessage::StatusUpdate {
            component: "QRZ".into(),
            status: text,
            level,
        },
    );
}

fn result(tx: &crossbeam_channel::Sender<TuiMessage>, callsign: String, lookup: Option<QrzLookup>) {
    if let Some(l) = lookup {
        reply(
            tx,
            TuiMessage::QrzLookupUpdate {
                callsign,
                name: l.name,
                qth: l.qth,
                grid: l.grid,
                country: l.country,
                state: l.state,
            },
        );
    } else {
        status(
            tx,
            format!("{callsign} not found on QRZ"),
            pancetta_core::DiagnosticLevel::Info,
        );
    }
}

// Reset even if the spawned task is cancelled or unwinds.
struct FlightGuard(Arc<AtomicBool>);
impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) async fn handle_lookup_callsign(ctx: &QrzLookupCtx, callsign: String) {
    let callsign = callsign.trim().to_uppercase();
    let cfg = ctx.config.read().await.network.qrz_xml.clone();
    if !cfg.enabled || cfg.username.is_empty() || cfg.password.is_empty() {
        status(
            &ctx.tx,
            "lookups need [network.qrz_xml] username/password".into(),
            pancetta_core::DiagnosticLevel::Warn,
        );
        return;
    }
    if let Some(cached) = ctx.cache.lock().await.get(&callsign).cloned() {
        result(&ctx.tx, callsign, cached);
        return;
    }
    if ctx
        .in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        status(
            &ctx.tx,
            "lookup already in progress, try again".into(),
            pancetta_core::DiagnosticLevel::Info,
        );
        return;
    }
    let guard = FlightGuard(ctx.in_flight.clone());
    let client = {
        let mut client = ctx.client.lock().await;
        client
            .get_or_insert_with(|| {
                let c = QrzXmlClient::new(
                    cfg.username,
                    cfg.password,
                    format!("pancetta-{}", env!("CARGO_PKG_VERSION")),
                );
                Arc::new(if let Some(url) = &ctx.base_url {
                    c.with_base_url(url)
                } else {
                    c
                })
            })
            .clone()
    };
    let tx = ctx.tx.clone();
    let cache = ctx.cache.clone();
    tokio::spawn(async move {
        let _guard = guard;
        match tokio::time::timeout(Duration::from_secs(2), client.lookup(&callsign)).await {
            Ok(Ok(lookup)) => {
                cache
                    .lock()
                    .await
                    .insert(callsign.clone(), Some(lookup.clone()));
                result(&tx, callsign, Some(lookup));
            }
            Ok(Err(pancetta_dx::DxError::ExternalService(ref e)))
                if e.to_ascii_lowercase().contains("not found")
                    || e.contains("returned no callsign data") =>
            {
                cache.lock().await.insert(callsign.clone(), None);
                result(&tx, callsign, None);
            }
            Ok(Err(_)) => status(
                &tx,
                format!("{callsign} lookup failed"),
                pancetta_core::DiagnosticLevel::Warn,
            ),
            Err(_) => status(
                &tx,
                format!("{callsign} lookup timed out"),
                pancetta_core::DiagnosticLevel::Warn,
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn context(
        enabled: bool,
        url: Option<String>,
    ) -> (QrzLookupCtx, crossbeam_channel::Receiver<TuiMessage>) {
        let mut cfg = pancetta_config::Config::default();
        cfg.network.qrz_xml.enabled = enabled;
        cfg.network.qrz_xml.username = "user".into();
        cfg.network.qrz_xml.password = "secret".into();
        let (tx, rx) = crossbeam_channel::unbounded();
        (QrzLookupCtx::new(Arc::new(RwLock::new(cfg)), tx, url), rx)
    }

    async fn next(rx: &crossbeam_channel::Receiver<TuiMessage>) -> TuiMessage {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(m) = rx.try_recv() {
                    return m;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("QRZ reply deadline")
    }

    async fn speaking_qrz_server(
        body: &'static str,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                seen.fetch_add(1, Ordering::SeqCst);
                let mut request = vec![0; 4096];
                let n = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                let response = if request.contains("username=") {
                    "<QRZDatabase><Session><Key>abc</Key></Session></QRZDatabase>"
                } else {
                    body
                };
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            response.len(),
                            response
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        (url, count, task)
    }

    #[tokio::test]
    async fn missing_credentials_explains_config_keys() {
        let (ctx, rx) = context(false, None);
        handle_lookup_callsign(&ctx, "DL1ABC".into()).await;
        assert!(
            matches!(next(&rx).await, TuiMessage::StatusUpdate {component, status, ..}
            if component == "QRZ" && status == "lookups need [network.qrz_xml] username/password")
        );
    }

    #[tokio::test]
    async fn configured_lookup_returns_fields_and_caches_normalized_call() {
        let (url, count, server) = speaking_qrz_server("<QRZDatabase><Callsign><call>DL1ABC</call><name>Hans</name><addr2>Amberg</addr2><grid>JO62jk</grid><country>Germany</country><state>BY</state></Callsign></QRZDatabase>").await;
        let (ctx, rx) = context(true, Some(url));
        handle_lookup_callsign(&ctx, " dl1abc ".into()).await;
        assert!(
            matches!(next(&rx).await, TuiMessage::QrzLookupUpdate {callsign, name, qth, grid, country, state}
            if callsign == "DL1ABC" && name.as_deref() == Some("Hans") && qth.as_deref() == Some("Amberg")
            && grid.as_deref() == Some("JO62jk") && country.as_deref() == Some("Germany") && state.as_deref() == Some("BY"))
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
        handle_lookup_callsign(&ctx, "DL1ABC".into()).await;
        assert!(
            matches!(next(&rx).await, TuiMessage::QrzLookupUpdate {callsign, ..} if callsign == "DL1ABC")
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn lookup_misses_are_cached_without_another_connection() {
        let (url, count, server) = speaking_qrz_server(
            "<QRZDatabase><Session><Error>Not found: DL1ABC</Error></Session></QRZDatabase>",
        )
        .await;
        let (ctx, rx) = context(true, Some(url));
        for _ in 0..2 {
            handle_lookup_callsign(&ctx, "DL1ABC".into()).await;
            assert!(
                matches!(next(&rx).await, TuiMessage::StatusUpdate {status, ..} if status == "DL1ABC not found on QRZ")
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn silent_qrz_server_times_out_without_blocking_later_commands() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let (ctx, rx) = context(true, Some(url));
        let started = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_millis(100),
            handle_lookup_callsign(&ctx, "DL1ABC".into()),
        )
        .await
        .expect("dispatch must return before HTTP");
        handle_lookup_callsign(&ctx, "JA1ABC".into()).await;
        assert!(
            matches!(next(&rx).await, TuiMessage::StatusUpdate {status, ..} if status == "lookup already in progress, try again")
        );
        assert!(
            matches!(next(&rx).await, TuiMessage::StatusUpdate {status, ..} if status == "DL1ABC lookup timed out")
        );
        assert!(started.elapsed() < Duration::from_millis(2500));
        assert!(!ctx.in_flight.load(Ordering::Acquire));
        // The next command can use the normal refusal path immediately, even after timeout.
        ctx.config.write().await.network.qrz_xml.enabled = false;
        handle_lookup_callsign(&ctx, "JA1ABC".into()).await;
        assert!(
            matches!(next(&rx).await, TuiMessage::StatusUpdate {status, ..} if status.starts_with("lookups need"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn service_errors_do_not_leak_credentials_or_cache_failures() {
        let (url, count, server) = speaking_qrz_server("<QRZDatabase><Session><Error>Username/password incorrect: secret</Error></Session></QRZDatabase>").await;
        let (ctx, rx) = context(true, Some(url));
        for _ in 0..2 {
            handle_lookup_callsign(&ctx, "DL1ABC".into()).await;
            assert!(
                matches!(next(&rx).await, TuiMessage::StatusUpdate {status, ..} if status == "DL1ABC lookup failed")
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 3);
        server.abort();
    }
}
