//! The unified inbox (spec §12, §9): the human's inbox, the all-project feed (paged by a
//! message cursor) and search, as a list beside a reader, with receipts on every message.
//! Opening a message addressed to the human marks it read.

use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;

use super::SessionCtx;
use super::task_page::{MessageView, message_view};
use super::views::{
    Labels, ProjectOption, Pulse, UiError, UiResult, html, project_options, pulse_counts,
};
use crate::domain::{Actor, Address, Message, MessageId};
use crate::http::AppState;
use crate::service::messages::InboxFilter;
use crate::service::tasks::Caller;

/// Messages per page of the all-project feed.
pub const FEED_PER_PAGE: u32 = 50;
/// Most unarchived messages the human inbox lists at once (oldest first, like agents').
pub const INBOX_LIMIT: u32 = 100;
/// Most search results shown.
pub const SEARCH_LIMIT: u32 = 50;
/// Most agents offered as compose recipients (newest first).
pub const COMPOSE_AGENTS: u32 = 200;

#[derive(Deserialize, Default)]
pub struct InboxQuery {
    /// `inbox` (default) or `all`.
    #[serde(default)]
    pub view: Option<String>,
    /// Search words; when set, the list shows search results.
    #[serde(default)]
    pub q: Option<String>,
    /// The message open in the reader.
    #[serde(default)]
    pub m: Option<String>,
    /// Feed cursor: messages older than this one.
    #[serde(default)]
    pub before: Option<String>,
}

/// Where a reply to the open message goes.
pub struct ReplyTarget {
    pub to_kind: &'static str,
    pub to: String,
    pub label: String,
    pub subject: String,
}

pub struct AgentOption {
    pub id: String,
    pub label: String,
}

#[derive(Template)]
#[template(path = "inbox.html")]
struct InboxPage {
    csrf: Option<String>,
    logout: bool,
    /// The top bar's counts (`None` when signed out).
    pulse: Option<Pulse>,
    nav: &'static str,
    /// `inbox`, `all` or `search`.
    view: &'static str,
    q: String,
    /// This list's own URL (live re-fetch), without the cursor.
    list_href: String,
    items: Vec<MessageView>,
    /// The human inbox hit `INBOX_LIMIT`.
    truncated: bool,
    older: Option<String>,
    selected: Option<MessageView>,
    /// The open message is in the human inbox (so it can be archived).
    archivable: bool,
    reply: Option<ReplyTarget>,
    projects: Vec<ProjectOption>,
    agents: Vec<AgentOption>,
}

fn parse_id(raw: Option<&str>, what: &str) -> UiResult<Option<MessageId>> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s
            .parse()
            .map(Some)
            .map_err(|_| UiError::Invalid(format!("Bad {what}."))),
        None => Ok(None),
    }
}

fn internal(e: impl std::fmt::Display) -> UiError {
    UiError::Internal(e.to_string())
}

pub async fn inbox(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Query(q): Query<InboxQuery>,
) -> UiResult<Response> {
    let now = state.clock.now();
    let store = &*state.store;
    let search =
        q.q.as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
    let view = if !search.is_empty() {
        "search"
    } else if q.view.as_deref() == Some("all") {
        "all"
    } else {
        "inbox"
    };
    let selected_id = parse_id(q.m.as_deref(), "message id")?;
    let before = parse_id(q.before.as_deref(), "cursor")?;

    // Open first, so the list below already shows it read.
    let selected_msg = match selected_id {
        Some(id) => Some(
            store
                .get_message(id)
                .await?
                .ok_or(UiError::NotFound("No such message."))?,
        ),
        None => None,
    };
    if let Some(m) = &selected_msg
        && m.to == Address::Human
    {
        state
            .messages
            .mark_read(Caller::Human, &[m.id])
            .await
            .map_err(internal)?;
    }

    let mut labels = Labels::load(store).await?;
    let base = |extra: &str| -> String {
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        match view {
            "search" => pairs.push(("q", &search)),
            "all" => pairs.push(("view", "all")),
            _ => {}
        }
        let mut href = String::from("/ui/inbox");
        let query = serde_urlencoded::to_string(&pairs).unwrap_or_default();
        let joined = [query.as_str(), extra]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("&");
        if !joined.is_empty() {
            href.push('?');
            href.push_str(&joined);
        }
        href
    };

    let mut items = Vec::new();
    let mut truncated = false;
    let mut older = None;
    match view {
        "inbox" => {
            let mut got = state
                .messages
                .human_inbox(InboxFilter {
                    limit: Some(INBOX_LIMIT),
                    ..InboxFilter::default()
                })
                .await
                .map_err(internal)?;
            truncated = got.len() == INBOX_LIMIT as usize;
            got.reverse();
            for item in got {
                let mut v = message_view(&mut labels, now, &item.message).await?;
                v.unread = item.read_at.is_none();
                items.push(v);
            }
        }
        "all" => {
            let mut page = store.list_messages(None, before, FEED_PER_PAGE + 1).await?;
            if page.len() > FEED_PER_PAGE as usize {
                page.truncate(FEED_PER_PAGE as usize);
                older = page.last().map(|m| base(&format!("before={}", m.id)));
            }
            for m in &page {
                items.push(message_view(&mut labels, now, m).await?);
            }
        }
        _ => {
            for m in state
                .messages
                .search(&search, None, SEARCH_LIMIT)
                .await
                .map_err(internal)?
            {
                items.push(message_view(&mut labels, now, &m).await?);
            }
        }
    }
    let ids: Vec<MessageId> = items.iter().filter_map(|v| v.id.parse().ok()).collect();
    let receipts = store.message_receipts(&ids).await?;
    let cursor = before.map(|b| format!("before={b}")).unwrap_or_default();
    for (v, r) in items.iter_mut().zip(receipts) {
        v.receipt = Some(r.state());
        let open = format!("m={}", v.id);
        v.href = base(
            &[cursor.as_str(), open.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("&"),
        );
    }

    let (selected, reply, archivable) = match &selected_msg {
        Some(m) => {
            let mut v = message_view(&mut labels, now, m).await?;
            v.receipt = store
                .message_receipts(&[m.id])
                .await?
                .first()
                .map(|r| r.state());
            let reply = reply_target(m).map(|mut r| {
                r.label = match (m.task_id, &v.task) {
                    (Some(_), Some((_, title))) => format!("thread: {title}"),
                    (Some(_), None) => "the thread".into(),
                    (None, _) => v.from.clone(),
                };
                r
            });
            (Some(v), reply, m.to == Address::Human)
        }
        None => (None, None, false),
    };

    // Compose targets: projects and the newest `COMPOSE_AGENTS` agents.
    let mut agents: Vec<AgentOption> = Vec::new();
    for agent in store.list_agents(COMPOSE_AGENTS).await? {
        let project = agent
            .project_id
            .map(|p| format!(" · {}", labels.project(p).name))
            .unwrap_or_default();
        agents.push(AgentOption {
            id: agent.id.to_string(),
            label: format!("{}@{}{project}", agent.harness, agent.host),
        });
    }
    agents.sort_by(|a, b| a.label.cmp(&b.label));

    html(&InboxPage {
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(&state).await?),
        nav: "inbox",
        view,
        list_href: base(&selected_id.map(|m| format!("m={m}")).unwrap_or_default()),
        q: search.clone(),
        items,
        truncated,
        older,
        selected,
        archivable,
        reply,
        projects: project_options(&labels, None),
        agents,
    })
}

/// Where the human's reply to `m` goes: its task thread, else its sending agent (with an
/// empty `label` for the caller to fill). `None` for the human's own messages.
pub(super) fn reply_target(m: &Message) -> Option<ReplyTarget> {
    match (m.task_id, m.from) {
        (Some(t), _) => Some(ReplyTarget {
            to_kind: "task",
            to: t.to_string(),
            label: String::new(),
            subject: String::new(),
        }),
        (None, Actor::Agent(agent, _)) => Some(ReplyTarget {
            to_kind: "agent",
            to: agent.to_string(),
            label: String::new(),
            subject: reply_subject(m.subject.as_deref()),
        }),
        // The human's own message: no one to reply to.
        (None, Actor::Human) => None,
    }
}

use crate::service::messages::reply_subject;

#[cfg(test)]
mod tests {
    use super::reply_subject;

    #[test]
    fn reply_subjects_are_not_doubled() {
        assert_eq!(reply_subject(Some("Hi")), "Re: Hi");
        assert_eq!(reply_subject(Some("re: Hi")), "re: Hi");
        assert_eq!(reply_subject(None), "");
    }
}
