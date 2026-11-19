// Public test identity, never a credential. Generated with:
// openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj /CN=localhost
//   -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1'
//   -addext 'extendedKeyUsage=serverAuth' -addext 'basicConstraints=critical,CA:FALSE'
// Then x509 -outform DER and pkcs8 -topk8 -nocrypt -outform DER.
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use std::{error::Error, future::Future, sync::Arc, time::Duration};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const DEADLINE: Duration = Duration::from_secs(5);
const CERT: &[u8] = include_bytes!("fixtures/localhost-cert.der");
const KEY: &[u8] = include_bytes!("fixtures/localhost-key.der");

async fn listener() -> (String, tokio::task::JoinHandle<std::io::Result<()>>) {
    // Explicit server provider must not install a process default or mask a
    // production client's ambiguous/missing provider configuration.
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(CERT)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY)),
    )
    .unwrap();
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(DEADLINE, socket.accept())
            .await
            .expect("client never reached the TLS fixture")
            .unwrap();
        tokio::time::timeout(DEADLINE, TlsAcceptor::from(Arc::new(config)).accept(stream))
            .await
            .expect("TLS listener timed out")
            .map(|_| ())
    });
    (address, task)
}

async fn trusted_control() {
    let (address, server) = listener().await;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(CERT)).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::time::timeout(DEADLINE, TcpStream::connect(&address))
        .await
        .expect("trusted control TCP connection timed out")
        .unwrap();
    tokio::time::timeout(
        DEADLINE,
        TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), tcp),
    )
    .await
    .expect("trusted control timed out")
    .expect("fixture must have a usable identity");
    tokio::time::timeout(DEADLINE, server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

pub async fn rejects_untrusted<F, Fut, T, E>(scheme: &str, connect: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: Error + 'static,
{
    trusted_control().await;
    let (address, server) = listener().await;
    let result = tokio::time::timeout(DEADLINE, connect(format!("{scheme}://{address}/")))
        .await
        .expect("client timed out instead of rejecting the certificate");
    let error = match result {
        Ok(_) => panic!("untrusted TLS connection succeeded"),
        Err(e) => e,
    };
    let mut description = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        description.push_str(&format!("; {cause}"));
        source = cause.source();
    }
    let normalized = description.to_ascii_lowercase();
    assert!(
        normalized.contains("unknownissuer")
            || normalized.contains("unknown issuer")
            || normalized.contains("self-signed certificate")
            || normalized.contains("untrusted root"),
        "expected an untrusted issuer, got: {description}"
    );
    // Accepting TCP is established by the task reaching the TLS error; a
    // refused connection or timeout cannot satisfy this assertion.
    assert!(
        tokio::time::timeout(DEADLINE, server)
            .await
            .unwrap()
            .unwrap()
            .is_err(),
        "server accepted an untrusted handshake"
    );
}

pub async fn rejects_http(client: reqwest::Client) {
    rejects_untrusted("https", |url| async move { client.get(url).send().await }).await;
}
