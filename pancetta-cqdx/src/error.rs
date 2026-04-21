//! Error types for the cqdx.io client.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CqdxError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parsing failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Authentication failed: invalid or expired PAT")]
    Unauthorized,

    #[error("Server error: {status} — {message}")]
    Server { status: u16, message: String },

    #[error("Not configured: no PAT token provided")]
    NotConfigured,

    #[error("Response too large: {0} bytes")]
    ResponseTooLarge(u64),

    #[error("Invalid base URL: {0}")]
    InvalidBaseUrl(String),
}

pub type Result<T> = std::result::Result<T, CqdxError>;
