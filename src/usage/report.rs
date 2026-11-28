//! The wire types of `POST /v1/usage` and its limits.

use serde::{Deserialize, Serialize};

use crate::domain::{Billing, SessionId};

/// Records in one request.
pub const MAX_RECORDS: usize = 500;
/// Request body size.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_KEY_BYTES: usize = 200;
pub const MAX_MODEL_BYTES: usize = 128;
/// Largest accepted value of any token count.
pub const MAX_COUNT: i64 = 1_000_000_000;
/// How far ahead of the server clock a record may be dated.
pub const MAX_FUTURE: time::Duration = time::Duration::minutes(5);
/// How old a record may be.
pub const MAX_AGE: time::Duration = time::Duration::days(30);

/// A batch of usage records from one session.
#[derive(Debug, Serialize, Deserialize)]
pub struct UsageReport {
    pub session_id: SessionId,
    pub parser: String,
    #[serde(default)]
    pub harness_version: Option<String>,
    #[serde(default)]
    pub harness_session_id: Option<String>,
    /// Decoded one by one, so a malformed record is rejected alone.
    pub records: Vec<serde_json::Value>,
}

/// One record as sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRecord {
    pub key: String,
    /// RFC 3339.
    pub at: String,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub cache_read: i64,
    pub reasoning: i64,
    #[serde(default)]
    pub speed: Option<String>,
    pub billing: Billing,
}

/// A record the server did not store, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejected {
    pub key: String,
    pub reason: String,
}

/// The answer to a report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestOutcome {
    pub accepted: u32,
    pub duplicate: u32,
    pub rejected: Vec<Rejected>,
}
