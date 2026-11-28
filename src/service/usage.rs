//! Token usage: validating and storing reported records, and summaries over them.

use std::sync::Arc;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::clock::Clock;
use crate::service::identity::Resolved;
use crate::store::{
    NewUsage, Store, StoreError, StoreResult, UsageBatch, UsageCounts, UsageFilter, UsageGroup,
    UsageRow,
};
use crate::usage::prices::PriceTable;
use crate::usage::report::{
    IngestOutcome, MAX_AGE, MAX_COUNT, MAX_FUTURE, MAX_KEY_BYTES, MAX_MODEL_BYTES, Rejected,
    UsageReport, WireRecord,
};

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    /// The request as a whole is unusable; nothing was stored.
    #[error("{0}")]
    Envelope(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct UsageService {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    prices: Arc<PriceTable>,
}

impl UsageService {
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>, prices: Arc<PriceTable>) -> Self {
        Self {
            store,
            clock,
            prices,
        }
    }

    pub fn prices(&self) -> &PriceTable {
        &self.prices
    }

    /// Validates the envelope (an error stores nothing) and each record (a bad one is
    /// rejected alone), then stores the good ones in one store call.
    pub async fn ingest(
        &self,
        r: &Resolved,
        report: &UsageReport,
    ) -> Result<IngestOutcome, UsageError> {
        check_envelope(report)?;
        let now = self.clock.now();
        let mut outcome = IngestOutcome::default();
        let mut records = Vec::with_capacity(report.records.len());
        for value in &report.records {
            match validate(value, now) {
                Ok(rec) => records.push(rec),
                Err(rejected) => outcome.rejected.push(rejected),
            }
        }
        if !records.is_empty() {
            let written = self
                .store
                .insert_usage(
                    &UsageBatch {
                        session: r.session.id,
                        agent: &r.agent,
                        token: r.session.token_id,
                        parser: &report.parser,
                        harness_session_id: report.harness_session_id.as_deref(),
                        records: &records,
                        now,
                    },
                    &*self.prices,
                )
                .await?;
            outcome.accepted = written.inserted;
            outcome.duplicate = written.duplicate;
        }
        Ok(outcome)
    }

    pub async fn summary(
        &self,
        filter: &UsageFilter,
        group: UsageGroup,
    ) -> StoreResult<Vec<UsageRow>> {
        self.store.usage_summary(filter, group).await
    }
}

fn check_envelope(report: &UsageReport) -> Result<(), UsageError> {
    let bad = |m: &str| Err(UsageError::Envelope(m.to_string()));
    if !parser_ok(&report.parser) {
        return bad("parser must be at most 32 bytes like name/1");
    }
    if report
        .harness_version
        .as_deref()
        .is_some_and(|v| v.len() > 64)
    {
        return bad("harness_version over 64 bytes");
    }
    if report
        .harness_session_id
        .as_deref()
        .is_some_and(|v| v.len() > 200)
    {
        return bad("harness_session_id over 200 bytes");
    }
    Ok(())
}

/// `[a-z0-9-]+/[0-9]+`, at most 32 bytes.
fn parser_ok(p: &str) -> bool {
    if p.is_empty() || p.len() > 32 {
        return false;
    }
    let Some((name, version)) = p.split_once('/') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
}

fn validate(value: &serde_json::Value, now: OffsetDateTime) -> Result<NewUsage, Rejected> {
    let reject = |key: &str, reason: &str| Rejected {
        key: key.to_string(),
        reason: reason.to_string(),
    };
    let Ok(w) = serde_json::from_value::<WireRecord>(value.clone()) else {
        let key = value.get("key").and_then(|k| k.as_str()).unwrap_or("");
        return Err(reject(key, "malformed record"));
    };
    let key = w.key.as_str();
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(reject(key, "key empty or over 200 bytes"));
    }
    if w.model.is_empty()
        || w.model.len() > MAX_MODEL_BYTES
        || !w
            .model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@-".contains(&b))
    {
        return Err(reject(
            key,
            "model empty, over 128 bytes or has characters outside A-Za-z0-9._:/@-",
        ));
    }
    let counts = [
        w.input,
        w.output,
        w.cache_write_5m,
        w.cache_write_1h,
        w.cache_read,
        w.reasoning,
    ];
    if counts.iter().any(|c| !(0..=MAX_COUNT).contains(c)) {
        return Err(reject(key, "count below 0 or over 1000000000"));
    }
    let Ok(at) = OffsetDateTime::parse(&w.at, &Rfc3339) else {
        return Err(reject(key, "timestamp not RFC 3339"));
    };
    if at > now + MAX_FUTURE {
        return Err(reject(key, "timestamp more than 5 minutes ahead"));
    }
    if at < now - MAX_AGE {
        return Err(reject(key, "timestamp more than 30 days old"));
    }
    if let Some(s) = &w.speed
        && (s.len() > 16 || !s.bytes().all(|b| b.is_ascii_lowercase()))
    {
        return Err(reject(key, "speed over 16 bytes or not a-z"));
    }
    Ok(NewUsage {
        key: w.key,
        model: w.model,
        counts: UsageCounts {
            input: w.input,
            output: w.output,
            cache_write_5m: w.cache_write_5m,
            cache_write_1h: w.cache_write_1h,
            cache_read: w.cache_read,
            reasoning: w.reasoning,
        },
        speed: w.speed,
        billing: w.billing,
        occurred_at: at,
    })
}
