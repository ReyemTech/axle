//! API price table: the built-in `prices.toml` plus an optional operator override file.
//!
//! Prices are USD per million tokens, which is micro-USD per token, so a record's cost in
//! micro-USD is `round(sum(tokens_k * price_k))`.

use std::path::Path;

use serde::Deserialize;
use toml::Spanned;

use crate::store::{Priced, Pricer, UsageCounts};

/// The built-in table.
pub const BUILTIN: &str = include_str!("prices.toml");

#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    #[error("{file}: line {line}: {message}")]
    Bad {
        file: String,
        line: usize,
        message: String,
    },
    #[error("cannot read {file}: {source}")]
    Io {
        file: String,
        source: std::io::Error,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    version: String,
    #[serde(default)]
    model: Vec<RawModel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    id: Spanned<String>,
    #[serde(rename = "match", default)]
    globs: Vec<Spanned<String>>,
    input: Spanned<f64>,
    output: Spanned<f64>,
    cache_write_5m: Option<Spanned<f64>>,
    cache_write_1h: Option<Spanned<f64>>,
    cache_read: Option<Spanned<f64>>,
    fast: Option<RawTier>,
    long_context: Option<RawLongContext>,
}

/// A tier overlays any of the five prices.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTier {
    input: Option<Spanned<f64>>,
    output: Option<Spanned<f64>>,
    cache_write_5m: Option<Spanned<f64>>,
    cache_write_1h: Option<Spanned<f64>>,
    cache_read: Option<Spanned<f64>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLongContext {
    above: i64,
    input: Option<Spanned<f64>>,
    output: Option<Spanned<f64>>,
    cache_write_5m: Option<Spanned<f64>>,
    cache_write_1h: Option<Spanned<f64>>,
    cache_read: Option<Spanned<f64>>,
}

/// Prices a tier sets; `None` keeps the layer below.
#[derive(Debug, Clone, Copy, Default)]
struct Layer {
    input: Option<f64>,
    output: Option<f64>,
    cache_write_5m: Option<f64>,
    cache_write_1h: Option<f64>,
    cache_read: Option<f64>,
}

impl Layer {
    fn overlay(&mut self, top: &Layer) {
        self.input = top.input.or(self.input);
        self.output = top.output.or(self.output);
        self.cache_write_5m = top.cache_write_5m.or(self.cache_write_5m);
        self.cache_write_1h = top.cache_write_1h.or(self.cache_write_1h);
        self.cache_read = top.cache_read.or(self.cache_read);
    }
}

#[derive(Debug, Clone)]
struct Entry {
    id: String,
    globs: Vec<String>,
    base: Layer,
    fast: Option<Layer>,
    long_context: Option<(i64, Layer)>,
    /// `builtin:<version>` or `file:<version>`.
    origin: String,
}

/// The built-in prices with an optional override merged over them.
#[derive(Debug, Clone)]
pub struct PriceTable {
    /// Version of the table this was built from (the built-in one; an override keeps it).
    version: String,
    entries: Vec<Entry>,
}

fn line_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    text.as_bytes()[..end]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

fn bad(file: &str, text: &str, offset: usize, message: impl Into<String>) -> PriceError {
    PriceError::Bad {
        file: file.to_string(),
        line: line_of(text, offset),
        message: message.into(),
    }
}

fn price(
    v: &Option<Spanned<f64>>,
    what: &str,
    id_at: usize,
    file: &str,
    text: &str,
) -> Result<Option<f64>, PriceError> {
    let Some(v) = v else { return Ok(None) };
    check(v, what, id_at, file, text).map(Some)
}

fn check(
    v: &Spanned<f64>,
    what: &str,
    id_at: usize,
    file: &str,
    text: &str,
) -> Result<f64, PriceError> {
    let n = *v.get_ref();
    if n.is_finite() && n >= 0.0 {
        Ok(n)
    } else {
        // Report the entry's own line, as the brief asks for semantic errors.
        Err(bad(
            file,
            text,
            id_at,
            format!("{what} must be zero or a positive number"),
        ))
    }
}

fn layer_of(
    t: [&Option<Spanned<f64>>; 5],
    id_at: usize,
    file: &str,
    text: &str,
) -> Result<Layer, PriceError> {
    Ok(Layer {
        input: price(t[0], "input", id_at, file, text)?,
        output: price(t[1], "output", id_at, file, text)?,
        cache_write_5m: price(t[2], "cache_write_5m", id_at, file, text)?,
        cache_write_1h: price(t[3], "cache_write_1h", id_at, file, text)?,
        cache_read: price(t[4], "cache_read", id_at, file, text)?,
    })
}

/// `*` matches any run of characters; everything else matches itself.
fn glob_matches(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = s.chars().collect();
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

impl PriceTable {
    /// Parses the built-in table. A bad built-in file is a bug, so this panics.
    pub fn builtin() -> PriceTable {
        let (version, entries) = Self::parse_entries(BUILTIN, "builtin prices.toml", "builtin")
            .unwrap_or_else(|e| panic!("built-in price table is invalid: {e}"));
        PriceTable { version, entries }
    }

    /// A standalone table from one file (entries are marked `file:<version>`).
    pub fn parse(text: &str, file: &str) -> Result<PriceTable, PriceError> {
        let (version, entries) = Self::parse_entries(text, file, "file")?;
        Ok(PriceTable { version, entries })
    }

    /// `text` merged over `self`: an entry with the same `id` replaces the existing one
    /// entirely, new ids are appended.
    pub fn with_override(mut self, text: &str, file: &str) -> Result<PriceTable, PriceError> {
        let (_, over) = Self::parse_entries(text, file, "file")?;
        for e in over {
            match self.entries.iter_mut().find(|x| x.id == e.id) {
                Some(slot) => *slot = e,
                None => self.entries.push(e),
            }
        }
        Ok(self)
    }

    /// The built-in table, then the file at `path` merged over it.
    pub fn load(path: Option<&Path>) -> Result<PriceTable, PriceError> {
        let table = PriceTable::builtin();
        let Some(path) = path else { return Ok(table) };
        let file = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| PriceError::Io {
            file: file.clone(),
            source,
        })?;
        table.with_override(&text, &file)
    }

    /// Version of the built-in table.
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn parse_entries(
        text: &str,
        file: &str,
        kind: &str,
    ) -> Result<(String, Vec<Entry>), PriceError> {
        let raw: RawFile = toml::from_str(text).map_err(|e| {
            let at = e.span().map(|s| s.start).unwrap_or(0);
            bad(file, text, at, e.message().to_string())
        })?;
        let origin = format!("{kind}:{}", raw.version);
        let mut entries: Vec<Entry> = Vec::with_capacity(raw.model.len());
        for m in &raw.model {
            let id_at = m.id.span().start;
            let id = m.id.get_ref().clone();
            if id.is_empty() {
                return Err(bad(file, text, id_at, "id must not be empty"));
            }
            if entries.iter().any(|e| e.id == id) {
                return Err(bad(file, text, id_at, format!("duplicate id {id:?}")));
            }
            let mut globs = Vec::with_capacity(m.globs.len());
            for g in &m.globs {
                if g.get_ref().is_empty() {
                    return Err(bad(file, text, id_at, "match glob must not be empty"));
                }
                globs.push(g.get_ref().clone());
            }
            let base = layer_of(
                [
                    &Some(m.input.clone()),
                    &Some(m.output.clone()),
                    &m.cache_write_5m,
                    &m.cache_write_1h,
                    &m.cache_read,
                ],
                id_at,
                file,
                text,
            )?;
            let fast = match &m.fast {
                Some(t) => Some(layer_of(
                    [
                        &t.input,
                        &t.output,
                        &t.cache_write_5m,
                        &t.cache_write_1h,
                        &t.cache_read,
                    ],
                    id_at,
                    file,
                    text,
                )?),
                None => None,
            };
            let long_context = match &m.long_context {
                Some(l) => {
                    if l.above < 0 {
                        return Err(bad(
                            file,
                            text,
                            id_at,
                            "long_context.above must not be negative",
                        ));
                    }
                    Some((
                        l.above,
                        layer_of(
                            [
                                &l.input,
                                &l.output,
                                &l.cache_write_5m,
                                &l.cache_write_1h,
                                &l.cache_read,
                            ],
                            id_at,
                            file,
                            text,
                        )?,
                    ))
                }
                None => None,
            };
            entries.push(Entry {
                id,
                globs,
                base,
                fast,
                long_context,
                origin: origin.clone(),
            });
        }
        Ok((raw.version, entries))
    }

    /// Exact `id`; else the entry with the longest matching glob (first listed on a tie).
    fn lookup(&self, model: &str) -> Option<&Entry> {
        if let Some(e) = self.entries.iter().find(|e| e.id == model) {
            return Some(e);
        }
        let mut best: Option<(usize, &Entry)> = None;
        for e in &self.entries {
            for g in &e.globs {
                let len = g.chars().count();
                if best.is_none_or(|(l, _)| len > l) && glob_matches(g, model) {
                    best = Some((len, e));
                }
            }
        }
        best.map(|(_, e)| e)
    }
}

impl Pricer for PriceTable {
    fn price(&self, model: &str, speed: Option<&str>, counts: &UsageCounts) -> Option<Priced> {
        let e = self.lookup(model)?;
        let mut p = e.base;
        if let Some((above, tier)) = &e.long_context {
            let prompt = counts
                .input
                .saturating_add(counts.cache_write_5m)
                .saturating_add(counts.cache_write_1h)
                .saturating_add(counts.cache_read);
            if prompt > *above {
                p.overlay(tier);
            }
        }
        if let (Some("fast"), Some(tier)) = (speed, &e.fast) {
            p.overlay(tier);
        }
        let input = p.input?;
        let cw5m = p.cache_write_5m.unwrap_or(input);
        let cw1h = p.cache_write_1h.unwrap_or(cw5m);
        let cache_read = p.cache_read.unwrap_or(input);
        let cost = counts.input as f64 * input
            + counts.output as f64 * p.output?
            + counts.cache_write_5m as f64 * cw5m
            + counts.cache_write_1h as f64 * cw1h
            + counts.cache_read as f64 * cache_read;
        Some(Priced {
            cost_micros: cost.round() as i64,
            price_ref: format!("{}#{}", e.origin, e.id),
        })
    }
}
