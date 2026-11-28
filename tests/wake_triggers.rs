//! Trigger wiring (spec "Trigger rules"): which human actions, task creations and posts wake
//! a session, the covering rule for children of sanctioned tasks, and the peer-wake guards.

// Service tests run on SQLite only; `store_matrix!` is not needed here.
#[allow(unused_macros)]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::config::ServerConfig;
use axle::domain::{
    Actor, Address, AgentId, Eligibility, Importance, MessageKind, ProjectId, ProjectWakeSettings,
    SessionId, Task, TaskEventKind, TaskId, TaskStatus, TurnState, WakeChannel, WakeKind,
    WakeOutcome, WakeTrigger,
};
use axle::events::EventBus;
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::messages::{InboxFilter, MessageService, NewMessage};
use axle::service::tasks::{Caller, ClaimOutcome, Completion, NewRequest, NewTask, TaskService};
use axle::service::wake::WakeService;
use axle::store::{AttemptQuery, Store};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-28 9:00 UTC);
const PA: &str = "github.com/acme/a";
const PB: &str = "github.com/acme/b";
const PC: &str = "github.com/acme/c";
const PD: &str = "github.com/acme/d";

struct Env {
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    wake: Arc<WakeService>,
    tasks: Arc<TaskService>,
    messages: Arc<MessageService>,
    _db: common::TempDb,
}

fn config() -> ServerConfig {
    ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap()
}

/// What a wake-log count matches.
#[derive(Default, Clone, Copy)]
struct Count {
    outcome: Option<WakeOutcome>,
    session: Option<SessionId>,
    task: Option<TaskId>,
    from: Option<AgentId>,
    trigger: Option<WakeTrigger>,
}

impl Env {
    async fn new() -> Self {
        let (store, db) = common::sqlite_store().await;
        let clock = Arc::new(TestClock::new(T0));
        let bus = EventBus::new(store.clone(), clock.clone());
        let cfg = config();
        let wake =
            Arc::new(WakeService::new(store.clone(), clock.clone(), &cfg).with_events(bus.clone()));
        let tasks = Arc::new(
            TaskService::from_config(store.clone(), clock.clone(), &cfg)
                .with_events(bus.clone())
                .with_wake(wake.clone()),
        );
        let messages = Arc::new(
            MessageService::new(store.clone(), clock.clone())
                .with_events(bus)
                .with_wake(wake.clone()),
        );
        Self {
            store,
            clock,
            wake,
            tasks,
            messages,
            _db: db,
        }
    }

    fn now(&self) -> OffsetDateTime {
        self.clock.now()
    }

    fn advance(&self, d: Duration) {
        self.clock.advance(d);
    }

    async fn session_of(&self, project: &str, host: &str) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(project.into()),
                host: host.into(),
                harness: "claude-code".into(),
                session: SessionId::new(),
                token: common::token(&*self.store).await,
            },
        )
        .await
        .unwrap()
    }

    /// Makes `s` wakeable, heartbeating now and idle for the last 10 s.
    async fn idle(&self, s: &Resolved) {
        self.store
            .report_turn(
                s.session.id,
                Some(TurnState::Idle),
                Some(WakeChannel::ClaudeSocket),
                self.now() - Duration::seconds(10),
            )
            .await
            .unwrap();
        self.store
            .touch_heartbeat(s.session.id, self.now())
            .await
            .unwrap();
    }

    async fn busy(&self, s: &Resolved) {
        self.wake
            .on_turn(s.session.id, TurnState::Busy)
            .await
            .unwrap();
    }

    /// A woken session takes its turn (which clears an inbox wake) and goes idle again.
    async fn settle(&self, s: &Resolved) {
        self.busy(s).await;
        self.idle(s).await;
    }

    async fn project(&self, key: &str) -> ProjectId {
        self.store
            .upsert_project(key, key, self.now())
            .await
            .unwrap()
            .id
    }

    async fn auto_start(&self, key: &str) {
        let project = self.project(key).await;
        let settings = ProjectWakeSettings {
            auto_start: true,
            ..ProjectWakeSettings::defaults(project)
        };
        self.store
            .put_wake_settings(&settings, self.now())
            .await
            .unwrap();
    }

    async fn human_task(&self, key: &str, title: &str, start_now: bool) -> Task {
        self.human_task_with(
            key,
            NewTask {
                title: title.into(),
                start_now,
                ..NewTask::default()
            },
        )
        .await
    }

    async fn human_task_with(&self, key: &str, new: NewTask) -> Task {
        let project_id = self.project(key).await;
        self.tasks
            .create(Caller::Human, NewTask { project_id, ..new })
            .await
            .unwrap()
    }

    async fn child(&self, by: &Resolved, parent: &Task, title: &str) -> Task {
        self.tasks
            .create(
                Caller::Agent(by),
                NewTask {
                    project_id: parent.project_id,
                    parent_id: Some(parent.id),
                    title: title.into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    async fn claim(&self, s: &Resolved, id: TaskId) {
        assert!(matches!(
            self.tasks.claim(s, id).await.unwrap(),
            ClaimOutcome::Claimed(_)
        ));
    }

    async fn get(&self, id: TaskId) -> Task {
        self.store.get_task(id).await.unwrap().unwrap()
    }

    /// An agent's post into a task thread (`task_post`).
    async fn post(&self, by: &Resolved, task: TaskId, body: &str) {
        self.send(by, Address::Task(task), body).await;
    }

    async fn send(&self, by: &Resolved, to: Address, body: &str) {
        self.messages
            .send(
                &Actor::Agent(by.agent.id, by.session.id),
                message(to, body, None),
            )
            .await
            .unwrap();
    }

    /// Wake-log rows of kind `wake` matching `c` (outcome `sent` unless given).
    async fn count(&self, c: Count) -> u32 {
        self.store
            .count_wake_attempts(&AttemptQuery {
                kind: WakeKind::Wake,
                outcome: c.outcome.unwrap_or(WakeOutcome::Sent),
                session: c.session,
                task: c.task,
                from_agent: c.from,
                triggers: c.trigger.map(|t| vec![t]),
                since: T0 - Duration::days(1),
            })
            .await
            .unwrap()
    }

    /// `sent` wakes of `s` with `trigger`.
    async fn woken(&self, s: &Resolved, trigger: WakeTrigger) -> u32 {
        self.count(Count {
            session: Some(s.session.id),
            trigger: Some(trigger),
            ..Count::default()
        })
        .await
    }

    async fn sent(&self) -> u32 {
        self.count(Count::default()).await
    }

    /// The task `s` is reserved for, if it holds an unexpired reservation for one.
    async fn reserved_task(&self, s: &Resolved) -> Option<TaskId> {
        let session = self.store.get_session(s.session.id).await.unwrap().unwrap();
        match session.wake_until {
            Some(until) if until > self.now() => session.wake_task_id,
            _ => None,
        }
    }

    /// Bodies in `s`'s inbox that start with `prefix`.
    async fn inbox_bodies(&self, s: &Resolved, prefix: &str) -> Vec<String> {
        self.messages
            .inbox(s, InboxFilter::default())
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.message.body)
            .filter(|b| b.starts_with(prefix))
            .collect()
    }
}

fn message(to: Address, body: &str, reply_to: Option<axle::domain::MessageId>) -> NewMessage {
    NewMessage {
        to,
        subject: None,
        body: body.into(),
        importance: Importance::Normal,
        reply_to,
    }
}

// ---------------------------------------------------------------- human triggers

#[tokio::test]
async fn human_thread_post_wakes_idle_owner() {
    let env = Env::new().await;
    let owner = env.session_of(PA, "a").await;
    let task = env.human_task(PA, "fix the build", false).await;
    env.claim(&owner, task.id).await;

    // A busy owner is not interrupted.
    env.busy(&owner).await;
    env.messages
        .send(
            &Actor::Human,
            message(Address::Task(task.id), "busy?", None),
        )
        .await
        .unwrap();
    assert_eq!(env.sent().await, 0);

    env.idle(&owner).await;
    env.messages
        .send(&Actor::Human, message(Address::Task(task.id), "now?", None))
        .await
        .unwrap();
    assert_eq!(env.woken(&owner, WakeTrigger::HumanPost).await, 1);
    assert_eq!(env.sent().await, 1);
    // An inbox wake: about the task, not a new job, so nothing to claim.
    assert_eq!(env.reserved_task(&owner).await, None);
}

#[tokio::test]
async fn human_inbox_reply_wakes_one_session_of_agent() {
    let env = Env::new().await;
    let s1 = env.session_of(PA, "a").await;
    let s2 = env.session_of(PA, "a").await;
    assert_eq!(s1.agent.id, s2.agent.id, "two sessions of one agent");
    env.idle(&s1).await;
    env.idle(&s2).await;

    // A fresh message from the human is not a reply: no wake.
    env.messages
        .send_as_human(message(Address::Agent(s1.agent.id), "fyi", None))
        .await
        .unwrap();
    assert_eq!(env.sent().await, 0);

    let question = env
        .messages
        .send(
            &Actor::Agent(s1.agent.id, s1.session.id),
            message(Address::Human, "which branch?", None),
        )
        .await
        .unwrap();
    env.messages
        .send_as_human(message(
            Address::Agent(s1.agent.id),
            "main",
            Some(question.id),
        ))
        .await
        .unwrap();
    let woken = env.woken(&s1, WakeTrigger::HumanReply).await
        + env.woken(&s2, WakeTrigger::HumanReply).await;
    assert_eq!(woken, 1, "exactly one session of the agent");
    assert_eq!(env.sent().await, 1);
}

#[tokio::test]
async fn plan_decision_wakes_owner_and_sanctions() {
    let env = Env::new().await;
    let a = env.session_of(PA, "a").await;
    let b = env.session_of(PB, "b").await;
    let plan_task = |key: &'static str, title: &'static str| {
        let env = &env;
        async move {
            env.human_task_with(
                key,
                NewTask {
                    title: title.into(),
                    requires_plan: true,
                    ..NewTask::default()
                },
            )
            .await
        }
    };
    let approved = plan_task(PA, "approve me").await;
    let revised = plan_task(PB, "revise me").await;
    env.claim(&a, approved.id).await;
    env.claim(&b, revised.id).await;
    let (_, plan_a) = env
        .tasks
        .propose_plan(&a, approved.id, "step 1".into())
        .await
        .unwrap();
    let (_, plan_b) = env
        .tasks
        .propose_plan(&b, revised.id, "step 1".into())
        .await
        .unwrap();
    env.idle(&a).await;
    env.idle(&b).await;

    let task = env
        .tasks
        .approve_plan(approved.id, plan_a.id)
        .await
        .unwrap();
    assert!(task.sanctioned, "approving the plan sanctions the task");
    assert!(env.get(approved.id).await.sanctioned);
    assert_eq!(env.woken(&a, WakeTrigger::HumanDecision).await, 1);

    let task = env
        .tasks
        .request_plan_changes(revised.id, plan_b.id, "split step 1")
        .await
        .unwrap();
    assert!(!task.sanctioned, "requesting changes sanctions nothing");
    assert_eq!(env.woken(&b, WakeTrigger::HumanDecision).await, 1);
    assert_eq!(env.sent().await, 2);
}

#[tokio::test]
async fn start_now_routes_and_unticked_does_not() {
    let env = Env::new().await;
    let free = env.session_of(PA, "f").await;
    env.idle(&free).await;

    let plain = env.human_task(PA, "later", false).await;
    assert_eq!(plain.status, TaskStatus::Open);
    assert!(!plain.sanctioned);
    assert_eq!(env.reserved_task(&free).await, None);
    assert_eq!(env.sent().await, 0);

    let now = env.human_task(PA, "now", true).await;
    assert_eq!(now.status, TaskStatus::Open);
    assert!(now.sanctioned);
    assert!(env.get(now.id).await.sanctioned);
    assert_eq!(env.reserved_task(&free).await, Some(now.id));
    assert_eq!(env.woken(&free, WakeTrigger::StartNow).await, 1);
    assert_eq!(env.sent().await, 1);
}

// ---------------------------------------------------------------- the covering rule

#[tokio::test]
async fn child_of_sanctioned_routes_without_card() {
    let env = Env::new().await;
    let coordinator = env.session_of(PA, "c").await;
    let parent = env.human_task(PA, "epic", true).await;
    env.claim(&coordinator, parent.id).await;
    let free = env.session_of(PA, "f").await;
    env.idle(&free).await;
    // The coordinator's own agent is never woken for its child.
    env.idle(&coordinator).await;

    let child = env.child(&coordinator, &parent, "part 1").await;
    assert_eq!(child.status, TaskStatus::Open, "no card: not a request");
    assert!(child.sanctioned);
    assert!(env.get(child.id).await.sanctioned);
    let kinds: Vec<_> = env
        .store
        .task_events(child.id)
        .await
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds[0], TaskEventKind::Created);
    assert!(!kinds.contains(&TaskEventKind::RequestCreated));
    assert!(kinds.contains(&TaskEventKind::Routed));
    assert_eq!(env.reserved_task(&free).await, Some(child.id));
    assert_eq!(
        env.count(Count {
            session: Some(free.session.id),
            from: Some(coordinator.agent.id),
            trigger: Some(WakeTrigger::ChildOfSanctioned),
            ..Count::default()
        })
        .await,
        1
    );
    assert_eq!(env.reserved_task(&coordinator).await, None);
}

#[tokio::test]
async fn child_of_unsanctioned_is_pull_only() {
    let env = Env::new().await;
    let coordinator = env.session_of(PA, "c").await;
    let parent = env.human_task(PA, "epic", false).await;
    env.claim(&coordinator, parent.id).await;
    let free = env.session_of(PA, "f").await;
    env.idle(&free).await;

    let child = env.child(&coordinator, &parent, "part 1").await;
    assert_eq!(child.status, TaskStatus::Open);
    assert!(!child.sanctioned);
    assert_eq!(env.reserved_task(&free).await, None);
    assert_eq!(env.sent().await, 0);

    // Sanctioned but not past its plan gate (plan required, not approved): not covered.
    let gated = env
        .human_task_with(
            PB,
            NewTask {
                title: "plan first".into(),
                requires_plan: true,
                start_now: true,
                ..NewTask::default()
            },
        )
        .await;
    assert!(gated.sanctioned);
    let other = env.session_of(PB, "o").await;
    let child = env.child(&other, &gated, "early part").await;
    assert!(!child.sanctioned);
    assert_eq!(env.reserved_task(&free).await, None);
    assert_eq!(env.sent().await, 0);
}

#[tokio::test]
async fn grandchild_is_covered() {
    let env = Env::new().await;
    let coordinator = env.session_of(PA, "c").await;
    let parent = env.human_task(PA, "epic", true).await;
    env.claim(&coordinator, parent.id).await;
    let f1 = env.session_of(PA, "f1").await;
    let f2 = env.session_of(PA, "f2").await;
    env.idle(&f1).await;

    let child = env.child(&coordinator, &parent, "part 1").await;
    assert!(child.sanctioned);
    assert_eq!(env.reserved_task(&f1).await, Some(child.id));
    env.claim(&f1, child.id).await;
    env.idle(&f2).await;
    // The child's holder coordinates its own children.
    let grandchild = env.child(&f1, &child, "part 1a").await;
    assert!(
        grandchild.sanctioned,
        "a child of a sanctioned child is covered"
    );
    assert_eq!(env.reserved_task(&f2).await, Some(grandchild.id));
}

#[tokio::test]
async fn child_by_a_non_holder_is_pull_only() {
    let env = Env::new().await;
    let coordinator = env.session_of(PA, "c").await;
    let parent = env.human_task(PA, "epic", true).await;
    env.claim(&coordinator, parent.id).await;
    let free = env.session_of(PA, "f").await;
    env.idle(&free).await;

    // Another agent of the same project, and an agent of another project, each create a
    // child under the sanctioned task they do not hold.
    let neighbour = env.session_of(PA, "n").await;
    let foreign = env.session_of(PB, "x").await;
    for by in [&neighbour, &foreign] {
        let child = env.child(by, &parent, "unsanctioned part").await;
        assert_eq!(child.status, TaskStatus::Open);
        assert!(!child.sanctioned, "only the parent's holder is covered");
        assert!(!env.get(child.id).await.sanctioned);
        let kinds: Vec<_> = env
            .store
            .task_events(child.id)
            .await
            .unwrap()
            .iter()
            .map(|e| e.kind)
            .collect();
        assert!(!kinds.contains(&TaskEventKind::Routed));
        assert!(!kinds.contains(&TaskEventKind::Queued));
    }
    assert_eq!(env.reserved_task(&free).await, None);
    assert_eq!(env.sent().await, 0);

    // The coordinator's covered child is still not the neighbour's to extend.
    let child = env.child(&coordinator, &parent, "part 1").await;
    assert!(child.sanctioned);
    let stray = env.child(&neighbour, &child, "part 1a").await;
    assert!(!stray.sanctioned);
}

#[tokio::test]
async fn child_inherits_depth_and_chain() {
    let env = Env::new().await;
    env.auto_start(PB).await;
    let requester = env.session_of(PA, "r").await;
    let root = env.human_task(PA, "root", false).await;
    env.claim(&requester, root.id).await;
    let request = env
        .tasks
        .request(
            &requester,
            NewRequest {
                project_id: env.project(PB).await,
                title: "work in b".into(),
                body: String::new(),
                eligibility: Eligibility::default(),
                importance: Importance::Normal,
                parent_id: None,
            },
        )
        .await
        .unwrap()
        .task;
    assert_eq!(request.request_depth, 1);
    assert_eq!(request.request_chain, vec![root.project_id]);
    assert!(request.sanctioned);

    let coordinator = env.session_of(PB, "c").await;
    env.claim(&coordinator, request.id).await;
    let child = env.child(&coordinator, &request, "part").await;
    assert!(child.sanctioned);
    assert_eq!(child.request_depth, 1);
    assert_eq!(child.request_chain, vec![root.project_id]);
    assert_eq!(child.request_dedupe_key, None, "not a new request");
    let stored = env.get(child.id).await;
    assert_eq!(stored.request_depth, 1);
    assert_eq!(stored.request_chain, vec![root.project_id]);
}

// ---------------------------------------------------------------- peer posts

#[tokio::test]
async fn peer_post_wakes_owner_within_guards() {
    let env = Env::new().await;
    let a = env.session_of(PA, "a").await;
    let b = env.session_of(PB, "b").await;
    let ta = env.human_task(PA, "a's task", false).await;
    let tb = env.human_task(PB, "b's task", false).await;
    env.claim(&a, ta.id).await;
    env.claim(&b, tb.id).await;
    env.idle(&b).await;

    // The owner's own post never wakes it.
    env.post(&b, tb.id, "note to self").await;
    assert_eq!(env.sent().await, 0);

    env.post(&a, tb.id, "your build broke mine").await;
    assert_eq!(
        env.count(Count {
            session: Some(b.session.id),
            from: Some(a.agent.id),
            trigger: Some(WakeTrigger::PeerPost),
            ..Count::default()
        })
        .await,
        1
    );
    assert_eq!(env.sent().await, 1);
    assert_eq!(
        env.inbox_bodies(&b, "your build").await,
        vec!["your build broke mine".to_string()]
    );
}

#[tokio::test]
async fn peer_post_over_depth_or_in_chain_delivers_without_wake() {
    let env = Env::new().await;
    env.auto_start(PB).await;
    env.auto_start(PC).await;
    let a = env.session_of(PA, "a").await;
    let b = env.session_of(PB, "b").await;
    let c = env.session_of(PC, "c").await;
    let d = env.session_of(PD, "d").await;

    // A chain A → B → C: c holds a depth-2 task whose chain is [A, B].
    let root = env.human_task(PA, "root", false).await;
    env.claim(&a, root.id).await;
    let ask = |by: &Resolved, key: &'static str, title: &'static str| {
        let env = &env;
        let by = by.clone();
        async move {
            env.tasks
                .request(
                    &by,
                    NewRequest {
                        project_id: env.project(key).await,
                        title: title.into(),
                        body: String::new(),
                        eligibility: Eligibility::default(),
                        importance: Importance::Normal,
                        parent_id: None,
                    },
                )
                .await
                .unwrap()
                .task
        }
    };
    let to_b = ask(&a, PB, "work in b").await;
    env.claim(&b, to_b.id).await;
    let to_c = ask(&b, PC, "work in c").await;
    env.claim(&c, to_c.id).await;
    assert_eq!(to_c.request_depth, 2);
    let td = env.human_task(PD, "d's task", false).await;
    env.claim(&d, td.id).await;
    env.idle(&c).await;
    env.idle(&d).await;

    // c posts at depth 3: delivered, nobody woken.
    env.post(&c, td.id, "deep: hello").await;
    assert_eq!(env.inbox_bodies(&d, "deep:").await.len(), 1);
    // a's project is in to_c's chain: delivered, nobody woken.
    env.post(&a, to_c.id, "loop: hello").await;
    assert_eq!(env.inbox_bodies(&c, "loop:").await.len(), 1);
    assert_eq!(env.sent().await, 0);
    assert_eq!(
        env.count(Count {
            outcome: Some(WakeOutcome::Skipped),
            trigger: Some(WakeTrigger::PeerPost),
            ..Count::default()
        })
        .await,
        2
    );

    // Control: d (depth 1, outside the chain) may wake c.
    env.post(&d, to_c.id, "fine: hello").await;
    assert_eq!(env.woken(&c, WakeTrigger::PeerPost).await, 1);
}

#[tokio::test]
async fn peer_ping_pong_is_capped() {
    let env = Env::new().await;
    let a = env.session_of(PA, "a").await;
    let b = env.session_of(PB, "b").await;
    let c = env.session_of(PC, "c").await;
    let d = env.session_of(PD, "d").await;
    let ta = env.human_task(PA, "a's task", false).await;
    env.claim(&a, ta.id).await;
    // b holds three tasks, one per poster, so the per-task gap never decides.
    let mut tb = Vec::new();
    for i in 0..3 {
        let t = env.human_task(PB, &format!("b's task {i}"), false).await;
        env.claim(&b, t.id).await;
        tb.push(t.id);
    }

    // An hour of rounds 6 minutes apart; posts in a round 90 s apart, so neither the
    // 1-per-minute session cap nor the 5-minute task gap is what stops a wake.
    let step = Duration::seconds(90);
    let rounds = 10;
    for _ in 0..rounds {
        env.settle(&b).await;
        env.post(&a, tb[0], "ping from a").await;
        env.advance(step);
        env.settle(&a).await;
        env.post(&b, ta.id, "pong from b").await;
        env.advance(step);
        env.settle(&b).await;
        env.post(&c, tb[1], "ping from c").await;
        env.advance(step);
        env.settle(&b).await;
        env.post(&d, tb[2], "ping from d").await;
        env.advance(step);
    }
    assert!(env.now() - T0 <= Duration::hours(1));

    // Every post was delivered.
    assert_eq!(env.inbox_bodies(&b, "ping from a").await.len(), rounds);
    assert_eq!(env.inbox_bodies(&b, "ping from c").await.len(), rounds);
    assert_eq!(env.inbox_bodies(&b, "ping from d").await.len(), rounds);
    assert_eq!(env.inbox_bodies(&a, "pong from b").await.len(), rounds);

    let peer = |session: &Resolved, from: Option<AgentId>, outcome| Count {
        outcome: Some(outcome),
        session: Some(session.session.id),
        from,
        trigger: Some(WakeTrigger::PeerPost),
        ..Count::default()
    };
    // a only hears from b: the pair budget (3 per hour) is the binding limit.
    assert_eq!(
        env.count(peer(&a, Some(b.agent.id), WakeOutcome::Sent))
            .await,
        3
    );
    assert_eq!(
        env.count(peer(&a, Some(b.agent.id), WakeOutcome::Skipped))
            .await,
        rounds as u32 - 3,
        "every other post was refused by the peer budget, not another cap"
    );
    // b hears from three posters: the session budget (6 per hour) binds first.
    let a_to_b = env
        .count(peer(&b, Some(a.agent.id), WakeOutcome::Sent))
        .await;
    assert!(a_to_b <= 3, "a → b woke {a_to_b} times");
    let to_b = env.count(peer(&b, None, WakeOutcome::Sent)).await;
    assert_eq!(to_b, 6);
    assert_eq!(
        env.count(peer(&b, None, WakeOutcome::Skipped)).await,
        3 * rounds as u32 - 6
    );
}

#[tokio::test]
async fn child_closed_wakes_parent_owner_outside_budget() {
    let env = Env::new().await;
    let coordinator = env.session_of(PA, "c").await;
    let worker = env.session_of(PA, "w").await;
    let parent = env.human_task(PA, "epic", false).await;
    env.claim(&coordinator, parent.id).await;
    let child = env.child(&coordinator, &parent, "part").await;
    env.claim(&worker, child.id).await;

    // The worker spends its whole pair budget on posts in the parent's thread.
    for i in 0..4 {
        env.settle(&coordinator).await;
        env.post(&worker, parent.id, &format!("progress {i}")).await;
        env.advance(Duration::minutes(6));
    }
    let peer_sent = Count {
        session: Some(coordinator.session.id),
        trigger: Some(WakeTrigger::PeerPost),
        ..Count::default()
    };
    assert_eq!(env.count(peer_sent).await, 3);

    env.settle(&coordinator).await;
    env.tasks
        .complete(
            &worker,
            child.id,
            Completion {
                summary: "done".into(),
                ..Completion::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(env.woken(&coordinator, WakeTrigger::ChildClosed).await, 1);
    assert_eq!(env.count(peer_sent).await, 3, "not counted as a peer wake");
    let notes = env
        .store
        .task_thread(parent.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.kind == MessageKind::System)
        .count();
    assert!(notes >= 1);
}

// ---------------------------------------------------------------- re-routing

#[tokio::test]
async fn released_sanctioned_task_is_routed_again() {
    let env = Env::new().await;
    let a = env.session_of(PA, "a").await;
    let plain = env.human_task(PA, "plain", false).await;
    let released = env.human_task(PA, "released", true).await;
    let forced = env.human_task(PA, "forced", true).await;
    env.claim(&a, plain.id).await;
    env.claim(&a, released.id).await;
    env.claim(&a, forced.id).await;
    let f1 = env.session_of(PA, "f1").await;
    let f2 = env.session_of(PA, "f2").await;
    env.idle(&f1).await;
    env.idle(&f2).await;

    env.tasks.release(&a, plain.id, None).await.unwrap();
    assert_eq!(env.sent().await, 0, "an unsanctioned task stays pull-only");

    env.tasks.release(&a, released.id, None).await.unwrap();
    env.tasks.force_release(forced.id).await.unwrap();
    let mut reserved = vec![
        env.reserved_task(&f1).await.unwrap(),
        env.reserved_task(&f2).await.unwrap(),
    ];
    reserved.sort();
    let mut expected = vec![released.id, forced.id];
    expected.sort();
    assert_eq!(reserved, expected);
    assert_eq!(
        env.count(Count {
            trigger: Some(WakeTrigger::Queue),
            ..Count::default()
        })
        .await,
        2
    );
}

#[tokio::test]
async fn expired_lease_is_requeued_by_tick() {
    let env = Env::new().await;
    let a = env.session_of(PA, "a").await;
    let task = env
        .human_task_with(
            PA,
            NewTask {
                title: "abandoned".into(),
                start_now: true,
                lease_ttl: Some(Duration::seconds(60)),
                ..NewTask::default()
            },
        )
        .await;
    env.claim(&a, task.id).await;
    env.advance(Duration::minutes(2));
    let f = env.session_of(PA, "f").await;
    env.idle(&f).await;

    env.wake.tick().await.unwrap();
    assert_eq!(env.reserved_task(&f).await, Some(task.id));
    assert_eq!(env.woken(&f, WakeTrigger::Queue).await, 1);
}

#[tokio::test]
async fn peer_message_and_top_level_create_never_wake() {
    let env = Env::new().await;
    let a = env.session_of(PA, "a").await;
    let b = env.session_of(PB, "b").await;
    let tb = env.human_task(PB, "b's task", false).await;
    env.claim(&b, tb.id).await;
    env.idle(&b).await;
    let free = env.session_of(PB, "f").await;
    env.idle(&free).await;

    env.send(&a, Address::Agent(b.agent.id), "fyi").await;
    env.send(&a, Address::Session(b.session.id), "fyi").await;
    env.send(&a, Address::Project(tb.project_id), "fyi").await;
    let top = env
        .tasks
        .create(
            Caller::Agent(&a),
            NewTask {
                project_id: tb.project_id,
                title: "top level".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    assert!(!top.sanctioned);
    assert_eq!(env.reserved_task(&free).await, None);
    assert_eq!(env.sent().await, 0);
}

// ---------------------------------------------------------------- claiming ends the wake

/// After the woken session claims its task, the reservation is gone at once and the wake is
/// recorded `claimed`; a tick past the reservation's lapse records no `expired`.
async fn assert_claim_cleared(env: &Env, s: &Resolved, task: TaskId) {
    assert_eq!(env.reserved_task(s).await, None, "session reservable again");
    let session = env.store.get_session(s.session.id).await.unwrap().unwrap();
    assert_eq!(session.wake_task_id, None);
    let claimed = Count {
        outcome: Some(WakeOutcome::Claimed),
        session: Some(s.session.id),
        task: Some(task),
        ..Count::default()
    };
    assert_eq!(env.count(claimed).await, 1);
    env.advance(Duration::minutes(3));
    env.wake.tick().await.unwrap();
    let expired = Count {
        outcome: Some(WakeOutcome::Expired),
        ..Count::default()
    };
    assert_eq!(env.count(expired).await, 0);
    assert_eq!(env.count(claimed).await, 1);
}

#[tokio::test]
async fn claim_by_woken_session_clears_its_reservation() {
    let env = Env::new().await;
    let free = env.session_of(PA, "f").await;
    env.idle(&free).await;
    let task = env.human_task(PA, "now", true).await;
    assert_eq!(env.reserved_task(&free).await, Some(task.id));

    env.claim(&free, task.id).await;
    assert_claim_cleared(&env, &free, task.id).await;
}

#[tokio::test]
async fn claim_next_by_woken_session_clears_its_reservation() {
    let env = Env::new().await;
    let free = env.session_of(PA, "f").await;
    env.idle(&free).await;
    let task = env.human_task(PA, "now", true).await;
    assert_eq!(env.reserved_task(&free).await, Some(task.id));

    let claimed = env
        .tasks
        .claim_next(&free, task.project_id)
        .await
        .unwrap()
        .expect("claims the routed task");
    assert_eq!(claimed.id, task.id);
    assert_claim_cleared(&env, &free, task.id).await;
}
