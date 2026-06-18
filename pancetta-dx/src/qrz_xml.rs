//! QRZ.com paid XML callsign-lookup client (read-side enrichment scaffold).
//!
//! The [QRZ XML Logbook Data API](https://www.qrz.com/page/xml_data.html)
//! lets a paying subscriber look up a callsign and receive structured
//! biographical/location data (name, grid, DXCC, country, state, …). It is a
//! **credentialed, per-operator paid subscription** and cannot be proxied
//! through cqdx.io, so — like LoTW / ClubLog / QRZ Logbook — its credentials
//! stay local on the pancetta host and are **never logged**.
//!
//! This module is a self-contained client scaffold built to pancetta's
//! established "scaffold + wire, operator confirms" pattern for credentialed
//! integrations. It is **not** wired into the decode/priority hot path; that is
//! a later operator decision. Construct a [`QrzXmlClient`] from the operator's
//! local config and call [`lookup`](QrzXmlClient::lookup); the client manages
//! the QRZ session-key lifecycle internally (cache + auto re-auth on timeout).
//!
//! ## Protocol
//!
//! All requests go to `https://xmldata.qrz.com/xml/current/`. QRZ uses
//! semicolon-separated query parameters.
//!
//! 1. **Auth:** `?username=<u>;password=<p>;agent=pancetta-<ver>` → XML with a
//!    `<Session><Key>…</Key></Session>` session key (or `<Session><Error>…`).
//! 2. **Lookup:** `?s=<sessionkey>;callsign=<call>` → XML with a `<Callsign>`
//!    block. A `<Session><Error>Session Timeout</Error></Session>` (or any
//!    error mentioning a session/timeout) triggers a single transparent
//!    re-auth + retry.
//!
//! The session key is cached behind a [`tokio::sync::Mutex`] and reused across
//! lookups; it is re-fetched on first use and whenever QRZ reports the session
//! has expired.

// rationale: the crate-wide `DxError` is intentionally a flat (non-boxed) enum
// for ergonomic `?`; boxing it crate-wide to satisfy this lint is out of scope.
#![allow(clippy::result_large_err)]

use crate::{DxError, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::Client;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::debug;

/// Default request timeout for QRZ XML calls (seconds).
const QRZ_XML_TIMEOUT_SECS: u64 = 30;

/// Parsed result of a QRZ callsign lookup.
///
/// Every field is optional: QRZ records vary widely in completeness and some
/// fields require a higher subscription tier. `call` is the (echoed) callsign
/// QRZ matched, which may differ in case/format from the queried string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QrzLookup {
    /// Callsign QRZ matched (`<call>`).
    pub call: Option<String>,
    /// Operator name — typically last name, or full name (`<name>` / `<fname>`).
    pub name: Option<String>,
    /// Maidenhead grid locator (`<grid>`).
    pub grid: Option<String>,
    /// Country / DXCC entity name (`<country>`).
    pub country: Option<String>,
    /// DXCC entity number (`<dxcc>`).
    pub dxcc: Option<String>,
    /// State / province (`<state>`), where applicable.
    pub state: Option<String>,
}

/// Client for the QRZ.com paid XML callsign-lookup API.
///
/// Holds the operator's credentials and a cached session key. Default-off:
/// only constructed when the operator has enabled `[network.qrz_xml]`. All
/// credentials and the session key are kept in memory and **never logged**
/// (log target `dx.qrz`).
pub struct QrzXmlClient {
    username: String,
    password: String,
    agent: String,
    base_url: String,
    client: Client,
    /// Cached QRZ session key; `None` until first auth, cleared on timeout.
    session_key: Mutex<Option<String>>,
}

impl QrzXmlClient {
    /// QRZ XML API base URL.
    const BASE_URL: &'static str = "https://xmldata.qrz.com/xml/current/";

    /// Build a new client from the operator's QRZ XML credentials.
    ///
    /// `agent` identifies pancetta to QRZ (e.g. `pancetta-<version>`); QRZ asks
    /// callers to send a descriptive agent string.
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
        agent: impl Into<String>,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            agent: agent.into(),
            base_url: Self::BASE_URL.to_string(),
            client: Client::builder()
                .timeout(Duration::from_secs(QRZ_XML_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|_| Client::new()),
            session_key: Mutex::new(None),
        }
    }

    /// Override the base URL (used only by tests / mock servers).
    #[cfg(test)]
    fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Look up a callsign, returning the parsed [`QrzLookup`].
    ///
    /// Authenticates lazily on first use and transparently re-authenticates
    /// once if QRZ reports the session has expired (`Session Timeout`). Any
    /// QRZ-reported error (bad credentials, callsign not found, …) maps to a
    /// [`DxError`]; **no credential or session-key value is ever included** in
    /// an error or log line.
    pub async fn lookup(&self, callsign: &str) -> Result<QrzLookup> {
        // Ensure we hold a session key (auth on first use).
        let key = self.session_key().await?;

        match self.lookup_with_key(&key, callsign).await {
            Ok(lookup) => Ok(lookup),
            Err(DxError::ExternalService(msg)) if is_session_expired(&msg) => {
                // Session expired between auth and lookup (or was stale in the
                // cache). Drop the cached key, re-auth, and retry once.
                debug!(target: "dx.qrz", "QRZ session expired; re-authenticating");
                self.invalidate_session().await;
                let key = self.session_key().await?;
                self.lookup_with_key(&key, callsign).await
            }
            Err(e) => Err(e),
        }
    }

    /// Return a valid session key, authenticating if the cache is empty.
    async fn session_key(&self) -> Result<String> {
        let mut guard = self.session_key.lock().await;
        if let Some(key) = guard.as_ref() {
            return Ok(key.clone());
        }
        let key = self.authenticate().await?;
        *guard = Some(key.clone());
        Ok(key)
    }

    /// Clear the cached session key so the next lookup re-authenticates.
    async fn invalidate_session(&self) {
        *self.session_key.lock().await = None;
    }

    /// Authenticate against QRZ and return a fresh session key.
    async fn authenticate(&self) -> Result<String> {
        // QRZ uses ';'-separated query params. reqwest would percent-encode a
        // raw ';' in a single query value, so build the query string directly.
        let query = format!(
            "username={};password={};agent={}",
            urlencode(&self.username),
            urlencode(&self.password),
            urlencode(&self.agent),
        );
        let url = format!("{}?{}", self.base_url, query);

        let body = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(DxError::Network)?
            .error_for_status()
            .map_err(DxError::Network)?
            .text()
            .await
            .map_err(DxError::Network)?;

        match parse_session(&body)? {
            SessionResult::Key(key) => {
                debug!(target: "dx.qrz", "QRZ authentication succeeded");
                Ok(key)
            }
            SessionResult::Error(msg) => {
                // `msg` is QRZ's own error text (e.g. "Username/password
                // incorrect"), never our credentials.
                Err(DxError::ExternalService(format!(
                    "QRZ authentication failed: {msg}"
                )))
            }
            SessionResult::None => Err(DxError::ExternalService(
                "QRZ authentication returned no session key or error".to_string(),
            )),
        }
    }

    /// Perform a single lookup with the given session key.
    async fn lookup_with_key(&self, key: &str, callsign: &str) -> Result<QrzLookup> {
        let query = format!("s={};callsign={}", urlencode(key), urlencode(callsign));
        let url = format!("{}?{}", self.base_url, query);

        let body = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(DxError::Network)?
            .error_for_status()
            .map_err(DxError::Network)?
            .text()
            .await
            .map_err(DxError::Network)?;

        // A session error in the response (timeout/invalid) is surfaced as an
        // ExternalService error so `lookup` can decide whether to re-auth.
        if let SessionResult::Error(msg) = parse_session(&body)? {
            return Err(DxError::ExternalService(format!("QRZ lookup error: {msg}")));
        }

        let lookup = parse_callsign(&body)?;
        if lookup == QrzLookup::default() {
            return Err(DxError::ExternalService(format!(
                "QRZ returned no callsign data for '{callsign}'"
            )));
        }
        Ok(lookup)
    }
}

/// Outcome of parsing a `<Session>` block.
enum SessionResult {
    /// Session key present (`<Key>`).
    Key(String),
    /// Session error present (`<Error>`); the next lookup decides what to do.
    Error(String),
    /// No session key or error (a pure data response, or none).
    None,
}

/// True if a QRZ session-error message indicates the session expired and a
/// re-auth should be attempted.
fn is_session_expired(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("session timeout")
        || lower.contains("invalid session")
        || lower.contains("session expired")
        || (lower.contains("session") && lower.contains("expire"))
}

/// Parse the `<Session>` block: returns a `<Key>` or an `<Error>` if present.
fn parse_session(xml: &str) -> Result<SessionResult> {
    // QRZ nests `<Key>`/`<Error>` inside `<Session>`. Extract the text of the
    // first `<Key>` (preferred) or `<Error>` we encounter.
    let key = extract_tag_text(xml, "Key")?;
    if let Some(key) = key {
        if !key.trim().is_empty() {
            return Ok(SessionResult::Key(key.trim().to_string()));
        }
    }
    if let Some(err) = extract_tag_text(xml, "Error")? {
        if !err.trim().is_empty() {
            return Ok(SessionResult::Error(err.trim().to_string()));
        }
    }
    Ok(SessionResult::None)
}

/// Parse a `<Callsign>` block into a [`QrzLookup`].
///
/// QRZ's callsign fields are flat text elements; we pull the ones pancetta
/// cares about. Missing fields stay `None`.
fn parse_callsign(xml: &str) -> Result<QrzLookup> {
    let text = |tag: &str| -> Result<Option<String>> {
        Ok(extract_tag_text(xml, tag)?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    };

    // Prefer the full name field if present, else the first name.
    let name = match text("name")? {
        Some(n) => Some(n),
        None => text("fname")?,
    };

    Ok(QrzLookup {
        call: text("call")?,
        name,
        grid: text("grid")?,
        country: text("country")?,
        dxcc: text("dxcc")?,
        state: text("state")?,
    })
}

/// Extract the inner text of the first `<tag>…</tag>` element with the given
/// local name (namespace-agnostic). Returns `Ok(None)` if absent.
///
/// Uses the already-present `quick-xml` reader rather than a fragile regex; it
/// tolerates QRZ's `<QRZDatabase>` namespace prefix because matching is on the
/// element's local name.
fn extract_tag_text(xml: &str, tag: &str) -> Result<Option<String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut in_target = false;
    let mut collected = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if local_name_eq(e.name().as_ref(), tag) {
                    in_target = true;
                    collected.clear();
                }
            }
            Ok(Event::Text(e)) if in_target => {
                let unescaped = e
                    .unescape()
                    .map_err(|err| DxError::Parse(format!("QRZ XML decode error: {err}")))?;
                collected.push_str(&unescaped);
            }
            Ok(Event::End(e)) => {
                if in_target && local_name_eq(e.name().as_ref(), tag) {
                    return Ok(Some(collected));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DxError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(None)
}

/// Compare an XML element's (possibly namespace-prefixed) raw name to a target
/// local name, case-insensitively on the local part.
fn local_name_eq(raw: &[u8], target: &str) -> bool {
    // Strip an optional `prefix:` namespace qualifier.
    let local = match raw.iter().rposition(|&b| b == b':') {
        Some(pos) => &raw[pos + 1..],
        None => raw,
    };
    local.eq_ignore_ascii_case(target.as_bytes())
}

/// Minimal percent-encoder for query-parameter values.
///
/// QRZ separates params with `;`, so we hand-build the query string and encode
/// only the characters that would break it or the URL (kept dependency-free —
/// `url`'s form serializer would also re-encode `;`).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTH_OK: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<QRZDatabase version="1.34" xmlns="http://xmldata.qrz.com">
  <Session>
    <Key>2331uf894c4bd29f3923f3bacf02c532d7bd9</Key>
    <Count>123</Count>
    <SubExp>Wed Jan 1 12:34:03 2030</SubExp>
    <GMTime>Sun Aug 16 03:51:47 2026</GMTime>
  </Session>
</QRZDatabase>"#;

    const LOOKUP_OK: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<QRZDatabase version="1.34" xmlns="http://xmldata.qrz.com">
  <Callsign>
    <call>AA7BQ</call>
    <fname>FRED L</fname>
    <name>LLOYD</name>
    <grid>DM43bp</grid>
    <country>United States</country>
    <dxcc>291</dxcc>
    <state>AZ</state>
  </Callsign>
  <Session>
    <Key>2331uf894c4bd29f3923f3bacf02c532d7bd9</Key>
    <Count>124</Count>
  </Session>
</QRZDatabase>"#;

    const SESSION_TIMEOUT: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<QRZDatabase version="1.34" xmlns="http://xmldata.qrz.com">
  <Session>
    <Error>Session Timeout</Error>
    <GMTime>Sun Aug 16 03:51:47 2026</GMTime>
  </Session>
</QRZDatabase>"#;

    const AUTH_BAD_CREDS: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<QRZDatabase version="1.34" xmlns="http://xmldata.qrz.com">
  <Session>
    <Error>Username/password incorrect</Error>
  </Session>
</QRZDatabase>"#;

    #[test]
    fn parse_session_extracts_key() {
        match parse_session(AUTH_OK).unwrap() {
            SessionResult::Key(k) => {
                assert_eq!(k, "2331uf894c4bd29f3923f3bacf02c532d7bd9");
            }
            _ => panic!("expected a session key"),
        }
    }

    #[test]
    fn parse_session_extracts_error() {
        match parse_session(SESSION_TIMEOUT).unwrap() {
            SessionResult::Error(e) => assert_eq!(e, "Session Timeout"),
            _ => panic!("expected a session error"),
        }
        assert!(!matches!(
            parse_session(SESSION_TIMEOUT).unwrap(),
            SessionResult::None
        ));
    }

    #[test]
    fn parse_session_prefers_key_over_absent_error() {
        // The lookup response carries a Key (no Error) -> Key.
        match parse_session(LOOKUP_OK).unwrap() {
            SessionResult::Key(_) => {}
            _ => panic!("expected a session key"),
        }
    }

    #[test]
    fn parse_callsign_extracts_fields() {
        let l = parse_callsign(LOOKUP_OK).unwrap();
        assert_eq!(l.call.as_deref(), Some("AA7BQ"));
        // Prefers <name> (last name) over <fname>.
        assert_eq!(l.name.as_deref(), Some("LLOYD"));
        assert_eq!(l.grid.as_deref(), Some("DM43bp"));
        assert_eq!(l.country.as_deref(), Some("United States"));
        assert_eq!(l.dxcc.as_deref(), Some("291"));
        assert_eq!(l.state.as_deref(), Some("AZ"));
    }

    #[test]
    fn parse_callsign_falls_back_to_fname() {
        let xml = r#"<QRZDatabase><Callsign><call>K5ARH</call><fname>TONY</fname><grid>EM12</grid></Callsign></QRZDatabase>"#;
        let l = parse_callsign(xml).unwrap();
        assert_eq!(l.call.as_deref(), Some("K5ARH"));
        assert_eq!(l.name.as_deref(), Some("TONY"));
        assert_eq!(l.grid.as_deref(), Some("EM12"));
        assert_eq!(l.country, None);
    }

    #[test]
    fn parse_callsign_empty_is_default() {
        let xml = r#"<QRZDatabase><Session><Key>abc</Key></Session></QRZDatabase>"#;
        assert_eq!(parse_callsign(xml).unwrap(), QrzLookup::default());
    }

    #[test]
    fn is_session_expired_matches_qrz_text() {
        assert!(is_session_expired("Session Timeout"));
        assert!(is_session_expired("QRZ lookup error: Session Timeout"));
        assert!(is_session_expired("Invalid session key"));
        assert!(!is_session_expired("Username/password incorrect"));
        assert!(!is_session_expired("Not found: AA7BQ"));
    }

    #[test]
    fn auth_bad_creds_is_error_not_key() {
        match parse_session(AUTH_BAD_CREDS).unwrap() {
            SessionResult::Error(e) => assert!(e.contains("incorrect")),
            _ => panic!("expected a session error"),
        }
    }

    #[test]
    fn local_name_eq_handles_namespace_prefix() {
        assert!(local_name_eq(b"call", "call"));
        assert!(local_name_eq(b"qrz:call", "call"));
        assert!(local_name_eq(b"Call", "call"));
        assert!(!local_name_eq(b"callsign", "call"));
    }

    #[test]
    fn urlencode_preserves_unreserved_encodes_specials() {
        assert_eq!(urlencode("K5ARH"), "K5ARH");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("p;w=d"), "p%3Bw%3Dd");
        assert_eq!(urlencode("a.b-c_d~e"), "a.b-c_d~e");
    }

    #[test]
    fn client_constructs_with_base_url_override() {
        let c = QrzXmlClient::new("user", "pass", "pancetta-test")
            .with_base_url("http://127.0.0.1:0/xml/current/");
        assert_eq!(c.username, "user");
        assert_eq!(c.agent, "pancetta-test");
        assert!(c.base_url.starts_with("http://127.0.0.1"));
    }

    // ----- Live-protocol flow tests against a local mock HTTP server. --------
    // wiremock is not a dev-dependency of pancetta-dx; we stand up a tiny
    // single-threaded TCP server that serves canned XML so the full
    // auth -> lookup -> re-auth flow is exercised without a network dep.

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex as StdMutex};

    /// A minimal HTTP server that returns successive canned bodies, one per
    /// request, recording each request's raw query string.
    fn spawn_mock(responses: Vec<&'static str>) -> (String, Arc<StdMutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/xml/current/");
        let queries: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let queries_thread = Arc::clone(&queries);

        std::thread::spawn(move || {
            for (i, resp) in responses.into_iter().enumerate() {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                // First line: "GET /xml/current/?<query> HTTP/1.1"
                if let Some(first) = req.lines().next() {
                    if let Some(q) = first.split_whitespace().nth(1) {
                        queries_thread.lock().unwrap().push(q.to_string());
                    }
                }
                let body = resp.as_bytes();
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(http.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
                let _ = i; // request index unused beyond ordering
            }
        });

        (base, queries)
    }

    #[tokio::test]
    async fn auth_then_lookup_parses_fields() {
        // Request 1 = auth (returns Key), request 2 = lookup (returns Callsign).
        let (base, queries) = spawn_mock(vec![AUTH_OK, LOOKUP_OK]);
        let client = QrzXmlClient::new("user", "pass", "pancetta-test").with_base_url(base);

        let lookup = client.lookup("AA7BQ").await.unwrap();
        assert_eq!(lookup.call.as_deref(), Some("AA7BQ"));
        assert_eq!(lookup.grid.as_deref(), Some("DM43bp"));
        assert_eq!(lookup.dxcc.as_deref(), Some("291"));

        let qs = queries.lock().unwrap();
        assert_eq!(qs.len(), 2, "expected one auth + one lookup request");
        assert!(qs[0].contains("username=user"), "auth query: {}", qs[0]);
        assert!(
            qs[0].contains("agent=pancetta-test"),
            "auth query: {}",
            qs[0]
        );
        assert!(qs[1].contains("callsign=AA7BQ"), "lookup query: {}", qs[1]);
        assert!(qs[1].contains("s="), "lookup should carry session key");
    }

    #[tokio::test]
    async fn session_timeout_triggers_reauth() {
        // Seed a (stale) cached session key, then: lookup#1 -> Session Timeout,
        // re-auth -> new Key, lookup#2 -> Callsign.
        let (base, queries) = spawn_mock(vec![SESSION_TIMEOUT, AUTH_OK, LOOKUP_OK]);
        let client = QrzXmlClient::new("user", "pass", "pancetta-test").with_base_url(base);
        *client.session_key.lock().await = Some("stale-key".to_string());

        let lookup = client.lookup("AA7BQ").await.unwrap();
        assert_eq!(lookup.call.as_deref(), Some("AA7BQ"));

        let qs = queries.lock().unwrap();
        assert_eq!(qs.len(), 3, "expected lookup, re-auth, lookup: {qs:?}");
        // First request used the stale key.
        assert!(qs[0].contains("s=stale-key"), "first lookup: {}", qs[0]);
        // Second request is the re-auth (has username, no session key param).
        assert!(qs[1].contains("username=user"), "re-auth: {}", qs[1]);
        // Third request is the retried lookup with the fresh key.
        assert!(qs[2].contains("callsign=AA7BQ"), "retry lookup: {}", qs[2]);
    }

    #[tokio::test]
    async fn bad_credentials_surface_as_error() {
        let (base, _queries) = spawn_mock(vec![AUTH_BAD_CREDS]);
        let client = QrzXmlClient::new("user", "wrong", "pancetta-test").with_base_url(base);
        let err = client.lookup("AA7BQ").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("authentication failed"), "msg: {msg}");
        // The error must NOT leak the password.
        assert!(!msg.contains("wrong"), "error leaked credential: {msg}");
    }
}
