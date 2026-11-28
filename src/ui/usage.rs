//! The Usage page: token use and its API-equivalent cost over a range, with filters, a
//! breakdown by one dimension and a per-day (per-hour for Today) list. All state is in the
//! query string, so a view can be bookmarked. A filter value that names nothing gives an empty
//! table, never an error; only a malformed custom date is a 422.

use std::collections::BTreeSet;

use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use time::macros::format_description;
use time::{Date, Duration, OffsetDateTime, Time};

use super::SessionCtx;
use super::views::{
    BoardQuery, Labels, ProjectLabel, ProjectOption, Pulse, UiError, UiResult, board_filter, html,
    project_options, pulse_counts,
};
use crate::domain::{AgentId, Billing, ProjectId, TaskId};
use crate::http::AppState;
use crate::store::{TaskScope, UsageFilter, UsageGroup, UsageRow};

/// Agents offered in the agent, harness and host selects.
const AGENT_OPTIONS: u32 = 500;
/// The longest gap-filled over-time list; a longer span lists only the days with usage.
const MAX_FILLED: i64 = 400;

#[derive(Deserialize, Default)]
pub struct UsageQuery {
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
    project: Option<String>,
    agent: Option<String>,
    harness: Option<String>,
    host: Option<String>,
    task: Option<String>,
    model: Option<String>,
    billing: Option<String>,
    group: Option<String>,
}

/// A trimmed, non-empty query value.
fn given(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// "845", "12.3k", "1.24M", "3.10B".
pub fn compact(n: f64) -> String {
    let n = n.max(0.0);
    if n < 999.5 {
        format!("{n:.0}")
    } else if n < 999_950.0 {
        format!("{:.1}k", n / 1e3)
    } else if n < 999_995_000.0 {
        format!("{:.2}M", n / 1e6)
    } else {
        format!("{:.2}B", n / 1e9)
    }
}

/// `1234567` as "1,234,567".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "$0.00", "<$0.01", "$3.40", "$1,234.56".
pub fn usd(micros: f64) -> String {
    let micros = micros.max(0.0);
    if micros == 0.0 {
        return "$0.00".into();
    }
    if micros < 10_000.0 {
        return "<$0.01".into();
    }
    let cents = (micros / 10_000.0).round() as u64;
    format!("${}.{:02}", thousands(cents / 100), cents % 100)
}

/// The exact cost for a `title`: dollars with 6 decimals.
fn usd_exact(micros: f64) -> String {
    let micros = micros.max(0.0).round() as u64;
    format!(
        "${}.{:06}",
        thousands(micros / 1_000_000),
        micros % 1_000_000
    )
}

/// The exact token count for a `title`.
fn tokens_exact(n: f64) -> String {
    thousands(n.max(0.0).round() as u64)
}

pub(super) fn tokens(r: &UsageRow) -> f64 {
    r.input + r.output + r.cache_write + r.cache_read
}

/// Whether any record of the row has a price.
fn priced(r: &UsageRow) -> bool {
    r.unpriced < r.requests || r.requests == 0
}

/// "no price"; "$3.40", plus " est." when any record was not billed per token and
/// " + N no price" when some had no price.
pub(super) fn cost_text(r: &UsageRow) -> String {
    if !priced(r) {
        return "no price".into();
    }
    let mut s = usd(r.cost_micros);
    if r.estimated > 0 {
        s.push_str(" est.");
    }
    if r.unpriced > 0 {
        s.push_str(&format!(" + {} no price", r.unpriced));
    }
    s
}

/// `data-share` (4 decimals, 0..1) and the visible percentage.
fn share(part: f64, whole: f64) -> (String, String) {
    let f = if whole > 0.0 {
        (part / whole).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let pct = (f * 100.0).round();
    let text = if f > 0.0 && pct < 1.0 {
        "<1%".to_string()
    } else {
        format!("{pct:.0}%")
    };
    (format!("{f:.4}"), text)
}

// ---------------------------------------------------------------- range

#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Today,
    Week,
    Month,
    All,
    Custom,
}

impl Range {
    const ALL: [(Range, &'static str, &'static str); 5] = [
        (Range::Today, "today", "Today"),
        (Range::Week, "7d", "7 days"),
        (Range::Month, "30d", "30 days"),
        (Range::All, "all", "All time"),
        (Range::Custom, "custom", "From/To dates"),
    ];

    fn parse(q: &UsageQuery) -> Range {
        let dated = given(&q.from).is_some() || given(&q.to).is_some();
        match given(&q.range) {
            None if dated => Range::Custom,
            Some(raw) => Self::ALL
                .iter()
                .find(|(_, v, _)| *v == raw)
                .map_or(Range::Week, |(r, _, _)| *r),
            None => Range::Week,
        }
    }
}

fn parse_day(raw: &str) -> UiResult<Date> {
    Date::parse(raw, format_description!("[year]-[month]-[day]"))
        .map_err(|_| UiError::Invalid("Dates are YYYY-MM-DD.".into()))
}

fn midnight(d: Date) -> OffsetDateTime {
    d.with_time(Time::MIDNIGHT).assume_utc()
}

/// The time bounds of the range and the first and last day (or hour) the over-time list
/// covers (`None`: from the first row with usage).
struct Span {
    from: Option<OffsetDateTime>,
    to: Option<OffsetDateTime>,
    first: Option<Date>,
    last: Date,
    /// The custom dates as given, swapped into order.
    from_day: String,
    to_day: String,
}

fn span(range: Range, q: &UsageQuery, now: OffsetDateTime) -> UiResult<Span> {
    let today = now.date();
    let rolling = |days: i64| {
        let from = now - Duration::days(days);
        Span {
            from: Some(from),
            to: None,
            first: Some(from.date()),
            last: today,
            from_day: String::new(),
            to_day: String::new(),
        }
    };
    Ok(match range {
        Range::Today => Span {
            from: Some(midnight(today)),
            to: None,
            first: Some(today),
            last: today,
            from_day: String::new(),
            to_day: String::new(),
        },
        Range::Week => rolling(7),
        Range::Month => rolling(30),
        Range::All => Span {
            from: None,
            to: None,
            first: None,
            last: today,
            from_day: String::new(),
            to_day: String::new(),
        },
        Range::Custom => {
            let mut from = given(&q.from).map(parse_day).transpose()?;
            let mut to = given(&q.to).map(parse_day).transpose()?;
            if let (Some(f), Some(t)) = (from, to)
                && f > t
            {
                (from, to) = (Some(t), Some(f));
            }
            Span {
                from: from.map(midnight),
                to: to.and_then(Date::next_day).map(midnight),
                first: from,
                last: to.unwrap_or(today),
                from_day: from.map(|d| d.to_string()).unwrap_or_default(),
                to_day: to.map(|d| d.to_string()).unwrap_or_default(),
            }
        }
    })
}

// ---------------------------------------------------------------- filters

const GROUPS: [(UsageGroup, &str, &str); 7] = [
    (UsageGroup::Project, "project", "Project"),
    (UsageGroup::Task, "task", "Task"),
    (UsageGroup::Agent, "agent", "Agent"),
    (UsageGroup::Harness, "harness", "Harness"),
    (UsageGroup::Host, "host", "Host"),
    (UsageGroup::Model, "model", "Model"),
    (UsageGroup::Day, "day", "Day"),
];

pub struct Opt {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

fn opts<'a>(values: impl IntoIterator<Item = (&'a str, &'a str)>, current: &str) -> Vec<Opt> {
    values
        .into_iter()
        .map(|(value, label)| Opt {
            value: value.to_string(),
            label: label.to_string(),
            selected: value == current,
        })
        .collect()
}

/// Distinct values as options, keeping a current value no agent has.
fn value_opts(mut values: BTreeSet<String>, current: Option<&str>) -> Vec<Opt> {
    if let Some(c) = current {
        values.insert(c.to_string());
    }
    values
        .into_iter()
        .map(|v| Opt {
            selected: current == Some(v.as_str()),
            label: v.clone(),
            value: v,
        })
        .collect()
}

// ---------------------------------------------------------------- views

pub struct Amount {
    pub short: String,
    pub exact: String,
}

fn amount(n: f64) -> Amount {
    Amount {
        short: compact(n),
        exact: tokens_exact(n),
    }
}

pub struct Totals {
    pub input: Amount,
    pub output: Amount,
    pub cache_write: Amount,
    pub cache_read: Amount,
    pub cost: Amount,
    pub subscription: Amount,
}

pub struct UsageName {
    pub text: String,
    pub href: Option<String>,
    pub project: Option<ProjectLabel>,
    /// "no task" / "no project" and other names that are not a thing's own name.
    pub none: bool,
}

pub struct BreakdownRow {
    pub name: UsageName,
    pub requests: i64,
    pub input: Amount,
    pub output: Amount,
    pub cache_write: Amount,
    pub cache_read: Amount,
    pub cost: String,
    pub cost_exact: String,
    pub share: String,
    pub share_text: String,
}

pub struct TimeRow {
    /// RFC 3339-ish for `<time datetime>`.
    pub datetime: String,
    pub label: String,
    pub tokens: Amount,
    pub cost: String,
    pub share: String,
}

#[derive(Template)]
#[template(path = "usage.html")]
struct UsagePage {
    csrf: Option<String>,
    logout: bool,
    pulse: Option<Pulse>,
    nav: &'static str,
    ranges: Vec<Opt>,
    custom: bool,
    from_day: String,
    to_day: String,
    projects: Vec<ProjectOption>,
    project_any: bool,
    agents: Vec<Opt>,
    agent_any: bool,
    harnesses: Vec<Opt>,
    hosts: Vec<Opt>,
    billings: Vec<Opt>,
    model: String,
    task: String,
    groups: Vec<Opt>,
    group_label: &'static str,
    totals: Totals,
    share_header: &'static str,
    rows: Vec<BreakdownRow>,
    hourly: bool,
    days: Vec<TimeRow>,
}

async fn name(
    labels: &mut Labels<'_>,
    group: UsageGroup,
    key: Option<&str>,
) -> UiResult<UsageName> {
    let plain = |text: &str, none: bool| UsageName {
        text: text.to_string(),
        href: None,
        project: None,
        none,
    };
    let Some(key) = key else {
        return Ok(match group {
            UsageGroup::Task => plain("no task", true),
            UsageGroup::Project => plain("no project", true),
            _ => plain("none", true),
        });
    };
    Ok(match group {
        UsageGroup::Project => match key.parse::<ProjectId>() {
            Ok(id) => {
                let p = labels.project(id);
                UsageName {
                    text: p.name.clone(),
                    href: None,
                    project: Some(p),
                    none: false,
                }
            }
            Err(_) => plain(key, false),
        },
        UsageGroup::Task => match key.parse::<TaskId>() {
            Ok(id) => match labels.task_title(id).await? {
                Some(title) => UsageName {
                    text: title,
                    href: Some(format!("/ui/tasks/{id}")),
                    project: None,
                    none: false,
                },
                None => plain(key, false),
            },
            Err(_) => plain(key, false),
        },
        UsageGroup::Agent => match key.parse::<AgentId>() {
            Ok(id) => plain(&labels.agent_label(id).await?, false),
            Err(_) => plain(key, false),
        },
        _ => plain(key, false),
    })
}

/// Usage by cost, rows without any priced record last, then by tokens.
fn sort_rows(rows: &mut [UsageRow]) {
    rows.sort_by(|a, b| {
        priced(b)
            .cmp(&priced(a))
            .then(b.cost_micros.total_cmp(&a.cost_micros))
            .then(tokens(b).total_cmp(&tokens(a)))
            .then(a.key.cmp(&b.key))
    });
}

/// One row per day from `first` to `last` (per hour of today up to `hourly`, when given),
/// zero rows for gaps.
fn time_rows(
    rows: &[UsageRow],
    hourly: Option<OffsetDateTime>,
    first: Option<Date>,
    last: Date,
) -> Vec<TimeRow> {
    let by_key: std::collections::HashMap<&str, &UsageRow> = rows
        .iter()
        .filter_map(|r| r.key.as_deref().map(|k| (k, r)))
        .collect();
    let mut keys: Vec<String> = if let Some(now) = hourly {
        (0..=now.hour())
            .map(|h| format!("{}T{h:02}", now.date()))
            .collect()
    } else {
        let data_first = rows
            .iter()
            .filter_map(|r| r.key.as_deref())
            .filter_map(|k| parse_day(k).ok())
            .min();
        match first.or(data_first) {
            Some(f) if (last - f).whole_days() <= MAX_FILLED => {
                let mut out = Vec::new();
                let mut d = f;
                while d <= last {
                    out.push(d.to_string());
                    match d.next_day() {
                        Some(n) => d = n,
                        None => break,
                    }
                }
                out
            }
            _ => by_key.keys().map(|k| k.to_string()).collect(),
        }
    };
    keys.sort();
    let zero = UsageRow::default();
    let busiest = rows.iter().map(tokens).fold(0.0, f64::max);
    keys.into_iter()
        .map(|k| {
            let r = by_key.get(k.as_str()).copied().unwrap_or(&zero);
            let (share, _) = share(tokens(r), busiest);
            let (datetime, label) = if hourly.is_some() {
                let hour = k.get(11..13).unwrap_or("00").to_string();
                (format!("{}:00Z", k), format!("{hour}:00"))
            } else {
                (k.clone(), k.clone())
            };
            TimeRow {
                datetime,
                label,
                tokens: amount(tokens(r)),
                cost: cost_text(r),
                share,
            }
        })
        .collect()
}

/// `GET /ui/usage`.
pub async fn usage_page(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Query(q): Query<UsageQuery>,
) -> UiResult<Response> {
    let now = state.clock.now();
    let range = Range::parse(&q);
    let span = span(range, &q, now)?;
    let group_raw = given(&q.group).unwrap_or("project");
    let (group, group_value, group_label) = GROUPS
        .iter()
        .find(|(_, v, _)| *v == group_raw)
        .copied()
        .unwrap_or(GROUPS[0]);

    // Each filter value either narrows the query or names nothing (`matches` false).
    let mut matches = true;
    let project = match board_filter(
        &*state.store,
        &BoardQuery {
            project: q.project.clone(),
        },
    )
    .await
    {
        Ok(p) => p,
        Err(UiError::NotFound(_)) => {
            matches = false;
            None
        }
        Err(e) => return Err(e),
    };
    let agent = given(&q.agent).and_then(|raw| {
        let id = raw.parse::<AgentId>().ok();
        matches &= id.is_some();
        id
    });
    let task = given(&q.task).and_then(|raw| {
        let scope = if raw.eq_ignore_ascii_case("none") {
            Some(TaskScope::NoTask)
        } else {
            raw.parse::<TaskId>()
                .ok()
                .map(|id| TaskScope::Task { id, subtasks: true })
        };
        matches &= scope.is_some();
        scope
    });
    let billing = given(&q.billing).and_then(|raw| {
        let b = raw.parse::<Billing>().ok();
        matches &= b.is_some();
        b
    });
    let harness = given(&q.harness).map(str::to_string);
    let host = given(&q.host).map(str::to_string);
    let model = given(&q.model).map(str::to_string);
    let filter = UsageFilter {
        from: span.from,
        to: span.to,
        project: project.as_ref().map(|p| p.id),
        agent,
        harness: harness.clone(),
        host: host.clone(),
        task,
        model: model.clone(),
        billing,
    };

    let hourly = range == Range::Today;
    let (total, mut rows, time) = if matches {
        let total = state
            .usage
            .summary(&filter, UsageGroup::Total)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let rows = state.usage.summary(&filter, group).await?;
        let over = if hourly {
            UsageGroup::Hour
        } else {
            UsageGroup::Day
        };
        let time = state.usage.summary(&filter, over).await?;
        (total, rows, time)
    } else {
        (UsageRow::default(), Vec::new(), Vec::new())
    };
    rows.retain(|r| r.requests > 0);
    sort_rows(&mut rows);

    let by_cost = total.cost_micros > 0.0;
    let mut labels = Labels::load(&*state.store).await?;
    let mut breakdown = Vec::with_capacity(rows.len());
    for r in &rows {
        let (share, share_text) = if by_cost {
            share(r.cost_micros, total.cost_micros)
        } else {
            share(tokens(r), tokens(&total))
        };
        breakdown.push(BreakdownRow {
            name: name(&mut labels, group, r.key.as_deref()).await?,
            requests: r.requests,
            input: amount(r.input),
            output: amount(r.output),
            cache_write: amount(r.cache_write),
            cache_read: amount(r.cache_read),
            cost: cost_text(r),
            cost_exact: usd_exact(r.cost_micros),
            share,
            share_text,
        });
    }

    let agents = state.store.list_agents(AGENT_OPTIONS).await?;
    let mut agent_opts = Vec::with_capacity(agents.len());
    for a in &agents {
        let project = a
            .project_id
            .map(|p| labels.project(p).name)
            .unwrap_or_else(|| "no project".into());
        agent_opts.push(Opt {
            value: a.id.to_string(),
            label: format!("{}@{} · {project}", a.harness, a.host),
            selected: agent == Some(a.id),
        });
    }
    agent_opts.sort_by(|a, b| a.label.cmp(&b.label));
    let harnesses = value_opts(
        agents.iter().map(|a| a.harness.clone()).collect(),
        harness.as_deref(),
    );
    let hosts = value_opts(
        agents.iter().map(|a| a.host.clone()).collect(),
        host.as_deref(),
    );
    let range_value = Range::ALL
        .iter()
        .find(|(r, _, _)| *r == range)
        .map_or("7d", |(_, v, _)| v);

    html(&UsagePage {
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(&state).await?),
        nav: "usage",
        ranges: opts(Range::ALL.iter().map(|(_, v, l)| (*v, *l)), range_value),
        custom: range == Range::Custom,
        from_day: span.from_day,
        to_day: span.to_day,
        projects: project_options(&labels, project.as_ref().map(|p| p.id)),
        project_any: project.is_none(),
        agent_any: !agent_opts.iter().any(|o| o.selected),
        agents: agent_opts,
        harnesses,
        hosts,
        billings: opts(
            [
                ("", "Any billing"),
                ("subscription", "subscription"),
                ("api", "api"),
                ("unknown", "unknown"),
            ],
            billing.map_or("", |b| b.as_str()),
        ),
        model: model.unwrap_or_default(),
        task: given(&q.task).unwrap_or_default().to_string(),
        groups: opts(GROUPS.iter().map(|(_, v, l)| (*v, *l)), group_value),
        group_label,
        totals: Totals {
            input: amount(total.input),
            output: amount(total.output),
            cache_write: amount(total.cache_write),
            cache_read: amount(total.cache_read),
            cost: Amount {
                short: usd(total.cost_micros),
                exact: usd_exact(total.cost_micros),
            },
            subscription: Amount {
                short: usd(total.estimated_cost_micros),
                exact: usd_exact(total.estimated_cost_micros),
            },
        },
        share_header: if by_cost {
            "Share of cost"
        } else {
            "Share of tokens"
        },
        rows: breakdown,
        hourly,
        days: time_rows(&time, hourly.then_some(now), span.first, span.last),
    })
}
