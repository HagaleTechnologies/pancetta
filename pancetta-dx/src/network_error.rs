//! URL-free wrapper for `reqwest` failures.
//!
//! `reqwest::Error` prints its request URL in both `Display` and `Debug`.
//! Some pancetta-dx requests carry an account credential or session key in
//! the query string (QRZ XML, LoTW report download), so network errors are
//! stored without their URL. The field is private to this module, so the
//! only way to build a [`NetworkError`] is `From<reqwest::Error>`, which
//! drops the URL first.

use thiserror::Error;

/// A `reqwest` failure with its request URL removed.
///
/// Keeps the operation and status text (e.g. `error sending request`,
/// `HTTP status server error (503 Service Unavailable)`) and the source
/// chain, never the URL.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct NetworkError(reqwest::Error);

impl From<reqwest::Error> for NetworkError {
    fn from(e: reqwest::Error) -> Self {
        Self(e.without_url())
    }
}
