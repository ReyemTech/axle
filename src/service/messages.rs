//! Durable messages: addressing, per-recipient delivery and ack, task threads, search
//! (spec §9).
//!
//! Delivery rules:
//! - agent-addressed: one recipient row per agent, shown to all its sessions, acked once;
//! - session-addressed: that session only, acked per session;
//! - project-addressed: every agent whose home project it is, resolved when an inbox is read
//!   from a per-agent cursor that starts at the agent's creation;
//! - task-addressed: the task's current owner agent and its creator (agent or human),
//!   resolved when an inbox is read;
//! - human-addressed: the human inbox.
//!
//! Project-, agent- and task-addressed mail is never delivered back to the agent that sent it.

use std::sync::Arc;

use crate::clock::Clock;
use crate::domain::{
    Actor, Address, Importance, InboxItem, Message, MessageId, ProjectId, TaskId, TokenId,
    WakeTrigger,
};
use crate::events::{Event, EventBus};
use crate::service::identity::Resolved;
use crate::service::tasks::Caller;
use crate::service::wake::WakeService;
use crate::store::{Recipient, Store, StoreError};

pub use crate::store::InboxFilter;

/// Largest accepted message (and task) body, in bytes.
pub const BODY_LIMIT: usize = 65_536;
/// Longest accepted message subject (and task title), in characters.
pub const SUBJECT_MAX: usize = 200;
/// Fewest and most quick replies a message to the human may offer.
pub const OPTIONS_MIN: usize = 2;
pub const OPTIONS_MAX: usize = 4;
/// Longest quick reply, in characters (after trimming).
pub const OPTION_MAX: usize = 80;

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub to: Address,
    pub subject: Option<String>,
    pub body: String,
    pub importance: Importance,
    pub reply_to: Option<MessageId>,
}

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("body exceeds {limit} bytes")]
    BodyTooLarge { limit: usize },
    #[error("{field} is longer than {max} characters")]
    TooLong { field: &'static str, max: usize },
    /// The sender, recipient or replied-to message does not exist.
    #[error("{0} not found")]
    NotFound(&'static str),
    /// A reply to a message for the human goes to its sender or its thread.
    #[error("a reply goes to the message's sender or its task thread")]
    MismatchedReply,
    /// Quick replies break a rule (count, length, line break, duplicate).
    #[error("options: {0}")]
    BadOptions(String),
    /// Quick replies are buttons in the human's UI; no other recipient can press them.
    #[error("options are only for messages to the human (to: {{\"human\": true}})")]
    OptionsNeedHuman,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Rejects a subject or title over `SUBJECT_MAX` characters.
/// `Re: <subject>` (not doubled), cut to `SUBJECT_MAX` characters; empty without a subject.
pub fn reply_subject(subject: Option<&str>) -> String {
    match subject.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) if s.to_ascii_lowercase().starts_with("re:") => s.to_string(),
        Some(s) => format!("Re: {s}").chars().take(SUBJECT_MAX).collect(),
        None => String::new(),
    }
}

pub fn check_subject(field: &'static str, text: &str) -> Result<(), MessageError> {
    if text.chars().count() > SUBJECT_MAX {
        return Err(MessageError::TooLong {
            field,
            max: SUBJECT_MAX,
        });
    }
    Ok(())
}

/// Quick replies, trimmed: `OPTIONS_MIN..=OPTIONS_MAX` of them, each 1..=`OPTION_MAX`
/// characters on one line, no two the same (ignoring case). None at all is fine.
pub fn check_options(options: &[String]) -> Result<Vec<String>, MessageError> {
    if options.is_empty() {
        return Ok(Vec::new());
    }
    let bad = |why: String| Err(MessageError::BadOptions(why));
    if !(OPTIONS_MIN..=OPTIONS_MAX).contains(&options.len()) {
        return bad(format!(
            "give {OPTIONS_MIN} to {OPTIONS_MAX} answers, not {}",
            options.len()
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(options.len());
    for raw in options {
        let option = raw.trim();
        if option.is_empty() {
            return bad("an answer is empty".into());
        }
        if option.chars().count() > OPTION_MAX {
            return bad(format!("an answer is longer than {OPTION_MAX} characters"));
        }
        if option.contains(['\n', '\r']) {
            return bad("an answer contains a line break; keep each to one line".into());
        }
        if out
            .iter()
            .any(|o| o.to_lowercase() == option.to_lowercase())
        {
            return bad(format!("{option:?} is offered twice"));
        }
        out.push(option.to_string());
    }
    Ok(out)
}

/// Rejects bodies over `BODY_LIMIT` bytes.
pub fn check_body(body: &str) -> Result<(), MessageError> {
    if body.len() > BODY_LIMIT {
        return Err(MessageError::BodyTooLarge { limit: BODY_LIMIT });
    }
    Ok(())
}

pub struct MessageService {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    events: Option<EventBus>,
    /// Wakes owners and agents after human posts and replies and peer task posts; `None`
    /// wakes nobody.
    wake: Option<Arc<WakeService>>,
}

impl MessageService {
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            clock,
            events: None,
            wake: None,
        }
    }

    /// Wakes sessions through `wake` after the sends that call for it (spec "Trigger
    /// rules").
    pub fn with_wake(mut self, wake: Arc<WakeService>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Publishes `MessageCreated` on `bus` after every send.
    pub fn with_events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    pub async fn send(&self, caller: &Actor, new: NewMessage) -> Result<Message, MessageError> {
        self.send_inner(caller, new, Vec::new(), false).await
    }

    /// `send`, offering the human quick replies (`check_options`). Only a message to the
    /// human may carry them; an empty list is a plain `send`.
    pub async fn send_with_options(
        &self,
        caller: &Actor,
        new: NewMessage,
        options: &[String],
    ) -> Result<Message, MessageError> {
        let options = check_options(options)?;
        if !options.is_empty() && new.to != Address::Human {
            return Err(MessageError::OptionsNeedHuman);
        }
        self.send_inner(caller, new, options, false).await
    }

    /// The human sends a message. When it replies to a message addressed to the human, it
    /// must go to that message's sending agent or its task thread, and the human's copy of
    /// the original is archived in the same transaction (answered mail stops waiting).
    ///
    /// A reply's subject is always derived here from the original's (`Re: <subject>`); any
    /// subject the caller supplied is ignored, so a posted form field cannot set it.
    pub async fn send_as_human(&self, mut new: NewMessage) -> Result<Message, MessageError> {
        let answers_human = match new.reply_to {
            Some(id) => {
                let original = self
                    .store
                    .get_message(id)
                    .await?
                    .ok_or(MessageError::NotFound("message"))?;
                new.subject =
                    Some(reply_subject(original.subject.as_deref())).filter(|s| !s.is_empty());
                if original.to == Address::Human {
                    let to_sender = matches!(
                        (original.from, new.to),
                        (Actor::Agent(a, _), Address::Agent(b)) if a == b
                    );
                    let to_thread = original.task_id.is_some_and(|t| new.to == Address::Task(t));
                    if !to_sender && !to_thread {
                        return Err(MessageError::MismatchedReply);
                    }
                    true
                } else {
                    false
                }
            }
            None => false,
        };
        let message = self
            .send_inner(&Actor::Human, new, Vec::new(), answers_human)
            .await?;
        if answers_human {
            self.archived(message.reply_to.into_iter().collect()).await;
        }
        Ok(message)
    }

    async fn archived(&self, ids: Vec<MessageId>) {
        if let Some(bus) = &self.events
            && !ids.is_empty()
        {
            bus.emit(Event::HumanArchived { archived: ids }).await;
        }
    }

    async fn send_inner(
        &self,
        caller: &Actor,
        new: NewMessage,
        options: Vec<String>,
        archive_original: bool,
    ) -> Result<Message, MessageError> {
        check_body(&new.body)?;
        if let Some(subject) = &new.subject {
            check_subject("subject", subject)?;
        }
        let (sender_project, token) = self.sender(caller).await?;
        if let Some(reply_to) = new.reply_to
            && self.store.get_message(reply_to).await?.is_none()
        {
            return Err(MessageError::NotFound("message"));
        }
        let (project_id, task_id) = self.place(&new.to, sender_project).await?;
        let message = Message {
            id: MessageId::new(),
            project_id,
            from: *caller,
            to: new.to,
            subject: new.subject,
            body: new.body,
            importance: new.importance,
            task_id,
            reply_to: new.reply_to,
            created_at: self.clock.now(),
            kind: crate::domain::MessageKind::Message,
            options,
        };
        if archive_original {
            self.store
                .insert_human_reply(&message, self.clock.now())
                .await?;
        } else {
            self.store.insert_message(&message, token).await?;
        }
        if let Some(bus) = &self.events {
            bus.emit(Event::MessageCreated {
                id: message.id,
                to: message.to,
                project_id: message.project_id,
            })
            .await;
        }
        self.wake_for(&message).await;
        Ok(message)
    }

    /// After a committed send, wakes whoever the trigger table says it should (spec
    /// "Trigger rules"). Runs after the commit: a failure is logged, never returned.
    async fn wake_for(&self, message: &Message) {
        let Some(wake) = &self.wake else {
            return;
        };
        if let Err(e) = self.wake_with(wake, message).await {
            eprintln!("axl: failed to wake for message {}: {e}", message.id);
        }
    }

    /// - The human posts in a task thread: the owner session (`HumanPost`).
    /// - The human replies, outside a thread, to an agent's message, addressed to that agent
    ///   or the session that wrote it: one live idle session of the agent (`HumanReply`).
    /// - An agent posts in a task another agent owns: the owner, within the peer guards
    ///   (`WakeService::peer_post`).
    ///
    /// Any other message (an agent's `send_message`, project mail) wakes nobody.
    async fn wake_with(&self, wake: &WakeService, message: &Message) -> Result<(), StoreError> {
        match (message.from, message.to) {
            (Actor::Human, Address::Task(t)) => {
                if let Some(task) = self.store.get_task(t).await? {
                    wake.wake_owner(&task, WakeTrigger::HumanPost, None).await?;
                }
            }
            (Actor::Human, to) => {
                let Some(id) = message.reply_to else {
                    return Ok(());
                };
                let Some(original) = self.store.get_message(id).await? else {
                    return Ok(());
                };
                if let Actor::Agent(agent, session) = original.from
                    && (to == Address::Agent(agent) || to == Address::Session(session))
                {
                    wake.wake_agent(agent, WakeTrigger::HumanReply).await?;
                }
            }
            (Actor::Agent(agent, session), Address::Task(t)) => {
                if let Some(task) = self.store.get_task(t).await? {
                    wake.peer_post(&task, agent, session).await?;
                }
            }
            (Actor::Agent(..), _) => {}
        }
        Ok(())
    }

    /// The caller's unacked messages, oldest first; marks the returned ones delivered.
    /// Unacked messages come back on every call (at-least-once).
    pub async fn inbox(
        &self,
        caller: &Resolved,
        filter: InboxFilter,
    ) -> Result<Vec<InboxItem>, MessageError> {
        Ok(self
            .store
            .inbox(recipient(caller), &filter, self.clock.now())
            .await?)
    }

    /// The human's unacked messages, oldest first; marks the returned ones delivered.
    pub async fn human_inbox(&self, filter: InboxFilter) -> Result<Vec<InboxItem>, MessageError> {
        Ok(self
            .store
            .inbox(Recipient::Human, &filter, self.clock.now())
            .await?)
    }

    /// Acknowledges messages in the caller's inbox; returns how many were newly acked.
    /// Agent-addressed (and project / task) messages are acked for the whole agent.
    /// The human's acks publish `HumanArchived`, so open pages drop the mail live.
    pub async fn ack(&self, caller: Caller<'_>, ids: &[MessageId]) -> Result<u64, MessageError> {
        let n = self
            .store
            .ack_messages(caller_recipient(caller), ids, self.clock.now())
            .await?;
        if n > 0 && matches!(caller, Caller::Human) {
            self.archived(ids.to_vec()).await;
        }
        Ok(n)
    }

    /// Records that the caller read these messages; returns how many were newly marked.
    pub async fn mark_read(
        &self,
        caller: Caller<'_>,
        ids: &[MessageId],
    ) -> Result<u64, MessageError> {
        Ok(self
            .store
            .mark_read(caller_recipient(caller), ids, self.clock.now())
            .await?)
    }

    /// Unacked items in the session's inbox (what `inbox` with no filter would return), for
    /// `whoami`, heartbeats and hooks. Writes nothing.
    pub async fn unread_count(&self, caller: &Resolved) -> Result<u32, MessageError> {
        let n = self.store.unread_count(recipient(caller), None).await?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Unacked items in the human inbox (`importance`: only that importance). Writes nothing.
    pub async fn human_unread_count(
        &self,
        importance: Option<Importance>,
    ) -> Result<u32, MessageError> {
        let n = self
            .store
            .unread_count(Recipient::Human, importance)
            .await?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Messages whose subject or body contain every word of `query`, newest first. Search
    /// covers all history, including messages that predate the caller.
    pub async fn search(
        &self,
        query: &str,
        project: Option<ProjectId>,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(vec![]);
        }
        Ok(self
            .store
            .search_messages(query.trim(), project, limit)
            .await?)
    }

    /// A task's thread, oldest first (starting with the message `task_create` wrote).
    pub async fn thread(&self, task: TaskId) -> Result<Vec<Message>, MessageError> {
        Ok(self.store.task_thread(task).await?)
    }

    // ---------------------------------------------------------------- internals

    /// The sender's home project and token, checking the session belongs to the agent.
    async fn sender(
        &self,
        caller: &Actor,
    ) -> Result<(Option<ProjectId>, Option<TokenId>), MessageError> {
        let Actor::Agent(agent, session) = *caller else {
            return Ok((None, None));
        };
        let session = self
            .store
            .get_session(session)
            .await?
            .filter(|s| s.agent_id == agent)
            .ok_or(MessageError::NotFound("session"))?;
        let agent = self
            .store
            .get_agent(agent)
            .await?
            .ok_or(MessageError::NotFound("agent"))?;
        Ok((agent.project_id, Some(session.token_id)))
    }

    /// The message's `(project_id, task_id)` for an address, checking the target exists.
    async fn place(
        &self,
        to: &Address,
        sender_project: Option<ProjectId>,
    ) -> Result<(Option<ProjectId>, Option<TaskId>), MessageError> {
        Ok(match *to {
            Address::Project(p) => {
                self.store
                    .get_project(p)
                    .await?
                    .ok_or(MessageError::NotFound("project"))?;
                (Some(p), None)
            }
            Address::Agent(a) => {
                let agent = self
                    .store
                    .get_agent(a)
                    .await?
                    .ok_or(MessageError::NotFound("agent"))?;
                (agent.project_id, None)
            }
            Address::Session(s) => {
                let session = self
                    .store
                    .get_session(s)
                    .await?
                    .ok_or(MessageError::NotFound("session"))?;
                let agent = self
                    .store
                    .get_agent(session.agent_id)
                    .await?
                    .ok_or(MessageError::NotFound("agent"))?;
                (agent.project_id, None)
            }
            Address::Task(t) => {
                let task = self
                    .store
                    .get_task(t)
                    .await?
                    .ok_or(MessageError::NotFound("task"))?;
                (Some(task.project_id), Some(t))
            }
            Address::Human => (sender_project, None),
        })
    }
}

fn recipient(r: &Resolved) -> Recipient {
    Recipient::Agent {
        agent: r.agent.id,
        session: r.session.id,
    }
}

fn caller_recipient(caller: Caller<'_>) -> Recipient {
    match caller {
        Caller::Agent(r) => recipient(r),
        Caller::Human => Recipient::Human,
    }
}
