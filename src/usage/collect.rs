//! One collector run: read each harness log from its cursor within the per-run caps, send
//! the new records to `POST /v1/usage`, and advance the cursors only when the server took
//! the batch (2xx) or refused it for good (400, 403, 404). Everything else keeps them, so
//! the next run resends and the server dedupes.
//!
//! Fail-open: a run returns at most one line for stderr, never the token or a URL.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::bridge::session_file::SessionInfo;
use crate::client::{AxlDir, ClientConfig, load_config, read_token, usage_off};
use crate::hook::Harness;
use crate::store::UsageCounts;
use crate::usage::billing;
use crate::usage::cursor::{self, Cursor};
use crate::usage::parsers::{self, Line, ParsedRecord, Parser, ParserState};
use crate::usage::report::{IngestOutcome, MAX_RECORDS, UsageReport};

/// Bytes read from one file per run.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Time spent parsing per run, across all files.
pub const PARSE_BUDGET: Duration = Duration::from_millis(300);
/// Cursors untouched for this long are deleted on SessionEnd.
pub const CURSOR_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// The whole `POST /v1/usage` request.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Collect<'a> {
    pub harness: Harness,
    pub home: &'a Path,
    /// The hook's stdin (JSON): `transcript_path`, `session_id`.
    pub stdin: &'a str,
    /// The bridge's session and its session file's mtime, when `find_session` found one.
    pub session: Option<(SessionInfo, SystemTime)>,
    pub env: &'a (dyn Fn(&str) -> Option<String> + Sync),
    /// SessionEnd: also prune old cursors.
    pub session_end: bool,
}

/// The per-run caps. [`collect`] uses the defaults; tests shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_records: usize,
    pub parse_budget: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            max_records: MAX_RECORDS,
            parse_budget: PARSE_BUDGET,
        }
    }
}

/// One run. `None`: nothing to say; `Some(line)`: the single stderr line.
pub async fn collect(c: Collect<'_>) -> Option<String> {
    collect_with(c, Limits::default()).await
}

/// [`collect`] with explicit caps.
pub async fn collect_with(c: Collect<'_>, limits: Limits) -> Option<String> {
    let axl_usage = (c.env)("AXL_USAGE");
    if usage_off(axl_usage.as_deref(), &ClientConfig::default()).is_some() {
        return None;
    }
    // A config that does not parse may hold `enabled = false`: never guess it is on.
    let Ok(cfg) = load_config(&AxlDir::new(c.home)) else {
        return Some("config.toml unreadable; usage off".to_string());
    };
    if usage_off(axl_usage.as_deref(), &cfg).is_some() {
        return None;
    }
    let (home, session_end) = (c.home, c.session_end);
    let line = run(c, &cfg.usage.unwrap_or_default(), limits).await;
    if session_end {
        cursor::prune(home, SystemTime::now(), CURSOR_MAX_AGE);
    }
    line
}

#[derive(Default, Deserialize)]
struct HookStdin {
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

async fn run(c: Collect<'_>, cfg: &crate::client::UsageConfig, limits: Limits) -> Option<String> {
    let stdin: HookStdin = serde_json::from_str(c.stdin).unwrap_or_default();
    let transcript = PathBuf::from(stdin.transcript_path.filter(|p| !p.trim().is_empty())?);
    let parser = parsers::for_harness(c.harness);
    let files = parser.files(&transcript);
    let main = cursor::load(c.home, &transcript);

    let (session_id, server_url, session_floor) = match &c.session {
        Some((info, mtime)) => (info.session_id, info.server_url.clone(), rfc3339(*mtime)),
        None => {
            let m = main.as_ref()?;
            (m.session_id?, m.server_url.clone()?, None)
        }
    };
    let token = read_token((c.env)("AXL_TOKEN").as_deref(), &AxlDir::new(c.home)).ok()?;
    let billing = cfg
        .billing_for(c.harness.name())
        .or_else(|| main.as_ref().and_then(|m| m.billing))
        .unwrap_or_else(|| billing::detect(c.harness, c.home, c.env));

    // What every cursor written this run carries; a new cursor also takes the floor.
    let template = Cursor {
        path: PathBuf::new(),
        dev: 0,
        ino: 0,
        offset: 0,
        parser: parser.id().to_string(),
        state: ParserState::default(),
        billing: Some(billing),
        session_id: Some(session_id),
        server_url: Some(server_url.clone()),
        floor: session_floor.or_else(|| main.as_ref().and_then(|m| m.floor.clone())),
        skip_partial: false,
        warned: false,
        updated_at: rfc3339(SystemTime::now()).unwrap_or_default(),
    };
    let unreadable = Cursor {
        path: transcript.clone(),
        warned: true,
        ..template.clone()
    };
    let home = c.home.to_path_buf();
    let read = tokio::task::spawn_blocking(move || {
        read_all(parser, &home, files, main, &template, limits)
    })
    .await
    .ok()?;
    let mut reads = match read {
        Ok(r) => r,
        Err(e) => {
            // Once per session: the cursor (keyed by path, so no file is needed) remembers it.
            if cursor::load(c.home, &transcript).is_some_and(|m| m.warned) {
                return None;
            }
            let _ = cursor::save(c.home, &unreadable);
            return Some(format!(
                "cannot read the transcript for usage ({})",
                e.kind()
            ));
        }
    };

    let records = merge(reads.iter_mut().flat_map(|r| r.records.drain(..)));
    if records.is_empty() {
        let unrecognised = reads
            .iter_mut()
            .find(|r| r.shaped && !r.any_record && !r.cursor.warned);
        let line = unrecognised.map(|r| {
            r.cursor.warned = true;
            format!("usage parser {} does not recognise this log", parser.id())
        });
        save_all(c.home, &reads);
        return line;
    }

    let report = UsageReport {
        session_id,
        parser: parser.id().to_string(),
        harness_version: None,
        harness_session_id: stdin.session_id,
        records: records
            .iter()
            .map(|r| serde_json::to_value(r.to_wire(billing)))
            .collect::<Result<_, _>>()
            .ok()?,
    };
    let client = reqwest::Client::builder()
        .timeout(SEND_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let sent = client
        .post(format!("{}/v1/usage", server_url.trim_end_matches('/')))
        .bearer_auth(&token)
        .json(&report)
        .send()
        .await;
    let res = match sent {
        Ok(res) => res,
        Err(e) => return Some(format!("usage report not sent ({}); will retry", reason(e))),
    };
    let status = res.status();
    if status.is_success() {
        save_all(c.home, &reads);
        let mut s = cursor::load_status(c.home);
        s.last_success_at = rfc3339(SystemTime::now());
        s.billing.insert(c.harness.name().to_string(), billing);
        let _ = cursor::save_status(c.home, &s);
        let outcome: IngestOutcome = res.json().await.unwrap_or_default();
        return outcome.rejected.first().map(|first| {
            format!(
                "usage report: {} of {} records rejected ({})",
                outcome.rejected.len(),
                records.len(),
                first.reason
            )
        });
    }
    let code = status.as_u16();
    if matches!(code, 400 | 403 | 404) {
        save_all(c.home, &reads);
        return Some(format!(
            "usage report refused by the server (HTTP {code}); skipped"
        ));
    }
    Some(format!("usage report not sent (HTTP {code}); will retry"))
}

/// A send failure in a few words, without the URL.
fn reason(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "connection failed".into()
    } else {
        e.without_url().to_string()
    }
}

fn rfc3339(t: SystemTime) -> Option<String> {
    OffsetDateTime::from(t).format(&Rfc3339).ok()
}

/// Cursors are best effort: a failed write only means the next run resends.
fn save_all(home: &Path, reads: &[FileRead]) {
    for r in reads {
        let _ = cursor::save(home, &r.cursor);
    }
}

/// Merges records with the same key: the first wins, each count is the larger.
fn merge(records: impl Iterator<Item = ParsedRecord>) -> Vec<ParsedRecord> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<ParsedRecord> = Vec::new();
    for r in records {
        match index.get(&r.key) {
            Some(&i) => max_counts(&mut out[i].counts, &r.counts),
            None => {
                index.insert(r.key.clone(), out.len());
                out.push(r);
            }
        }
    }
    out
}

fn max_counts(a: &mut UsageCounts, b: &UsageCounts) {
    a.input = a.input.max(b.input);
    a.output = a.output.max(b.output);
    a.cache_write_5m = a.cache_write_5m.max(b.cache_write_5m);
    a.cache_write_1h = a.cache_write_1h.max(b.cache_write_1h);
    a.cache_read = a.cache_read.max(b.cache_read);
    a.reasoning = a.reasoning.max(b.reasoning);
}

/// One file's share of a run: its cursor as it would be saved, and its records.
struct FileRead {
    cursor: Cursor,
    records: Vec<ParsedRecord>,
    /// A line looked like usage but did not parse.
    shaped: bool,
    /// A record was parsed (even one the floor dropped).
    any_record: bool,
}

impl FileRead {
    fn empty(cursor: Cursor) -> Self {
        Self {
            cursor,
            records: Vec::new(),
            shaped: false,
            any_record: false,
        }
    }
}

/// What is left of this run's caps.
struct Budget {
    deadline: Instant,
    records_left: usize,
    max_bytes: u64,
}

/// Reads every file; only the main transcript (first) failing is an error, a subagent file
/// that cannot be read is skipped.
fn read_all(
    parser: &dyn Parser,
    home: &Path,
    files: Vec<PathBuf>,
    main: Option<Cursor>,
    template: &Cursor,
    limits: Limits,
) -> io::Result<Vec<FileRead>> {
    let mut budget = Budget {
        deadline: Instant::now() + limits.parse_budget,
        records_left: limits.max_records,
        max_bytes: limits.max_file_bytes,
    };
    let mut main = Some(main);
    let mut out = Vec::with_capacity(files.len());
    for path in files {
        let existing = match main.take() {
            Some(m) => m,
            None => cursor::load(home, &path),
        };
        match read_file(parser, &path, existing, template, &mut budget) {
            Ok(r) => out.push(r),
            Err(e) if out.is_empty() => return Err(e),
            Err(_) => {}
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn file_id(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

#[cfg(not(unix))]
fn file_id(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn read_file(
    parser: &dyn Parser,
    path: &Path,
    existing: Option<Cursor>,
    template: &Cursor,
    budget: &mut Budget,
) -> io::Result<FileRead> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let (dev, ino) = file_id(&meta);
    let mut cur = match existing {
        Some(c) => Cursor {
            billing: template.billing,
            session_id: template.session_id,
            server_url: template.server_url.clone(),
            updated_at: template.updated_at.clone(),
            ..c
        },
        None => Cursor {
            dev,
            ino,
            ..template.clone()
        },
    };
    cur.path = path.to_path_buf();
    // A new inode, a shrunk file or another parser's cursor: start over (the server dedupes).
    if cur.dev != dev || cur.ino != ino || meta.len() < cur.offset || cur.parser != parser.id() {
        cur.dev = dev;
        cur.ino = ino;
        cur.offset = 0;
        cur.state = ParserState::default();
        cur.skip_partial = false;
        cur.parser = parser.id().to_string();
    }

    file.seek(SeekFrom::Start(cur.offset))?;
    let mut buf = Vec::new();
    (&mut file).take(budget.max_bytes).read_to_end(&mut buf)?;
    let full = buf.len() as u64 == budget.max_bytes;

    let mut start = 0;
    if cur.skip_partial {
        // The rest of a line longer than the cap: discard it through its newline.
        match buf.iter().position(|b| *b == b'\n') {
            Some(i) => {
                start = i + 1;
                cur.skip_partial = false;
            }
            None => {
                cur.offset += buf.len() as u64;
                return Ok(FileRead::empty(cur));
            }
        }
    }
    let Some(last) = buf[start..].iter().rposition(|b| *b == b'\n') else {
        if full && start == 0 {
            // One line longer than the cap: never parseable within it, so skip it.
            cur.offset += buf.len() as u64;
            cur.skip_partial = true;
        } else {
            // A partial last line waits for its newline.
            cur.offset += start as u64;
        }
        return Ok(FileRead::empty(cur));
    };
    let end = start + last + 1;

    let floor = cur
        .floor
        .as_deref()
        .and_then(|f| OffsetDateTime::parse(f, &Rfc3339).ok());
    let mut out = FileRead::empty(cur.clone());
    let mut state = cur.state.clone();
    let mut capped = full;
    // The last fallback record: where its line starts, the state before it, its index.
    let mut last_fallback: Option<(usize, ParserState, usize)> = None;
    let mut pos = start;
    while pos < end {
        if budget.records_left == 0 || Instant::now() >= budget.deadline {
            capped = true;
            break;
        }
        let nl = buf[pos..end]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(end - 1, |i| pos + i);
        let line = &buf[pos..nl];
        let line_start = pos;
        pos = nl + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let before = state.clone();
        match parser.line(line, &mut state) {
            Line::Record(r) => {
                out.any_record = true;
                if floor.is_some_and(|f| r.at < f) {
                    continue;
                }
                if r.fallback {
                    last_fallback = Some((line_start, before, out.records.len()));
                }
                out.records.push(r);
                budget.records_left = budget.records_left.saturating_sub(1);
            }
            Line::UsageShaped => out.shaped = true,
            Line::Other => {}
        }
    }

    // A cap cut a batch that has fallback records and no real one yet: the real record for
    // the last fallback may be the next line, so that line is read again next run rather
    // than sent twice under two keys. Not when it is the batch's first line (no progress).
    if capped
        && !state.saw_records
        && let Some((at, before, i)) = last_fallback.filter(|(at, ..)| *at > start)
    {
        out.records.truncate(i);
        state = before;
        pos = at;
    }
    parsers::drop_fallback(&mut out.records, &state);
    out.cursor.offset = cur.offset + pos as u64;
    out.cursor.state = state;
    Ok(out)
}
