//! `codex/1`: Codex rollout JSONL (checked against 0.172.0).
//!
//! Each line is `{"timestamp", "type", "payload"}`. A usage record is a `token_usage_record`
//! line, keyed by `payload.response_id`. Older rollouts only have `event_msg` lines with
//! `payload.type == "token_count"`; their `info.last_token_usage` is the fallback, keyed
//! `<session_meta id>:<ordinal>` and dropped from a file that also has real records
//! ([`super::drop_fallback`]). The model is the latest `turn_context.model`.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{Line, ParsedRecord, Parser, ParserState};
use crate::store::UsageCounts;

pub(super) struct Codex;

impl Parser for Codex {
    fn id(&self) -> &'static str {
        "codex/1"
    }

    /// The rollout alone.
    fn files(&self, transcript: &Path) -> Vec<PathBuf> {
        vec![transcript.to_path_buf()]
    }

    fn line(&self, line: &[u8], state: &mut ParserState) -> Line {
        // The head names the line; only the four kinds below are read further, so a long
        // `response_item` is scanned once and nothing in it is kept.
        let Ok(head) = serde_json::from_slice::<Head>(line) else {
            return Line::Other;
        };
        let event = head.payload.and_then(|p| p.kind);
        match (head.kind.as_deref(), event.as_deref()) {
            (Some("session_meta"), _) => {
                let id = payload::<SessionMeta>(line).and_then(|m| m.id);
                if let Some(id) = id.filter(|id| !id.is_empty()) {
                    state.session_meta_id = Some(id);
                }
                Line::Other
            }
            (Some("turn_context"), _) => {
                let model = payload::<TurnContext>(line).and_then(|t| t.model);
                if let Some(model) = model.filter(|m| !m.is_empty()) {
                    state.model = Some(model);
                }
                Line::Other
            }
            (Some("token_usage_record"), _) => {
                let Some(TokenUsageRecord {
                    response_id,
                    usage: Some(usage),
                }) = payload::<TokenUsageRecord>(line)
                else {
                    return Line::UsageShaped;
                };
                let key = response_id.filter(|k| !k.is_empty());
                let (Some(key), Some(model), Some(at)) =
                    (key, state.model.clone(), timestamp(head.timestamp))
                else {
                    return Line::UsageShaped;
                };
                state.saw_records = true;
                Line::Record(record(key, at, model, &usage, false))
            }
            (Some("event_msg"), Some("token_count")) => {
                let Some(count) = payload::<TokenCount>(line) else {
                    return Line::UsageShaped;
                };
                let Some(usage) = count.info.and_then(|i| i.last_token_usage) else {
                    return Line::Other;
                };
                let (Some(meta_id), Some(model), Some(at)) = (
                    state.session_meta_id.clone(),
                    state.model.clone(),
                    timestamp(head.timestamp),
                ) else {
                    return Line::UsageShaped;
                };
                state.ordinal += 1;
                let key = format!("{meta_id}:{}", state.ordinal);
                Line::Record(record(key, at, model, &usage, true))
            }
            _ => Line::Other,
        }
    }
}

/// The line's `payload` as `T`, or `None` when it does not read as one.
fn payload<T: DeserializeOwned>(line: &[u8]) -> Option<T> {
    serde_json::from_slice::<Envelope<T>>(line)
        .ok()
        .map(|e| e.payload)
}

fn timestamp(t: Option<String>) -> Option<OffsetDateTime> {
    t.and_then(|t| OffsetDateTime::parse(&t, &Rfc3339).ok())
}

fn record(
    key: String,
    at: OffsetDateTime,
    model: String,
    usage: &Usage,
    fallback: bool,
) -> ParsedRecord {
    ParsedRecord {
        key,
        at,
        model,
        counts: usage.counts(),
        speed: None,
        fallback,
    }
}

// The structs below name only the fields the record needs; serde skips every other field,
// so prompt and response text is never kept.

#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    payload: Option<HeadPayload>,
}

#[derive(Deserialize)]
struct HeadPayload {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    payload: T,
}

#[derive(Deserialize)]
struct SessionMeta {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct TurnContext {
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct TokenUsageRecord {
    #[serde(default)]
    response_id: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct TokenCount {
    #[serde(default)]
    info: Option<TokenCountInfo>,
}

#[derive(Deserialize)]
struct TokenCountInfo {
    #[serde(default)]
    last_token_usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    cached_input_tokens: Option<i64>,
    #[serde(default)]
    cache_write_input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    reasoning_output_tokens: Option<i64>,
}

impl Usage {
    fn counts(&self) -> UsageCounts {
        let n = |v: Option<i64>| v.unwrap_or(0);
        let cached = n(self.cached_input_tokens);
        UsageCounts {
            // Codex's input includes cached input, as in the OpenAI API.
            input: n(self.input_tokens).saturating_sub(cached).max(0),
            cache_read: cached,
            // Open question (spec, settled question 3): whether `cache_write_input_tokens`
            // is inside `input_tokens`. Settling it needs real rollouts, which tests may not
            // read, so this keeps the spec's mapping: a separate 5-minute cache write.
            cache_write_5m: n(self.cache_write_input_tokens),
            cache_write_1h: 0,
            output: n(self.output_tokens),
            reasoning: n(self.reasoning_output_tokens),
        }
    }
}
