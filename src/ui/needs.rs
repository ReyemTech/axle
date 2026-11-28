//! "Needs you": everything waiting on the human, as one strip of attention cards at the top of
//! the board and the overview. Its length is the top bar's "N need you" count.
//!
//! A card is one of:
//! - a plan awaiting approval (a `planning` task whose plan is `submitted`), with Approve and
//!   Request changes inline;
//! - a task blocked waiting on the human ("Blocked on you"), with what would unblock it and a
//!   reply box that posts into the task's thread;
//! - a task in `review` (awaiting the coordinator or the human), whoever created it;
//! - an unarchived human-due message of high or urgent importance (plans excluded: they have
//!   their own card), read with a peek that marks nothing delivered.

use askama::Template;
use axum::Extension;
use axum::extract::State;
use axum::response::Response;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::SessionCtx;
use super::inbox::{ReplyTarget, reply_target};
use super::views::{Labels, ProjectLabel, UiResult, html};
use crate::domain::{Actor, Message, MessageKind, PlanState, Task, TaskId, TaskStatus, WaitingOn};
use crate::http::AppState;
use crate::store::{StoreResult, TaskFilter};

/// Most messages of each importance the strip looks at.
const MESSAGE_LIMIT: u32 = 100;
/// Characters of a plan or message shown on its card.
const PREVIEW: usize = 280;

/// What the human is asked to do, in words; also the card's `data-need`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    ApprovePlan,
    BlockedOnYou,
    Review,
    Reply,
}

impl Need {
    pub fn key(&self) -> &'static str {
        match self {
            Need::ApprovePlan => "plan",
            Need::BlockedOnYou => "blocked",
            Need::Review => "review",
            Need::Reply => "reply",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Need::ApprovePlan => "Approve plan",
            Need::BlockedOnYou => "Blocked on you",
            Need::Review => "Review",
            Need::Reply => "Reply",
        }
    }
}

pub struct NeedCard {
    pub need: Need,
    /// The signal aspect drawn beside the label.
    pub aspect: &'static str,
    /// The thing it is about: the task's title, else the message's subject.
    pub title: String,
    /// Where the title links.
    pub href: String,
    pub project: Option<ProjectLabel>,
    /// The agent that is waiting (plan author, reviewer-to-be, sender).
    pub agent: Option<String>,
    /// For a message in a thread, the task it belongs to.
    pub task_title: Option<String>,
    pub preview: Option<String>,
    /// For plan cards: the plan rendered from markdown (safe HTML), shown clipped.
    pub plan_html: Option<String>,
    /// How long it has waited, e.g. `12m`.
    pub waited: String,
    pub since: String,
    /// For plan cards: the task to decide on and the plan shown (sent with the decision).
    pub task_id: Option<String>,
    pub plan_id: Option<String>,
    /// For reply cards: the message, which can be dismissed.
    pub message_id: Option<String>,
    pub importance: &'static str,
    /// For reply cards whose sender offered quick replies: the answers (peer text) and where
    /// a reply goes.
    pub options: Vec<String>,
    pub reply: Option<ReplyTarget>,
    /// For "Blocked on you" cards: the task whose thread the reply box posts to.
    pub reply_task: Option<String>,
}

/// "42s", "12m", "3h", "2d".
pub fn waited(now: OffsetDateTime, since: OffsetDateTime) -> String {
    let secs = (now - since).whole_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(PREVIEW) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// The latest `plan` message in a task's thread.
pub async fn latest_plan(state: &AppState, task: TaskId) -> StoreResult<Option<Message>> {
    Ok(state
        .store
        .task_thread(task)
        .await?
        .into_iter()
        .rev()
        .find(|m| m.kind == MessageKind::Plan))
}

/// Every card, plans first, then tasks blocked on the human, then reviews, then messages;
/// each group oldest first.
pub(super) async fn needs_you(
    state: &AppState,
    labels: &mut Labels<'_>,
) -> StoreResult<Vec<NeedCard>> {
    let now = state.clock.now();
    let since = |t: OffsetDateTime| t.format(&Rfc3339).unwrap_or_default();
    let mut cards = Vec::new();

    let planning: Vec<Task> = state
        .store
        .list_tasks(&TaskFilter {
            status: Some(TaskStatus::Planning),
            ..TaskFilter::default()
        })
        .await?
        .into_iter()
        .filter(|t| t.plan_state == PlanState::Submitted)
        .collect();
    let mut plans = Vec::with_capacity(planning.len());
    for t in planning {
        let plan = latest_plan(state, t.id).await?;
        let at = plan.as_ref().map_or(t.updated_at, |p| p.created_at);
        if plan.is_none() {
            continue;
        }
        plans.push((t, plan, at));
    }
    plans.sort_by_key(|(_, _, at)| *at);
    for (t, plan, at) in plans {
        let agent = match t.owner_agent_id {
            Some(a) => Some(labels.agent_label(a).await?),
            None => None,
        };
        cards.push(NeedCard {
            need: Need::ApprovePlan,
            aspect: "plan-submitted",
            title: t.title.clone(),
            href: format!("/ui/tasks/{}#plan", t.id),
            project: Some(labels.project(t.project_id)),
            agent,
            task_title: None,
            preview: None,
            plan_html: plan
                .as_ref()
                .map(|p| super::markdown::render(&p.body, None)),
            waited: waited(now, at),
            since: since(at),
            task_id: Some(t.id.to_string()),
            plan_id: plan.map(|p| p.id.to_string()),
            message_id: None,
            importance: "high",
            options: Vec::new(),
            reply: None,
            reply_task: None,
        });
    }

    cards.extend(blocked_on_you(state, labels, now).await?);

    let mut reviews: Vec<Task> = state
        .store
        .list_tasks(&TaskFilter {
            status: Some(TaskStatus::Review),
            ..TaskFilter::default()
        })
        .await?;
    reviews.sort_by_key(|t| t.updated_at);
    for t in reviews {
        let agent = match t.owner_agent_id {
            Some(a) => Some(labels.agent_label(a).await?),
            None => None,
        };
        cards.push(NeedCard {
            need: Need::Review,
            aspect: "review",
            title: t.title.clone(),
            href: format!("/ui/tasks/{}", t.id),
            project: Some(labels.project(t.project_id)),
            agent,
            task_title: None,
            preview: None,
            plan_html: None,
            waited: waited(now, t.updated_at),
            since: since(t.updated_at),
            task_id: None,
            plan_id: None,
            message_id: None,
            importance: "high",
            options: Vec::new(),
            reply: None,
            reply_task: None,
        });
    }

    // A read-only peek: showing the strip must not mark anything delivered.
    let mut messages = state.store.human_attention(MESSAGE_LIMIT).await?;
    messages.sort_by_key(|m| m.created_at);
    for m in messages {
        let agent = match m.from {
            Actor::Agent(a, _) => Some(labels.agent_label(a).await?),
            Actor::Human => None,
        };
        let task_title = match m.task_id {
            Some(t) => labels.task_title(t).await?,
            None => None,
        };
        let title = m
            .subject
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| clip(&m.body).chars().take(80).collect());
        cards.push(NeedCard {
            need: Need::Reply,
            aspect: m.importance.as_str(),
            title,
            href: format!("/ui/inbox?m={}", m.id),
            project: m.project_id.map(|p| labels.project(p)),
            agent,
            task_title,
            preview: Some(clip(&m.body)),
            plan_html: None,
            waited: waited(now, m.created_at),
            since: since(m.created_at),
            task_id: None,
            plan_id: None,
            message_id: Some(m.id.to_string()),
            importance: m.importance.as_str(),
            reply: if m.options.is_empty() {
                None
            } else {
                reply_target(&m)
            },
            options: m.options,
            reply_task: None,
        });
    }
    Ok(cards)
}

/// "Blocked on you" cards: blocked tasks waiting on the human, longest-blocked first. The
/// preview is what would unblock the task (else why it is blocked); the card's reply box posts
/// into the task's thread.
async fn blocked_on_you(
    state: &AppState,
    labels: &mut Labels<'_>,
    now: OffsetDateTime,
) -> StoreResult<Vec<NeedCard>> {
    let mut blocked: Vec<(Task, OffsetDateTime)> = state
        .store
        .list_tasks(&TaskFilter {
            status: Some(TaskStatus::Blocked),
            ..TaskFilter::default()
        })
        .await?
        .into_iter()
        .filter_map(|t| {
            let b = t.block.as_ref()?;
            if b.waiting_on != Some(WaitingOn::Human) {
                return None;
            }
            let at = b.since.unwrap_or(t.updated_at);
            Some((t, at))
        })
        .collect();
    blocked.sort_by_key(|(_, at)| *at);
    let mut cards = Vec::with_capacity(blocked.len());
    for (t, at) in blocked {
        let agent = match t.owner_agent_id {
            Some(a) => Some(labels.agent_label(a).await?),
            None => None,
        };
        let block = t.block.as_ref().expect("filtered on its block");
        let preview = match &block.unblock {
            Some(u) => format!("Needs: {}", clip(u)),
            None => clip(&block.reason),
        };
        cards.push(NeedCard {
            need: Need::BlockedOnYou,
            aspect: "blocked",
            title: t.title.clone(),
            href: format!("/ui/tasks/{}#blocked", t.id),
            project: Some(labels.project(t.project_id)),
            agent,
            task_title: None,
            preview: Some(preview),
            plan_html: None,
            waited: waited(now, at),
            since: at.format(&Rfc3339).unwrap_or_default(),
            task_id: None,
            plan_id: None,
            message_id: None,
            importance: "high",
            options: Vec::new(),
            reply: None,
            reply_task: Some(t.id.to_string()),
        });
    }
    Ok(cards)
}

#[derive(Template)]
#[template(path = "_needs_you.html")]
pub(super) struct NeedsFragment {
    pub(super) csrf: Option<String>,
    pub(super) needs: Vec<NeedCard>,
    /// Where the decision forms send the human back to.
    pub(super) back: &'static str,
}

#[derive(serde::Deserialize, Default)]
pub struct NeedsQuery {
    #[serde(default)]
    pub back: Option<String>,
}

/// The strip on its own, for the live re-fetch.
pub async fn needs_fragment(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    axum::extract::Query(q): axum::extract::Query<NeedsQuery>,
) -> UiResult<Response> {
    let mut labels = Labels::load(&*state.store).await?;
    let needs = needs_you(&state, &mut labels).await?;
    html(&NeedsFragment {
        csrf: Some(session.csrf),
        needs,
        back: if q.back.as_deref() == Some("/ui/") {
            "/ui/"
        } else {
            "/ui/board"
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;
    use time::macros::datetime;

    #[test]
    fn waits_read_in_the_largest_unit() {
        let now = datetime!(2026-11-27 9:00 UTC);
        assert_eq!(waited(now, now - Duration::seconds(5)), "5s");
        assert_eq!(waited(now, now - Duration::minutes(12)), "12m");
        assert_eq!(waited(now, now - Duration::hours(3)), "3h");
        assert_eq!(waited(now, now - Duration::days(2)), "2d");
    }
}
