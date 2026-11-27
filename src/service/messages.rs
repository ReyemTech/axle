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
};
use crate::events::{Event, EventBus};
use crate::service::identity::Resolved;
use crate::service::tasks::Caller;
use crate::store::{Recipient, Store, StoreError};

pub use crate::store::InboxFilter;

/// Largest accepted message (and task) body, in bytes.
pub const BODY_LIMIT: usize = 65_536;

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
    /// The sender, recipient or replied-to message does not exist.
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
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
}

impl MessageService {
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            clock,
            events: None,
        }
    }

    /// Publishes `MessageCreated` on `bus` after every send.
    pub fn with_events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    pub async fn send(&self, caller: &Actor, new: NewMessage) -> Result<Message, MessageError> {
        check_body(&new.body)?;
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
        };
        self.store.insert_message(&message, token).await?;
        if let Some(bus) = &self.events {
            bus.emit(Event::MessageCreated {
                id: message.id,
                to: message.to,
                project_id: message.project_id,
            })
            .await;
        }
        Ok(message)
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
    pub async fn ack(&self, caller: Caller<'_>, ids: &[MessageId]) -> Result<u64, MessageError> {
        Ok(self
            .store
            .ack_messages(caller_recipient(caller), ids, self.clock.now())
            .await?)
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
