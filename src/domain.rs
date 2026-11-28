//! Ids and structs shared by every layer.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

macro_rules! id_type {
    ($($(#[$meta:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// A new time-ordered (UUID v7) id.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    )+};
}

id_type!(
    ProjectId,
    AgentId,
    SessionId,
    TaskId,
    MessageId,
    TokenId,
    EnrollmentId
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    /// Normalised key, e.g. `github.com/acme/platform-infra`. Never changes.
    pub key: String,
    /// Human label shown in the UI (editable).
    pub display_name: String,
    pub created_at: OffsetDateTime,
}

/// Stable identity `(project, host, harness)`; `project_id` is `None` for agents with no
/// home project.
/// Longest project display name the UI accepts, in characters.
pub const DISPLAY_NAME_MAX: usize = 80;

/// A display name set by the human, made safe to show to models and people: trimmed,
/// 1–`DISPLAY_NAME_MAX` characters, with no control characters, no invisible formatting
/// characters (bidi overrides, zero-width marks) and no `[` / `]` (MCP output frames peer
/// content in brackets and prints display names unframed, so they must not fake a frame).
pub fn clean_display_name(raw: &str) -> Result<String, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("A display name must not be empty.");
    }
    if name.chars().count() > DISPLAY_NAME_MAX {
        return Err("A display name is at most 80 characters.");
    }
    let invisible = |c: char| matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}');
    if name.chars().any(|c| c.is_control() || invisible(c)) {
        return Err("A display name must not contain control or invisible characters.");
    }
    if name.contains(['[', ']']) {
        return Err("A display name must not contain [ or ].");
    }
    Ok(name.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: AgentId,
    pub project_id: Option<ProjectId>,
    pub host: String,
    pub harness: String,
    pub created_at: OffsetDateTime,
}

/// One live harness process of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    pub agent_id: AgentId,
    pub token_id: TokenId,
    pub started_at: OffsetDateTime,
    pub last_heartbeat_at: OffsetDateTime,
    pub last_tool_call_at: OffsetDateTime,
    /// What the harness last reported about its turn; `Unknown` until the first report.
    pub turn_state: TurnState,
    pub turn_state_at: Option<OffsetDateTime>,
    /// How Axle can reach this session when it is idle; `None` means it cannot.
    pub wake_channel: WakeChannel,
    /// The task a wake is reserved for, until `wake_until`.
    pub wake_task_id: Option<TaskId>,
    pub wake_until: Option<OffsetDateTime>,
    pub last_woken_at: Option<OffsetDateTime>,
    /// Set on sessions started by a headless run.
    pub run_id: Option<String>,
}

/// A bearer token as listed to the admin (the plaintext is shown once, at creation, and
/// only its SHA-256 hash is stored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub id: TokenId,
    pub label: String,
    pub created_at: OffsetDateTime,
    pub last_used_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
}

/// Generates `as_str` / `FromStr` / `Display` for a fieldless enum stored as text.
macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(format!(concat!("unknown ", stringify!($name), " {:?}"), other)),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Asked of another project and waiting for the human to approve it; never claimable.
    Requested,
    Open,
    /// Claimed, but the owner is writing a plan for the human to approve before any work.
    Planning,
    Claimed,
    Blocked,
    Review,
    Done,
    Cancelled,
}

text_enum!(TaskStatus {
    Requested => "requested",
    Open => "open",
    Planning => "planning",
    Claimed => "claimed",
    Blocked => "blocked",
    Review => "review",
    Done => "done",
    Cancelled => "cancelled",
});

/// What a session's harness says it is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    #[default]
    Unknown,
    Busy,
    Idle,
}

text_enum!(TurnState {
    Unknown => "unknown",
    Busy => "busy",
    Idle => "idle",
});

/// How Axle reaches an idle session (`None`: it cannot).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WakeChannel {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "claude-socket")]
    ClaudeSocket,
    #[serde(rename = "claude-monitor")]
    ClaudeMonitor,
    #[serde(rename = "codex-app-server")]
    CodexAppServer,
    #[serde(rename = "opencode-api")]
    OpencodeApi,
}

text_enum!(WakeChannel {
    None => "none",
    ClaudeSocket => "claude-socket",
    ClaudeMonitor => "claude-monitor",
    CodexAppServer => "codex-app-server",
    OpencodeApi => "opencode-api",
});

/// Whether an attempt woke a live session or started a headless run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeKind {
    Wake,
    Headless,
}

text_enum!(WakeKind {
    Wake => "wake",
    Headless => "headless",
});

/// What caused a wake attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeTrigger {
    HumanPost,
    HumanReply,
    HumanDecision,
    StartNow,
    ChildOfSanctioned,
    RequestApproved,
    AutoStart,
    PeerPost,
    ChildClosed,
    Queue,
}

text_enum!(WakeTrigger {
    HumanPost => "human_post",
    HumanReply => "human_reply",
    HumanDecision => "human_decision",
    StartNow => "start_now",
    ChildOfSanctioned => "child_of_sanctioned",
    RequestApproved => "request_approved",
    AutoStart => "auto_start",
    PeerPost => "peer_post",
    ChildClosed => "child_closed",
    Queue => "queue",
});

/// How an attempt turned out. Stored as unchecked text so later plans can add outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeOutcome {
    Sent,
    Delivered,
    Claimed,
    Expired,
    Failed,
    Held,
    Refused,
    Skipped,
}

text_enum!(WakeOutcome {
    Sent => "sent",
    Delivered => "delivered",
    Claimed => "claimed",
    Expired => "expired",
    Failed => "failed",
    Held => "held",
    Refused => "refused",
    Skipped => "skipped",
});

/// Where a work request stands when it is created (carried by `RequestCreated`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    AwaitingHuman,
    Routed,
    Queued,
}

text_enum!(RequestState {
    AwaitingHuman => "awaiting_human",
    Routed => "routed",
    Queued => "queued",
});

/// One row of the append-only wake log. Every change of outcome is a new row; caps count
/// `Sent` rows; `run_id` links the rows of one headless run.
#[derive(Debug, Clone, PartialEq)]
pub struct WakeAttempt {
    pub id: Uuid,
    pub kind: WakeKind,
    pub trigger: WakeTrigger,
    pub task_id: Option<TaskId>,
    pub session_id: Option<SessionId>,
    pub from_agent_id: Option<AgentId>,
    pub runner_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    pub outcome: WakeOutcome,
    pub detail: Option<String>,
    pub pr_url: Option<String>,
    pub at: OffsetDateTime,
}

/// A project's wake and headless-run settings (no stored row means `defaults`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectWakeSettings {
    pub project_id: ProjectId,
    pub auto_start: bool,
    pub headless_enabled: bool,
    pub headless_harness: Option<String>,
    pub max_concurrent_runs: u32,
    pub daily_run_cap: u32,
    pub max_run_minutes: u32,
    pub max_budget_usd: Option<f64>,
}

impl ProjectWakeSettings {
    pub fn defaults(project_id: ProjectId) -> Self {
        Self {
            project_id,
            auto_start: false,
            headless_enabled: false,
            headless_harness: None,
            max_concurrent_runs: 1,
            daily_run_cap: 5,
            max_run_minutes: 30,
            max_budget_usd: None,
        }
    }
}

/// Server-wide switches; both off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerSettings {
    pub wake_paused: bool,
    pub headless_paused: bool,
}

/// Where a `requires_plan` task's plan is (`None` for tasks without the gate, and after a
/// release).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    #[default]
    None,
    /// Claimed; the owner is writing the plan.
    Drafting,
    /// Waiting on the human (the lease does not expire).
    Submitted,
    /// The human asked for changes; the owner is revising.
    ChangesRequested,
    Approved,
}

text_enum!(PlanState {
    None => "none",
    Drafting => "drafting",
    Submitted => "submitted",
    ChangesRequested => "changes_requested",
    Approved => "approved",
});

/// How an enrollment is redeemed: a one-time code pasted into a command, or the device flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentKind {
    Code,
    Device,
}

text_enum!(EnrollmentKind {
    Code => "code",
    Device => "device",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentStatus {
    Pending,
    Approved,
    Consumed,
    Expired,
    Denied,
}

text_enum!(EnrollmentStatus {
    Pending => "pending",
    Approved => "approved",
    Consumed => "consumed",
    Expired => "expired",
    Denied => "denied",
});

/// A pending or finished request to mint a host token (the secrets are stored only as hashes
/// and never appear here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub id: EnrollmentId,
    pub kind: EnrollmentKind,
    pub label: String,
    pub host: String,
    pub status: EnrollmentStatus,
    pub token_id: Option<TokenId>,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub approved_at: Option<OffsetDateTime>,
    pub requester_ip: Option<String>,
}

/// Optional claim filter: when set, only agents with this harness / on this host may claim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Eligibility {
    pub harness: Option<String>,
    pub host: Option<String>,
}

/// Who did something: an agent session, or the human (UI / admin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    Agent(AgentId, SessionId),
    Human,
}

impl Actor {
    /// `(agent, session)` columns; both `None` for the human.
    pub fn ids(&self) -> (Option<AgentId>, Option<SessionId>) {
        match *self {
            Actor::Agent(a, s) => (Some(a), Some(s)),
            Actor::Human => (None, None),
        }
    }

    /// Inverse of `ids`: anything but a full pair is the human.
    pub fn from_ids(agent: Option<AgentId>, session: Option<SessionId>) -> Actor {
        match (agent, session) {
            (Some(a), Some(s)) => Actor::Agent(a, s),
            _ => Actor::Human,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    /// Home project.
    pub project_id: ProjectId,
    pub parent_id: Option<TaskId>,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub owner_session_id: Option<SessionId>,
    pub owner_agent_id: Option<AgentId>,
    /// `None` while not claimed, and while `blocked` / `review` (those never expire).
    pub lease_expires_at: Option<OffsetDateTime>,
    /// Incremented by every successful claim.
    pub lease_epoch: i64,
    /// Per-task lease length (whole seconds); `None` uses the server-wide TTL.
    pub lease_ttl: Option<time::Duration>,
    pub eligibility: Eligibility,
    pub created_by: Actor,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// When the task last entered `done`; `None` in any other status.
    pub done_at: Option<OffsetDateTime>,
    /// When the human acknowledged the done task; cleared when it leaves `done`.
    pub acked_at: Option<OffsetDateTime>,
    /// Incremented by every state change.
    pub version: i64,
    /// Claiming moves it to `planning`; work starts once the human approves the plan.
    pub requires_plan: bool,
    pub plan_state: PlanState,
    /// Why a `blocked` task is blocked and what would unblock it; `None` in any other status
    /// (and for a task blocked before the detail was recorded, if its history has no reason).
    pub block: Option<BlockDetail>,
    /// The human (or a task they sanctioned) approved waking agents for this task.
    pub sanctioned: bool,
    /// How many project hops a request has made; 0 for ordinary tasks.
    pub request_depth: u32,
    /// The task that started the request chain.
    pub origin_task_id: Option<TaskId>,
    /// The projects the request chain has passed through, in order.
    pub request_chain: Vec<ProjectId>,
    /// Collapses repeated identical requests inside the dedupe window.
    pub request_dedupe_key: Option<String>,
}

/// What a blocked task waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitingOn {
    /// The human: the task is a "Blocked on you" card in the UI's Needs you.
    Human,
    /// Another task: when it is `done`, this one is unblocked automatically.
    Task(TaskId),
    /// Something outside Axle, in words.
    External(String),
}

impl WaitingOn {
    /// The stored `waiting_kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            WaitingOn::Human => "human",
            WaitingOn::Task(_) => "task",
            WaitingOn::External(_) => "external",
        }
    }

    /// The stored `waiting_task_id`.
    pub fn task(&self) -> Option<TaskId> {
        match self {
            WaitingOn::Task(t) => Some(*t),
            _ => None,
        }
    }

    /// The stored `waiting_text`.
    pub fn text(&self) -> Option<&str> {
        match self {
            WaitingOn::External(t) => Some(t),
            _ => None,
        }
    }

    /// Inverse of `kind` / `task` / `text`; `None` for no or unknown kind.
    pub fn from_columns(
        kind: Option<&str>,
        task: Option<TaskId>,
        text: Option<String>,
    ) -> Option<WaitingOn> {
        match (kind?, task) {
            ("human", _) => Some(WaitingOn::Human),
            ("task", Some(t)) => Some(WaitingOn::Task(t)),
            ("external", _) => Some(WaitingOn::External(text.unwrap_or_default())),
            _ => None,
        }
    }

    /// As JSON for tools and history: `{"human": true}`, `{"task": id}`, `{"external": text}`.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            WaitingOn::Human => serde_json::json!({ "human": true }),
            WaitingOn::Task(t) => serde_json::json!({ "task": t.to_string() }),
            WaitingOn::External(text) => serde_json::json!({ "external": text }),
        }
    }
}

/// The current block of a `blocked` task (its history keeps every earlier one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDetail {
    /// Agent-written: why it is blocked.
    pub reason: String,
    /// When it entered `blocked` (an update keeps this).
    pub since: Option<OffsetDateTime>,
    pub waiting_on: Option<WaitingOn>,
    /// Agent-written: what would unblock it.
    pub unblock: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEventKind {
    Created,
    Claimed,
    Renewed,
    Released,
    Blocked,
    Review,
    Done,
    Cancelled,
    LeaseExpiredTakeover,
    ForceReleased,
    ChildClaimed,
    ChildBlocked,
    ChildDone,
    ChildCancelled,
    ChildrenComplete,
    /// The title and/or body was changed (the previous values are in the payload).
    Edited,
    /// The owner submitted a plan for approval (the plan is a `plan` message in the thread).
    PlanSubmitted,
    PlanApproved,
    /// The human asked for changes to the plan (the note is the event's note).
    PlanChangesRequested,
    /// Participating projects were added to a parent task (listed in the payload).
    ProjectsAdded,
    /// The owner changed a blocked task's reason, what it waits on or what unblocks it (the
    /// previous values are in the payload).
    BlockUpdated,
    /// The task this one waited on is done, so it left `blocked` on its own.
    Unblocked,
    /// Links to other tasks were added or removed (listed in the payload).
    Related,
    /// A top-level task was moved under a parent (in the payload; the note names it).
    MovedUnder,
    /// No session was free when the task became routable, so it waits in the wake queue.
    Queued,
    /// Axle reserved a free session for the task and asked it to wake (payload: the session
    /// and agent).
    Routed,
    /// The session's bridge reported that it delivered the wake prompt.
    WakeSent,
    /// The wake could not be delivered (failed, held or refused); that session is not tried
    /// again for this task.
    WakeFailed,
    /// The human acknowledged a done task. Only ever published as a live event; it is
    /// never written to a task's history.
    Acked,
}

text_enum!(TaskEventKind {
    Created => "created",
    Claimed => "claimed",
    Renewed => "renewed",
    Released => "released",
    Blocked => "blocked",
    Review => "review",
    Done => "done",
    Cancelled => "cancelled",
    LeaseExpiredTakeover => "lease_expired_takeover",
    ForceReleased => "force_released",
    ChildClaimed => "child_claimed",
    ChildBlocked => "child_blocked",
    ChildDone => "child_done",
    ChildCancelled => "child_cancelled",
    ChildrenComplete => "children_complete",
    Edited => "edited",
    PlanSubmitted => "plan_submitted",
    PlanApproved => "plan_approved",
    PlanChangesRequested => "plan_changes_requested",
    ProjectsAdded => "projects_added",
    BlockUpdated => "block_updated",
    Unblocked => "unblocked",
    Related => "related",
    MovedUnder => "moved_under",
    Queued => "queued",
    Routed => "routed",
    WakeSent => "wake_sent",
    WakeFailed => "wake_failed",
    Acked => "acked",
});

/// Where a done task stands with the human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckState {
    /// Not acked and finished within the auto-ack window.
    Unseen,
    /// The human acked it.
    Acked,
    /// Never acked, but older than the auto-ack window.
    AutoAcked,
}

impl AckState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unseen => "unseen",
            Self::Acked => "acked",
            Self::AutoAcked => "auto-acked",
        }
    }
}

/// `None` unless the task is done. A task exactly `auto_ack` old is still unseen.
pub fn ack_state(task: &Task, now: OffsetDateTime, auto_ack: time::Duration) -> Option<AckState> {
    if task.status != TaskStatus::Done {
        return None;
    }
    if task.acked_at.is_some() {
        return Some(AckState::Acked);
    }
    match task.done_at {
        Some(done_at) if done_at < now - auto_ack => Some(AckState::AutoAcked),
        _ => Some(AckState::Unseen),
    }
}

/// What a link says about the task it is written from (`TaskLink::task_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkKind {
    /// The two tasks are related; reads the same from both sides.
    Relates,
    /// The task follows up on the target.
    FollowUp,
    /// The task duplicates the target (and was cancelled for it).
    Duplicate,
}

text_enum!(LinkKind {
    Relates => "relates",
    FollowUp => "follow-up",
    Duplicate => "duplicate",
});

impl LinkKind {
    /// The link in words, as seen from one of its tasks: `outgoing` from the task it is
    /// written from, else from its target. Followed by the other task's title.
    pub fn label(&self, outgoing: bool) -> &'static str {
        match (self, outgoing) {
            (LinkKind::Relates, _) => "related to",
            (LinkKind::FollowUp, true) => "follow-up of",
            (LinkKind::FollowUp, false) => "follow-up",
            (LinkKind::Duplicate, true) => "duplicate of",
            (LinkKind::Duplicate, false) => "duplicated by",
        }
    }
}

/// A link between two tasks, written from `task_id` to `target_id`; both tasks show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLink {
    pub task_id: TaskId,
    pub target_id: TaskId,
    pub kind: LinkKind,
    pub created_at: OffsetDateTime,
}

/// One append-only entry in a task's history.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskEvent {
    pub id: Uuid,
    pub task_id: TaskId,
    pub at: OffsetDateTime,
    pub actor: Actor,
    pub kind: TaskEventKind,
    pub note: Option<String>,
    /// Structured detail, e.g. the previous owner on a takeover or a completion report.
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    #[default]
    Normal,
    High,
    Urgent,
}

text_enum!(Importance {
    Normal => "normal",
    High => "high",
    Urgent => "urgent",
});

impl Importance {
    /// 0 normal, 1 high, 2 urgent (the order SQL filters compare).
    pub fn rank(&self) -> i64 {
        match self {
            Importance::Normal => 0,
            Importance::High => 1,
            Importance::Urgent => 2,
        }
    }
}

/// Where a message goes (spec §9). The project is part of the address, not an access
/// barrier: any agent may address any project or agent.
///
/// Serialised as `{"kind": "agent", "id": "<uuid>"}` (`{"kind": "human"}` for the human).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Address {
    /// Every agent whose home project this is.
    Project(ProjectId),
    /// Durable: any session of the agent may read it; acked once for the agent.
    Agent(AgentId),
    /// One live session only; acked per session.
    Session(SessionId),
    /// The task's thread: delivered to its current owner and its creator.
    Task(TaskId),
    /// The owner's inbox.
    Human,
}

impl Address {
    /// `(to_kind, to_id)` columns.
    pub fn columns(&self) -> (&'static str, Option<Uuid>) {
        match *self {
            Address::Project(p) => ("project", Some(p.as_uuid())),
            Address::Agent(a) => ("agent", Some(a.as_uuid())),
            Address::Session(s) => ("session", Some(s.as_uuid())),
            Address::Task(t) => ("task", Some(t.as_uuid())),
            Address::Human => ("human", None),
        }
    }

    /// Inverse of `columns`.
    pub fn from_columns(kind: &str, id: Option<Uuid>) -> Result<Address, String> {
        Ok(match (kind, id) {
            ("project", Some(id)) => Address::Project(id.into()),
            ("agent", Some(id)) => Address::Agent(id.into()),
            ("session", Some(id)) => Address::Session(id.into()),
            ("task", Some(id)) => Address::Task(id.into()),
            ("human", None) => Address::Human,
            (kind, id) => return Err(format!("bad message address {kind:?} {id:?}")),
        })
    }
}

/// What a message is, beyond who sent it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    #[default]
    Message,
    /// A plan submitted for the human's approval (the latest one in a thread is under review).
    Plan,
    /// A note the server wrote on someone's behalf (an approval, added projects).
    System,
}

text_enum!(MessageKind {
    Message => "message",
    Plan => "plan",
    System => "system",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    /// The project the message is about: the addressed project, the addressed agent's /
    /// session's home project or the task's project; for `Human`, the sender's home project.
    pub project_id: Option<ProjectId>,
    pub from: Actor,
    pub to: Address,
    pub subject: Option<String>,
    pub body: String,
    pub importance: Importance,
    /// Set for task-thread messages.
    pub task_id: Option<TaskId>,
    pub reply_to: Option<MessageId>,
    pub created_at: OffsetDateTime,
    pub kind: MessageKind,
    /// Quick replies an agent offers the human (2-4 short answers); empty for most mail.
    pub options: Vec<String>,
}

/// A message as one recipient sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxItem {
    pub message: Message,
    /// When this recipient first fetched it.
    pub delivered_at: OffsetDateTime,
    pub read_at: Option<OffsetDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn ack_state_follows_window_and_ack() {
        let now = datetime!(2026-11-27 9:00 UTC);
        let n = time::Duration::days(7);
        let task = |status, done_at, acked_at| Task {
            id: TaskId::new(),
            project_id: ProjectId::new(),
            parent_id: None,
            title: "t".into(),
            body: String::new(),
            status,
            owner_session_id: None,
            owner_agent_id: None,
            lease_expires_at: None,
            lease_epoch: 0,
            lease_ttl: None,
            eligibility: Eligibility::default(),
            created_by: Actor::from_ids(None, None),
            created_at: now,
            updated_at: now,
            done_at,
            acked_at,
            version: 1,
            requires_plan: false,
            plan_state: PlanState::None,
            block: None,
            sanctioned: false,
            request_depth: 0,
            origin_task_id: None,
            request_chain: Vec::new(),
            request_dedupe_key: None,
        };
        let done = |at| task(TaskStatus::Done, Some(at), None);
        assert_eq!(ack_state(&done(now - n), now, n), Some(AckState::Unseen));
        assert_eq!(
            ack_state(&done(now - n - time::Duration::seconds(1)), now, n),
            Some(AckState::AutoAcked)
        );
        let acked = task(TaskStatus::Done, Some(now - n * 2), Some(now));
        assert_eq!(ack_state(&acked, now, n), Some(AckState::Acked));
        assert_eq!(ack_state(&task(TaskStatus::Open, None, None), now, n), None);
        assert_eq!(AckState::AutoAcked.as_str(), "auto-acked");
        assert_eq!(AckState::Unseen.as_str(), "unseen");
        assert_eq!(AckState::Acked.as_str(), "acked");
    }

    #[test]
    fn display_names_are_cleaned() {
        assert_eq!(
            clean_display_name("  platform-infra  ").unwrap(),
            "platform-infra"
        );
        assert_eq!(
            clean_display_name(&"é".repeat(80)).unwrap().chars().count(),
            80
        );
        for bad in [
            "",
            " \t ",
            "a\nb",
            "a\u{7f}b",
            "[axl from x]",
            "x]",
            "rtl\u{202E}gnp",
            "zero\u{200B}width",
        ] {
            assert!(clean_display_name(bad).is_err(), "{bad:?}");
        }
        assert!(clean_display_name(&"x".repeat(81)).is_err());
    }

    #[test]
    fn task_enums_round_trip_as_text() {
        for s in [
            TaskStatus::Requested,
            TaskStatus::Open,
            TaskStatus::Planning,
            TaskStatus::Claimed,
            TaskStatus::Blocked,
            TaskStatus::Review,
            TaskStatus::Done,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(s.as_str().parse::<TaskStatus>().unwrap(), s);
        }
        assert_eq!(
            TaskEventKind::LeaseExpiredTakeover.to_string(),
            "lease_expired_takeover"
        );
        assert!("nope".parse::<TaskStatus>().is_err());
        for p in [
            PlanState::None,
            PlanState::Drafting,
            PlanState::Submitted,
            PlanState::ChangesRequested,
            PlanState::Approved,
        ] {
            assert_eq!(p.as_str().parse::<PlanState>().unwrap(), p);
        }
        assert_eq!(MessageKind::Plan.to_string(), "plan");
        for t in [TurnState::Unknown, TurnState::Busy, TurnState::Idle] {
            assert_eq!(t.as_str().parse::<TurnState>().unwrap(), t);
        }
        for (c, text) in [
            (WakeChannel::None, "none"),
            (WakeChannel::ClaudeSocket, "claude-socket"),
            (WakeChannel::ClaudeMonitor, "claude-monitor"),
            (WakeChannel::CodexAppServer, "codex-app-server"),
            (WakeChannel::OpencodeApi, "opencode-api"),
        ] {
            assert_eq!(c.as_str(), text);
            assert_eq!(text.parse::<WakeChannel>().unwrap(), c);
        }
        for k in [WakeKind::Wake, WakeKind::Headless] {
            assert_eq!(k.as_str().parse::<WakeKind>().unwrap(), k);
        }
        for t in [
            WakeTrigger::HumanPost,
            WakeTrigger::HumanReply,
            WakeTrigger::HumanDecision,
            WakeTrigger::StartNow,
            WakeTrigger::ChildOfSanctioned,
            WakeTrigger::RequestApproved,
            WakeTrigger::AutoStart,
            WakeTrigger::PeerPost,
            WakeTrigger::ChildClosed,
            WakeTrigger::Queue,
        ] {
            assert_eq!(t.as_str().parse::<WakeTrigger>().unwrap(), t);
        }
        assert_eq!(
            WakeTrigger::ChildOfSanctioned.to_string(),
            "child_of_sanctioned"
        );
        for o in [
            WakeOutcome::Sent,
            WakeOutcome::Delivered,
            WakeOutcome::Claimed,
            WakeOutcome::Expired,
            WakeOutcome::Failed,
            WakeOutcome::Held,
            WakeOutcome::Refused,
            WakeOutcome::Skipped,
        ] {
            assert_eq!(o.as_str().parse::<WakeOutcome>().unwrap(), o);
        }
    }

    #[test]
    fn ids_round_trip_through_display_and_from_str() {
        let id = TaskId::new();
        assert_eq!(id.to_string().parse::<TaskId>().unwrap(), id);
        assert!("not-a-uuid".parse::<AgentId>().is_err());
    }

    #[test]
    fn ids_are_uuid_v7() {
        assert_eq!(SessionId::new().as_uuid().get_version_num(), 7);
    }
}
