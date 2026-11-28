//! Work requests: tasks another agent asks for, which the human approves or declines unless
//! the target project auto-starts them.

// Service tests run on SQLite only; `store_matrix!` is not needed here.
#[allow(unused_macros)]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::config::ServerConfig;
use axle::domain::{
    Actor, Eligibility, Importance, MessageKind, ProjectId, ProjectWakeSettings, RequestState,
    SessionId, Task, TaskEventKind, TaskId, TaskStatus, TurnState, WakeChannel, WakeKind,
    WakeOutcome,
};
use axle::events::{Event, EventBus};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{
    Caller, ClaimOutcome, NewRequest, NewTask, RequestOutcome, TaskError, TaskService,
};
use axle::service::wake::WakeService;
use axle::service::wake::guards::GuardError;
use axle::store::{AttemptQuery, Store};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use tokio::sync::broadcast;

const T0: OffsetDateTime = datetime!(2026-11-28 9:00 UTC);
const PROJECT: &str = "github.com/reyemtech/axle";
const OTHER: &str = "github.com/reyemtech/other";

struct Env {
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    tasks: Arc<TaskService>,
    events: broadcast::Receiver<Event>,
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

impl Env {
    async fn new() -> Self {
        let (store, db) = common::sqlite_store().await;
        let clock = Arc::new(TestClock::new(T0));
        let bus = EventBus::new(store.clone(), clock.clone());
        let events = bus.subscribe();
        let cfg = config();
        let wake =
            Arc::new(WakeService::new(store.clone(), clock.clone(), &cfg).with_events(bus.clone()));
        let tasks = Arc::new(
            TaskService::from_config(store.clone(), clock.clone(), &cfg)
                .with_events(bus)
                .with_wake(wake),
        );
        Self {
            store,
            clock,
            tasks,
            events,
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

    async fn request(
        &self,
        by: &Resolved,
        key: &str,
        title: &str,
    ) -> Result<RequestOutcome, TaskError> {
        let project_id = self.project(key).await;
        self.tasks
            .request(
                by,
                NewRequest {
                    project_id,
                    title: title.into(),
                    body: format!("please: {title}"),
                    eligibility: Eligibility::default(),
                    importance: Importance::Normal,
                    parent_id: None,
                },
            )
            .await
    }

    async fn get(&self, id: TaskId) -> Task {
        self.store.get_task(id).await.unwrap().unwrap()
    }

    async fn kinds(&self, id: TaskId) -> Vec<TaskEventKind> {
        self.store
            .task_events(id)
            .await
            .unwrap()
            .iter()
            .map(|e| e.kind)
            .collect()
    }

    async fn sent(&self) -> u32 {
        self.store
            .count_wake_attempts(&AttemptQuery {
                kind: WakeKind::Wake,
                outcome: WakeOutcome::Sent,
                session: None,
                task: None,
                from_agent: None,
                triggers: None,
                since: T0 - Duration::days(1),
            })
            .await
            .unwrap()
    }

    async fn reserved_task(&self, s: &Resolved) -> Option<TaskId> {
        let session = self.store.get_session(s.session.id).await.unwrap().unwrap();
        match session.wake_until {
            Some(until) if until > self.now() => session.wake_task_id,
            _ => None,
        }
    }

    /// The system notes in a task's thread (after its first message), as their bodies.
    async fn notes(&self, id: TaskId) -> Vec<String> {
        self.store
            .task_thread(id)
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.kind == MessageKind::System)
            .map(|m| {
                assert_eq!(m.from, Actor::Human, "the human decided");
                m.body
            })
            .collect()
    }

    fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        out
    }
}

#[tokio::test]
async fn request_in_ask_first_project_awaits_human() {
    let mut env = Env::new().await;
    let requester = env.session_of(OTHER, "r").await;
    let free = env.session_of(PROJECT, "b").await;
    env.idle(&free).await;

    let out = env
        .request(&requester, PROJECT, "Roll the CNPG users")
        .await
        .unwrap();
    assert_eq!(out.state, RequestState::AwaitingHuman);
    let task = env.get(out.task.id).await;
    assert_eq!(task, out.task);
    assert_eq!(task.status, TaskStatus::Requested);
    assert!(!task.sanctioned);
    assert_eq!(task.request_depth, 1);
    assert!(task.request_chain.is_empty());
    assert_eq!(task.origin_task_id, None);
    assert!(task.request_dedupe_key.is_some());
    assert_eq!(
        task.created_by,
        Actor::Agent(requester.agent.id, requester.session.id)
    );
    assert_eq!(
        env.kinds(task.id).await,
        vec![TaskEventKind::RequestCreated]
    );
    assert_eq!(env.sent().await, 0);
    assert_eq!(env.reserved_task(&free).await, None);
    let created: Vec<_> = env
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            Event::RequestCreated {
                request,
                project_id,
                state,
            } => Some((request, project_id, state)),
            _ => None,
        })
        .collect();
    assert_eq!(
        created,
        vec![(task.id, task.project_id, RequestState::AwaitingHuman)]
    );
}

#[tokio::test]
async fn request_in_auto_start_project_routes_now() {
    let mut env = Env::new().await;
    env.auto_start(PROJECT).await;
    let requester = env.session_of(OTHER, "r").await;
    let free = env.session_of(PROJECT, "b").await;
    env.idle(&free).await;

    let out = env
        .request(&requester, PROJECT, "Roll the CNPG users")
        .await
        .unwrap();
    assert_eq!(out.state, RequestState::Routed);
    let task = env.get(out.task.id).await;
    assert_eq!(task.status, TaskStatus::Open);
    assert!(task.sanctioned);
    assert_eq!(env.reserved_task(&free).await, Some(task.id));
    assert_eq!(env.sent().await, 1);
    let kinds = env.kinds(task.id).await;
    assert_eq!(kinds[0], TaskEventKind::RequestCreated);
    assert!(kinds.contains(&TaskEventKind::Routed));
    assert!(env.drain().iter().any(|e| matches!(
        e,
        Event::RequestCreated { request, state: RequestState::Routed, .. } if *request == task.id
    )));
}

#[tokio::test]
async fn auto_start_never_wakes_the_requester() {
    let env = Env::new().await;
    env.auto_start(PROJECT).await;
    // The requester works in the target project and is the only free session there.
    let requester = env.session_of(PROJECT, "r").await;
    env.idle(&requester).await;

    let out = env
        .request(&requester, PROJECT, "Tidy the README")
        .await
        .unwrap();
    assert_eq!(out.state, RequestState::Queued);
    assert_eq!(out.task.status, TaskStatus::Open);
    assert!(out.task.sanctioned);
    assert_eq!(env.reserved_task(&requester).await, None);
    assert_eq!(env.sent().await, 0);
    assert!(
        env.kinds(out.task.id)
            .await
            .contains(&TaskEventKind::Queued)
    );
}

#[tokio::test]
async fn dedupe_returns_existing_within_10_minutes() {
    let env = Env::new().await;
    let requester = env.session_of(OTHER, "r").await;

    let first = env
        .request(&requester, PROJECT, "Roll the CNPG users.")
        .await
        .unwrap();
    env.advance(Duration::minutes(9));
    let again = env
        .request(&requester, PROJECT, "  roll the  cnpg users")
        .await
        .unwrap();
    assert_eq!(again.task.id, first.task.id);
    assert_eq!(again.state, RequestState::AwaitingHuman);
    // Another sender is not folded in.
    let peer = env.session_of(OTHER, "p").await;
    let theirs = env
        .request(&peer, PROJECT, "Roll the CNPG users")
        .await
        .unwrap();
    assert_ne!(theirs.task.id, first.task.id);

    env.advance(Duration::minutes(2));
    let later = env
        .request(&requester, PROJECT, "Roll the CNPG users")
        .await
        .unwrap();
    assert_ne!(later.task.id, first.task.id);
    let pending = env
        .store
        .count_pending_requests(env.project(PROJECT).await)
        .await
        .unwrap();
    assert_eq!(pending, 3);
}

#[tokio::test]
async fn rate_limit_10_per_hour() {
    let env = Env::new().await;
    let requester = env.session_of(OTHER, "r").await;
    for i in 0..10 {
        env.request(&requester, PROJECT, &format!("job {i}"))
            .await
            .unwrap();
        env.advance(Duration::minutes(3));
    }
    // Now T0 + 30 min; add 30 s so the wait is 29.5 minutes, which rounds up.
    env.advance(Duration::seconds(30));
    let err = env
        .request(&requester, PROJECT, "job 10")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        TaskError::Guard(GuardError::TooMany { minutes: 30 })
    ));
    assert_eq!(
        err.to_string(),
        "too many requests; try again in 30 minutes"
    );
    // A dedupe hit is not a new request, so it is still answered.
    let hit = env.request(&requester, PROJECT, "job 9").await.unwrap();
    assert_eq!(hit.state, RequestState::AwaitingHuman);

    // Once the oldest leaves the hour, there is room for one more.
    env.advance(Duration::minutes(29) + Duration::seconds(31));
    assert_eq!(env.now(), T0 + Duration::hours(1) + Duration::seconds(1));
    env.request(&requester, PROJECT, "job 10").await.unwrap();
    let err = env
        .request(&requester, PROJECT, "job 11")
        .await
        .unwrap_err();
    // The oldest counted is now the one made at T0 + 3 min.
    assert_eq!(err.to_string(), "too many requests; try again in 3 minutes");
}

#[tokio::test]
async fn pending_cap_20() {
    let env = Env::new().await;
    let a = env.session_of(OTHER, "a").await;
    let b = env.session_of(OTHER, "b").await;
    let c = env.session_of(OTHER, "c").await;
    for i in 0..10 {
        env.request(&a, PROJECT, &format!("a {i}")).await.unwrap();
        env.request(&b, PROJECT, &format!("b {i}")).await.unwrap();
    }
    let err = env.request(&c, PROJECT, "one more").await.unwrap_err();
    assert!(matches!(err, TaskError::Guard(GuardError::ProjectFull(_))));
    assert_eq!(
        err.to_string(),
        format!("{PROJECT} already has 20 requests waiting for the human")
    );
    // Another project still takes requests.
    env.request(&c, "github.com/reyemtech/third", "one more")
        .await
        .unwrap();
}

#[tokio::test]
async fn depth_and_cycle_refused() {
    let env = Env::new().await;
    let (pa, pb, pc, pd) = (
        "github.com/acme/a",
        "github.com/acme/b",
        "github.com/acme/c",
        "github.com/acme/d",
    );
    let a = env.session_of(pa, "a").await;
    let b = env.session_of(pb, "b").await;
    let c = env.session_of(pc, "c").await;

    // The human's task in A (depth 0), held by a.
    let root = env
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: env.project(pa).await,
                title: "root".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        env.tasks.claim(&a, root.id).await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));

    // a → B: depth 1, chain [A].
    let to_b = env.request(&a, pb, "work in b").await.unwrap().task;
    assert_eq!(to_b.request_depth, 1);
    assert_eq!(to_b.request_chain, vec![root.project_id]);
    assert_eq!(to_b.origin_task_id, Some(root.id));
    let (to_b, _) = env
        .tasks
        .approve_request(to_b.id, to_b.version)
        .await
        .unwrap();
    assert!(matches!(
        env.tasks.claim(&b, to_b.id).await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));

    // b → A loops back.
    let err = env.request(&b, pa, "back to a").await.unwrap_err();
    assert!(matches!(err, TaskError::Guard(GuardError::Loop(_))));
    assert_eq!(err.to_string(), format!("request would loop back to {pa}"));

    // b → C: depth 2, chain [A, B].
    let to_c = env.request(&b, pc, "work in c").await.unwrap().task;
    assert_eq!(to_c.request_depth, 2);
    assert_eq!(to_c.request_chain, vec![root.project_id, to_b.project_id]);
    assert_eq!(to_c.origin_task_id, Some(to_b.id));
    let (to_c, _) = env
        .tasks
        .approve_request(to_c.id, to_c.version)
        .await
        .unwrap();
    assert!(matches!(
        env.tasks.claim(&c, to_c.id).await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));

    // c → D would be depth 3.
    let err = env.request(&c, pd, "work in d").await.unwrap_err();
    assert!(matches!(err, TaskError::Guard(GuardError::DepthLimit(2))));
    assert_eq!(err.to_string(), "request depth limit reached (2)");
    let pending = env
        .store
        .count_pending_requests(env.project(pd).await)
        .await
        .unwrap();
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn claim_of_requested_says_awaiting_approval() {
    let env = Env::new().await;
    let requester = env.session_of(OTHER, "r").await;
    let worker = env.session_of(PROJECT, "w").await;
    let out = env
        .request(&requester, PROJECT, "Roll the CNPG users")
        .await
        .unwrap();

    let claim = env.tasks.claim(&worker, out.task.id).await.unwrap();
    assert_eq!(claim, ClaimOutcome::AwaitingApproval);
    let next = env
        .tasks
        .claim_next(&worker, env.project(PROJECT).await)
        .await
        .unwrap();
    assert_eq!(next, None);
    assert_eq!(env.get(out.task.id).await.status, TaskStatus::Requested);
}

#[tokio::test]
async fn approve_routes_and_notes_thread() {
    let mut env = Env::new().await;
    let requester = env.session_of(OTHER, "r").await;
    let free = env.session_of(PROJECT, "b").await;
    env.idle(&free).await;
    let out = env
        .request(&requester, PROJECT, "Roll the CNPG users")
        .await
        .unwrap();
    env.drain();

    // A stale version is refused and changes nothing.
    let err = env
        .tasks
        .approve_request(out.task.id, out.task.version + 1)
        .await
        .unwrap_err();
    assert!(matches!(err, TaskError::RequestChanged));
    assert_eq!(err.to_string(), "this request changed — review it again");
    assert_eq!(env.get(out.task.id).await.status, TaskStatus::Requested);

    let (task, state) = env
        .tasks
        .approve_request(out.task.id, out.task.version)
        .await
        .unwrap();
    assert_eq!(state, RequestState::Routed);
    assert_eq!(task.status, TaskStatus::Open);
    assert!(task.sanctioned);
    assert_eq!(env.reserved_task(&free).await, Some(task.id));
    assert_eq!(env.sent().await, 1);
    let kinds = env.kinds(task.id).await;
    assert!(kinds.contains(&TaskEventKind::RequestApproved));
    assert!(kinds.contains(&TaskEventKind::Routed));
    assert_eq!(env.notes(task.id).await, vec!["routed to claude-code@b"]);
    assert!(env.drain().iter().any(|e| matches!(
        e,
        Event::RequestDecided { request, approved: true, .. } if *request == task.id
    )));

    // Approving it again: it is no longer a request.
    let err = env
        .tasks
        .approve_request(task.id, task.version)
        .await
        .unwrap_err();
    assert!(matches!(err, TaskError::RequestChanged));

    // With nobody free, the request is queued and the thread says so.
    let second = env
        .request(&requester, PROJECT, "Another job")
        .await
        .unwrap();
    let (_, state) = env
        .tasks
        .approve_request(second.task.id, second.task.version)
        .await
        .unwrap();
    assert_eq!(state, RequestState::Queued);
    assert_eq!(env.notes(second.task.id).await, vec!["queued"]);

    let err = env
        .tasks
        .approve_request(TaskId::new(), 0)
        .await
        .unwrap_err();
    assert!(matches!(err, TaskError::NotFound));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_and_decline_race_one_wins() {
    for _ in 0..5 {
        let env = Env::new().await;
        let requester = env.session_of(OTHER, "r").await;
        let free = env.session_of(PROJECT, "b").await;
        env.idle(&free).await;
        let out = env
            .request(&requester, PROJECT, "Roll the CNPG users")
            .await
            .unwrap();
        let (id, version) = (out.task.id, out.task.version);

        let approve = tokio::spawn({
            let tasks = env.tasks.clone();
            async move { tasks.approve_request(id, version).await.map(|_| ()) }
        });
        let decline = tokio::spawn({
            let tasks = env.tasks.clone();
            async move { tasks.decline_request(id, version, None).await.map(|_| ()) }
        });
        let (approve, decline) = tokio::join!(approve, decline);
        let results = [approve.unwrap(), decline.unwrap()];
        let won = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(won, 1, "{results:?}");
        assert!(
            results
                .iter()
                .any(|r| matches!(r, Err(TaskError::RequestChanged))),
            "{results:?}"
        );
        assert!(env.sent().await <= 1);
        let task = env.get(id).await;
        if results[0].is_ok() {
            assert_eq!(task.status, TaskStatus::Open);
            assert_eq!(env.sent().await, 1);
        } else {
            assert_eq!(task.status, TaskStatus::Cancelled);
            assert_eq!(env.sent().await, 0);
        }
    }
}

#[tokio::test]
async fn decline_cancels_without_wake() {
    let mut env = Env::new().await;
    let requester = env.session_of(OTHER, "r").await;
    let free = env.session_of(PROJECT, "b").await;
    env.idle(&free).await;
    let out = env
        .request(&requester, PROJECT, "Roll the CNPG users")
        .await
        .unwrap();
    env.drain();

    let task = env
        .tasks
        .decline_request(out.task.id, out.task.version, None)
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert!(!task.sanctioned);
    assert_eq!(env.sent().await, 0);
    assert_eq!(env.reserved_task(&free).await, None);
    assert!(
        env.kinds(task.id)
            .await
            .contains(&TaskEventKind::RequestDeclined)
    );
    assert_eq!(
        env.notes(task.id).await,
        vec!["The human declined this request."]
    );
    assert!(env.drain().iter().any(|e| matches!(
        e,
        Event::RequestDecided { request, approved: false, .. } if *request == task.id
    )));
    // Declined is final.
    let err = env
        .tasks
        .decline_request(task.id, task.version, None)
        .await
        .unwrap_err();
    assert!(matches!(err, TaskError::RequestChanged));

    // The human's own note replaces the default.
    let second = env
        .request(&requester, PROJECT, "Another job")
        .await
        .unwrap();
    env.tasks
        .decline_request(
            second.task.id,
            second.task.version,
            Some("  not this week "),
        )
        .await
        .unwrap();
    assert_eq!(env.notes(second.task.id).await, vec!["not this week"]);
}
