//! Publishing task changes and telling a parent's thread about its children, shared by the
//! task and delivery services.

use std::sync::Arc;

use crate::clock::Clock;
use crate::domain::{
    Actor, Address, Importance, Message, MessageId, MessageKind, Task, TaskEventKind, TaskStatus,
    TokenId,
};
use crate::events::{Event, EventBus};
use crate::store::Store;

/// Publishes events and posts the automatic parent-thread messages. Every failure is logged:
/// the change being announced has already committed.
#[derive(Clone)]
pub(crate) struct Notifier {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    events: Option<EventBus>,
}

impl Notifier {
    pub(crate) fn new(
        store: Arc<dyn Store>,
        clock: Arc<dyn Clock>,
        events: Option<EventBus>,
    ) -> Self {
        Self {
            store,
            clock,
            events,
        }
    }

    pub(crate) async fn changed(&self, task: &Task, kind: TaskEventKind) {
        if let Some(bus) = &self.events {
            bus.emit(Event::TaskChanged {
                id: task.id,
                project_id: task.project_id,
                status: task.status,
                kind,
            })
            .await;
        }
    }

    pub(crate) async fn message_created(&self, message: &Message) {
        if let Some(bus) = &self.events {
            bus.emit(Event::MessageCreated {
                id: message.id,
                to: message.to,
                project_id: message.project_id,
            })
            .await;
        }
    }

    /// After a committed change to `task` (`kind` is the event the store recorded): publishes
    /// it and, for a child, tells the coordinator (spec section 8). A child that is blocked,
    /// done (also through delivery) or cancelled posts a message into the parent's thread
    /// (delivered to the parent's owner and creator), as does the closing of the last open
    /// child (`children_complete`, written by the store). A parent the store moved in the
    /// same change (to `review`, `delivering` or `done`) is published too.
    pub(crate) async fn announce(
        &self,
        task: &Task,
        kind: TaskEventKind,
        note: Option<&str>,
        actor: Actor,
        token: Option<TokenId>,
        children_complete: bool,
    ) {
        self.changed(task, kind).await;
        let Some(parent_id) = task.parent_id else {
            return;
        };
        if !matches!(
            kind,
            TaskEventKind::Blocked
                | TaskEventKind::Done
                | TaskEventKind::Delivered
                | TaskEventKind::Cancelled
        ) {
            return;
        }
        let parent = match self.store.get_task(parent_id).await {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                eprintln!("axl: failed to read parent task {parent_id}: {e}");
                return;
            }
        };
        let status = match kind {
            TaskEventKind::Blocked => "blocked",
            TaskEventKind::Done | TaskEventKind::Delivered => {
                if task.status == TaskStatus::Delivering {
                    "delivering"
                } else {
                    "done"
                }
            }
            _ => "cancelled",
        };
        let detail = if kind == TaskEventKind::Cancelled {
            None
        } else {
            note
        };
        let body = match detail.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => format!(
                "Child task \"{}\" ({}) is {status}: {d}",
                task.title, task.id
            ),
            None => format!("Child task \"{}\" ({}) is {status}.", task.title, task.id),
        };
        let importance = if kind == TaskEventKind::Blocked {
            Importance::High
        } else {
            Importance::Normal
        };
        self.notify_parent(
            &parent,
            actor,
            token,
            &format!("child task {status}"),
            body,
            importance,
        )
        .await;
        // A parent the store moved in this change carries the child's `updated_at`.
        if parent.updated_at >= task.updated_at {
            match parent.status {
                TaskStatus::Review if children_complete => {
                    self.changed(&parent, TaskEventKind::ChildrenComplete).await;
                }
                TaskStatus::Delivering => {
                    self.changed(&parent, TaskEventKind::StageReached).await;
                }
                TaskStatus::Done => {
                    self.changed(&parent, TaskEventKind::Delivered).await;
                }
                _ => {}
            }
        }
        if !children_complete {
            return;
        }
        let body = format!(
            "All child tasks are done or cancelled; the parent task is {}.",
            parent.status
        );
        self.notify_parent(
            &parent,
            actor,
            token,
            "child tasks complete",
            body,
            Importance::High,
        )
        .await;
    }

    /// Posts an automatic message into the parent task's thread.
    async fn notify_parent(
        &self,
        parent: &Task,
        from: Actor,
        token: Option<TokenId>,
        subject: &str,
        body: String,
        importance: Importance,
    ) {
        let message = Message {
            id: MessageId::new(),
            project_id: Some(parent.project_id),
            from,
            to: Address::Task(parent.id),
            subject: Some(subject.to_string()),
            body,
            importance,
            task_id: Some(parent.id),
            reply_to: None,
            created_at: self.clock.now(),
            kind: MessageKind::System,
            options: Vec::new(),
        };
        if let Err(e) = self.store.insert_message(&message, token).await {
            eprintln!("axl: failed to notify parent task {}: {e}", parent.id);
            return;
        }
        self.message_created(&message).await;
    }
}
