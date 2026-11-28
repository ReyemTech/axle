//! `claude-code/1`: Claude Code transcript JSONL (checked against 2.1.329).
//!
//! A usage record is a `type: "assistant"` line with an object `message.usage`. Each content
//! block of one response is its own line repeating `requestId`, `message.id` and an identical
//! `usage`, so records share a key and the collector keeps one per key.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::IgnoredAny;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{Line, ParsedRecord, Parser, ParserState};
use crate::store::UsageCounts;

pub(super) struct ClaudeCode;

impl Parser for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code/1"
    }

    /// The transcript, then `<dir>/<stem>/subagents/*.jsonl` sorted by name.
    fn files(&self, transcript: &Path) -> Vec<PathBuf> {
        let mut files = vec![transcript.to_path_buf()];
        let (Some(dir), Some(stem)) = (transcript.parent(), transcript.file_stem()) else {
            return files;
        };
        let Ok(entries) = std::fs::read_dir(dir.join(stem).join("subagents")) else {
            return files;
        };
        let mut subagents: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl") && p.is_file())
            .collect();
        subagents.sort();
        files.extend(subagents);
        files
    }

    fn line(&self, line: &[u8], state: &mut ParserState) -> Line {
        let Ok(entry) = serde_json::from_slice::<Entry>(line) else {
            return shape_of(line);
        };
        if entry.kind.as_deref() != Some("assistant") {
            return Line::Other;
        }
        let Some(message) = entry.message else {
            return Line::Other;
        };
        let Some(usage) = message.usage else {
            return Line::Other;
        };
        let counts = usage.counts();
        let model = strip_suffix(message.model.as_deref().unwrap_or_default());
        if model.is_empty() || model == "<synthetic>" || priced_total_is_zero(&counts) {
            return Line::Other;
        }
        let key = [entry.request_id, message.id]
            .into_iter()
            .flatten()
            .find(|k| !k.is_empty());
        let at = entry
            .timestamp
            .and_then(|t| OffsetDateTime::parse(&t, &Rfc3339).ok());
        let (Some(key), Some(at)) = (key, at) else {
            return Line::UsageShaped;
        };
        state.saw_records = true;
        Line::Record(ParsedRecord {
            key,
            at,
            model: model.to_string(),
            counts,
            speed: match usage.speed {
                Some(Speed::Name(s)) => Some(s),
                Some(Speed::Other(_)) | None => None,
            },
            fallback: false,
        })
    }
}

/// A line the record struct could not read: usage-shaped when an assistant line still has
/// a `message.usage`, else not ours.
fn shape_of(line: &[u8]) -> Line {
    match serde_json::from_slice::<Shape>(line) {
        Ok(Shape {
            kind: Some(kind),
            message: Some(ShapeMessage { usage: Some(_) }),
        }) if kind == "assistant" => Line::UsageShaped,
        _ => Line::Other,
    }
}

/// `claude-opus-5-5[1m]` → `claude-opus-5-5`.
fn strip_suffix(model: &str) -> &str {
    match (model.ends_with(']'), model.rfind('[')) {
        (true, Some(i)) => &model[..i],
        _ => model,
    }
}

fn priced_total_is_zero(c: &UsageCounts) -> bool {
    [
        c.input,
        c.output,
        c.cache_write_5m,
        c.cache_write_1h,
        c.cache_read,
    ]
    .iter()
    .all(|n| *n == 0)
}

// The structs below name only the fields the record needs; serde skips every other field,
// so prompt and response text is never kept.

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "requestId", default)]
    request_id: Option<String>,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
    #[serde(default)]
    output_tokens_details: Option<OutputDetails>,
    #[serde(default)]
    speed: Option<Speed>,
}

impl Usage {
    fn counts(&self) -> UsageCounts {
        let n = |v: Option<i64>| v.unwrap_or(0);
        let (cache_write_5m, cache_write_1h) = match &self.cache_creation {
            Some(CacheCreation::Split(s)) => (
                n(s.ephemeral_5m_input_tokens),
                n(s.ephemeral_1h_input_tokens),
            ),
            Some(CacheCreation::Other(_)) | None => (n(self.cache_creation_input_tokens), 0),
        };
        UsageCounts {
            input: n(self.input_tokens),
            output: n(self.output_tokens),
            cache_write_5m,
            cache_write_1h,
            cache_read: n(self.cache_read_input_tokens),
            reasoning: self
                .output_tokens_details
                .as_ref()
                .and_then(|d| d.thinking_tokens)
                .unwrap_or(0),
        }
    }
}

/// `usage.cache_creation` counts only when it is an object.
#[derive(Deserialize)]
#[serde(untagged)]
enum CacheCreation {
    Split(CacheSplit),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
struct CacheSplit {
    #[serde(default)]
    ephemeral_5m_input_tokens: Option<i64>,
    #[serde(default)]
    ephemeral_1h_input_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct OutputDetails {
    #[serde(default)]
    thinking_tokens: Option<i64>,
}

/// `usage.speed` counts only when it is a string.
#[derive(Deserialize)]
#[serde(untagged)]
enum Speed {
    Name(String),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
struct Shape {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<ShapeMessage>,
}

#[derive(Deserialize)]
struct ShapeMessage {
    #[serde(default)]
    usage: Option<IgnoredAny>,
}
