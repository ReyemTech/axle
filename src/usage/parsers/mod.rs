//! Harness log parsers: each turns one log line into at most one usage record.
//!
//! A parser copies only named numeric fields, the model id, opaque ids and the timestamp
//! into [`ParsedRecord`]; nothing from a log line is forwarded wholesale (spec, "Privacy").
//! A changed log format gets a new parser id (`claude-code/2`), and both stay while old
//! logs exist.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::domain::Billing;
use crate::hook::Harness;
use crate::store::UsageCounts;
use crate::usage::report::WireRecord;

mod claude_code;
mod codex;

/// One model response's usage, read from a harness log.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRecord {
    pub key: String,
    pub at: OffsetDateTime,
    pub model: String,
    pub counts: UsageCounts,
    pub speed: Option<String>,
    /// From Codex's `token_count` fallback (dropped when the file has real records).
    pub fallback: bool,
}

impl ParsedRecord {
    /// The record as `POST /v1/usage` takes it.
    pub fn to_wire(&self, billing: Billing) -> WireRecord {
        WireRecord {
            key: self.key.clone(),
            at: self.at.format(&Rfc3339).unwrap_or_default(),
            model: self.model.clone(),
            input: self.counts.input,
            output: self.counts.output,
            cache_write_5m: self.counts.cache_write_5m,
            cache_write_1h: self.counts.cache_write_1h,
            cache_read: self.counts.cache_read,
            reasoning: self.counts.reasoning,
            speed: self.speed.clone(),
            billing,
        }
    }
}

/// Survives between runs in the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserState {
    pub model: Option<String>,
    pub session_meta_id: Option<String>,
    pub ordinal: u64,
    pub saw_records: bool,
}

/// What one log line is.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Record(ParsedRecord),
    /// Looks like usage, but the parser does not understand it.
    UsageShaped,
    Other,
}

pub trait Parser: Sync {
    fn id(&self) -> &'static str;
    /// The files one hook run reads, main transcript first.
    fn files(&self, transcript: &Path) -> Vec<PathBuf>;
    fn line(&self, line: &[u8], state: &mut ParserState) -> Line;
}

static CLAUDE_CODE: claude_code::ClaudeCode = claude_code::ClaudeCode;
static CODEX: codex::Codex = codex::Codex;

/// The current parser for a harness's logs.
pub fn for_harness(h: Harness) -> &'static dyn Parser {
    match h {
        Harness::ClaudeCode => &CLAUDE_CODE,
        Harness::Codex => &CODEX,
    }
}

/// The parser with this id (a cursor names the one that read it).
pub fn by_id(id: &str) -> Option<&'static dyn Parser> {
    [&CLAUDE_CODE as &'static dyn Parser, &CODEX]
        .into_iter()
        .find(|p| p.id() == id)
}

/// Drops `fallback` records (Codex `token_count`) from a file whose state saw real
/// records: the two describe the same responses under different keys, so the server
/// could not dedupe them.
pub fn drop_fallback(records: &mut Vec<ParsedRecord>, state: &ParserState) {
    if state.saw_records {
        records.retain(|r| !r.fallback);
    }
}

/// Parses a whole file (tests, fixtures): every `Line::Record`, in order, without the
/// fallback records [`drop_fallback`] drops.
pub fn parse_all(p: &dyn Parser, bytes: &[u8], state: &mut ParserState) -> Vec<ParsedRecord> {
    let mut records = bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .filter_map(|l| match p.line(l, state) {
            Line::Record(r) => Some(r),
            Line::UsageShaped | Line::Other => None,
        })
        .collect();
    drop_fallback(&mut records, state);
    records
}
