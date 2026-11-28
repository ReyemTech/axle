#[macro_use]
mod common;

use std::path::Path;
use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{
    Actor, Address, AgentId, Eligibility, Importance, Message, MessageId, MessageKind, PlanState,
    ProjectId, ProjectWakeSettings, ServerSettings, SessionId, Task, TaskEvent, TaskEventKind,
    TaskId, TaskStatus, TurnState, WakeAttempt, WakeChannel, WakeKind, WakeOutcome, WakeTrigger,
};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::store::{
    AttemptQuery, ClaimRequest, Claimant, LeaseUpdate, Store, TaskFilter, TransitionRequest,
};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-28 9:00 UTC);
const PROJECT: &str = "github.com/reyemtech/axle";

async fn agent(store: &Arc<dyn Store>, clock: &TestClock) -> Resolved {
    IdentityService::resolve(
        &**store,
        clock,
        IdentityInput {
            project_key: Some(PROJECT.into()),
            host: "host".into(),
            harness: "claude-code".into(),
            session: SessionId::new(),
            token: common::token(&**store).await,
        },
    )
    .await
    .unwrap()
}

/// A task as the service would build it, for `insert_task`.
fn task_in(project: ProjectId, title: &str, now: OffsetDateTime) -> Task {
    Task {
        id: TaskId::new(),
        project_id: project,
        parent_id: None,
        title: title.into(),
        body: String::new(),
        status: TaskStatus::Open,
        owner_session_id: None,
        owner_agent_id: None,
        lease_expires_at: None,
        lease_epoch: 0,
        lease_ttl: None,
        eligibility: Eligibility::default(),
        created_by: Actor::Human,
        created_at: now,
        updated_at: now,
        done_at: None,
        acked_at: None,
        version: 0,
        requires_plan: false,
        plan_state: PlanState::None,
        sanctioned: false,
        request_depth: 0,
        origin_task_id: None,
        request_chain: Vec::new(),
        request_dedupe_key: None,
    }
}

async fn insert(store: &Arc<dyn Store>, task: &Task) {
    let event = TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: task.id,
        at: task.created_at,
        actor: task.created_by,
        kind: TaskEventKind::Created,
        note: None,
        payload: None,
    };
    let first = Message {
        id: MessageId::new(),
        project_id: Some(task.project_id),
        from: task.created_by,
        to: Address::Task(task.id),
        subject: Some(task.title.clone()),
        body: String::new(),
        importance: Importance::Normal,
        task_id: Some(task.id),
        reply_to: None,
        created_at: task.created_at,
        kind: MessageKind::Message,
    };
    assert!(
        store
            .insert_task(task, &[], None, &event, &first)
            .await
            .unwrap()
    );
}

store_matrix!(
    requested_task_round_trips,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(T0);
        let a = agent(&store, &clock).await;
        let project = a.project.as_ref().unwrap().id;
        let origin = task_in(project, "origin", T0);
        insert(&store, &origin).await;

        let mut requested = task_in(project, "please do this", T0);
        requested.status = TaskStatus::Requested;
        requested.request_depth = 1;
        requested.origin_task_id = Some(origin.id);
        requested.request_chain = vec![project];
        requested.request_dedupe_key = Some("k".into());
        insert(&store, &requested).await;

        let got = store.get_task(requested.id).await.unwrap().unwrap();
        assert_eq!(got, requested);
        assert!(!got.sanctioned);

        let listed = store
            .list_tasks(&TaskFilter {
                status: Some(TaskStatus::Requested),
                ..TaskFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(listed, vec![requested]);
    }
);

store_matrix!(
    sanctioned_and_chain_round_trip,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(T0);
        let a = agent(&store, &clock).await;
        let project = a.project.as_ref().unwrap().id;
        let other = store
            .upsert_project("github.com/reyemtech/other", "other", T0)
            .await
            .unwrap()
            .id;
        let mut t = task_in(project, "chained", T0);
        t.sanctioned = true;
        t.request_chain = vec![project, other];
        insert(&store, &t).await;
        assert_eq!(store.get_task(t.id).await.unwrap().unwrap(), t);
    }
);

store_matrix!(
    requested_is_not_claimable,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(T0);
        let a = agent(&store, &clock).await;
        let project = a.project.as_ref().unwrap().id;
        let mut requested = task_in(project, "please do this", T0);
        requested.status = TaskStatus::Requested;
        requested.request_depth = 1;
        insert(&store, &requested).await;

        let req = ClaimRequest {
            claimant: Claimant {
                agent: a.agent.id,
                session: a.session.id,
                token: a.session.token_id,
                project: Some(project),
                harness: "claude-code".into(),
                host: "host".into(),
            },
            now: T0 + Duration::minutes(1),
            live_since: T0,
            default_ttl: Duration::minutes(15),
            event_id: uuid::Uuid::now_v7(),
        };
        let attempt = store.claim_task(requested.id, &req).await.unwrap();
        assert!(attempt.claimed.is_none());
        assert_eq!(attempt.previous.unwrap().status, TaskStatus::Requested);
        assert!(
            store
                .claim_next_task(project, &req)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.get_task(requested.id).await.unwrap().unwrap().status,
            TaskStatus::Requested
        );
    }
);

store_matrix!(new_session_has_unknown_turn_and_no_channel, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock).await;
    let s = store.get_session(a.session.id).await.unwrap().unwrap();
    assert_eq!(s.turn_state, TurnState::Unknown);
    assert_eq!(s.turn_state_at, None);
    assert_eq!(s.wake_channel, WakeChannel::None);
    assert_eq!(s.wake_task_id, None);
    assert_eq!(s.wake_until, None);
    assert_eq!(s.last_woken_at, None);
    assert_eq!(s.run_id, None);
    let _ = clock.now();
});

store_matrix!(wake_settings_default_then_upsert, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock).await;
    let project = a.project.as_ref().unwrap().id;
    assert_eq!(
        store.wake_settings(project).await.unwrap(),
        ProjectWakeSettings::defaults(project)
    );
    let mut s = ProjectWakeSettings::defaults(project);
    s.auto_start = true;
    s.headless_enabled = true;
    s.headless_harness = Some("claude-code".into());
    s.max_concurrent_runs = 2;
    s.daily_run_cap = 9;
    s.max_run_minutes = 45;
    s.max_budget_usd = Some(2.5);
    store.put_wake_settings(&s, T0).await.unwrap();
    assert_eq!(store.wake_settings(project).await.unwrap(), s);
    s.auto_start = false;
    s.max_budget_usd = None;
    store
        .put_wake_settings(&s, T0 + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(store.wake_settings(project).await.unwrap(), s);
});

store_matrix!(server_settings_default_then_upsert, |store: Arc<
    dyn Store,
>| async move {
    assert_eq!(
        store.server_settings().await.unwrap(),
        ServerSettings::default()
    );
    let s = ServerSettings {
        wake_paused: true,
        headless_paused: false,
    };
    store.put_server_settings(&s, T0).await.unwrap();
    assert_eq!(store.server_settings().await.unwrap(), s);
    let s = ServerSettings {
        wake_paused: false,
        headless_paused: true,
    };
    store
        .put_server_settings(&s, T0 + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(store.server_settings().await.unwrap(), s);
});

fn attempt(
    session: SessionId,
    trigger: WakeTrigger,
    outcome: WakeOutcome,
    at: OffsetDateTime,
) -> WakeAttempt {
    WakeAttempt {
        id: uuid::Uuid::now_v7(),
        kind: WakeKind::Wake,
        trigger,
        task_id: None,
        session_id: Some(session),
        from_agent_id: None,
        runner_id: None,
        run_id: None,
        outcome,
        detail: None,
        pr_url: None,
        at,
    }
}

store_matrix!(attempt_counts_filter_by_window_and_trigger, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock).await;
    let session = a.session.id;
    for (trigger, at) in [
        (WakeTrigger::HumanPost, T0),
        (WakeTrigger::HumanPost, T0 + Duration::minutes(59)),
        (WakeTrigger::PeerPost, T0 + Duration::minutes(59)),
    ] {
        let mut row = attempt(session, trigger, WakeOutcome::Sent, at);
        if trigger == WakeTrigger::PeerPost {
            row.from_agent_id = Some(a.agent.id);
        }
        store.insert_wake_attempt(&row).await.unwrap();
    }
    // Not `sent`, and another kind: neither counts.
    store
        .insert_wake_attempt(&attempt(
            session,
            WakeTrigger::HumanPost,
            WakeOutcome::Failed,
            T0 + Duration::minutes(59),
        ))
        .await
        .unwrap();
    let mut headless = attempt(
        session,
        WakeTrigger::HumanPost,
        WakeOutcome::Sent,
        T0 + Duration::minutes(59),
    );
    headless.kind = WakeKind::Headless;
    store.insert_wake_attempt(&headless).await.unwrap();

    let q = AttemptQuery {
        kind: WakeKind::Wake,
        outcome: WakeOutcome::Sent,
        session: Some(session),
        task: None,
        from_agent: None,
        triggers: None,
        since: T0,
    };
    assert_eq!(store.count_wake_attempts(&q).await.unwrap(), 3);
    let since_930 = AttemptQuery {
        since: T0 + Duration::minutes(30),
        ..q.clone()
    };
    assert_eq!(store.count_wake_attempts(&since_930).await.unwrap(), 2);
    let peer_only = AttemptQuery {
        triggers: Some(vec![WakeTrigger::PeerPost]),
        since: T0 + Duration::minutes(30),
        ..q.clone()
    };
    assert_eq!(store.count_wake_attempts(&peer_only).await.unwrap(), 1);
    let by_agent = AttemptQuery {
        from_agent: Some(a.agent.id),
        ..q.clone()
    };
    assert_eq!(store.count_wake_attempts(&by_agent).await.unwrap(), 1);
    let other_session = AttemptQuery {
        session: Some(SessionId::new()),
        ..q
    };
    assert_eq!(store.count_wake_attempts(&other_session).await.unwrap(), 0);
});

/// A request created by `by` in `project` at `at`.
async fn request_by(
    store: &Arc<dyn Store>,
    by: &Resolved,
    project: ProjectId,
    key: &str,
    at: OffsetDateTime,
) -> Task {
    let mut t = task_in(project, key, at);
    t.status = TaskStatus::Requested;
    t.created_by = Actor::Agent(by.agent.id, by.session.id);
    t.request_depth = 1;
    t.request_dedupe_key = Some(key.into());
    insert(store, &t).await;
    t
}

fn transition(id: TaskId, version: Option<i64>, sanctioned: Option<bool>) -> TransitionRequest {
    TransitionRequest {
        task: id,
        from: vec![TaskStatus::Requested],
        to: Some(TaskStatus::Open),
        owner: None,
        lease: LeaseUpdate::Clear,
        clear_owner: false,
        now: T0 + Duration::minutes(1),
        token: None,
        event: TaskEvent {
            id: uuid::Uuid::now_v7(),
            task_id: id,
            at: T0 + Duration::minutes(1),
            actor: Actor::Human,
            kind: TaskEventKind::Released,
            note: None,
            payload: None,
        },
        plan_from: None,
        plan_to: None,
        message: None,
        latest_plan: None,
        archive_plans: false,
        version,
        sanctioned,
    }
}

/// Approves `id` (requested to open) through the store.
async fn approve(store: &Arc<dyn Store>, id: TaskId) {
    let done = store
        .transition_task(&transition(id, None, None))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.task.status, TaskStatus::Open);
}

store_matrix!(
    find_request_within_window,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(T0);
        let a = agent(&store, &clock).await;
        let project = a.project.as_ref().unwrap().id;
        let t = request_by(&store, &a, project, "k", T0).await;
        let window = Duration::minutes(10);
        // Looked up 9m59s later, the window reaches back to before the request.
        let found = store
            .find_request(
                a.agent.id,
                project,
                "k",
                T0 + Duration::seconds(599) - window,
            )
            .await
            .unwrap();
        assert_eq!(found.map(|f| f.id), Some(t.id));
        // At +10m the window starts after the request was created.
        assert!(
            store
                .find_request(a.agent.id, project, "k", T0 + Duration::seconds(1))
                .await
                .unwrap()
                .is_none()
        );
        // Wrong key, wrong project and wrong requester never match.
        assert!(
            store
                .find_request(a.agent.id, project, "other", T0)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .find_request(a.agent.id, ProjectId::new(), "k", T0)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .find_request(AgentId::new(), project, "k", T0)
                .await
                .unwrap()
                .is_none()
        );
        // Any status counts: an approved (open) request is still found.
        approve(&store, t.id).await;
        assert!(
            store
                .find_request(a.agent.id, project, "k", T0)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .count_requests_by(a.agent.id, T0 - Duration::hours(1))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .count_requests_by(a.agent.id, T0 + Duration::minutes(1))
                .await
                .unwrap(),
            0
        );
    }
);

store_matrix!(oldest_request_is_the_first_counted, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock).await;
    let project = a.project.as_ref().unwrap().id;
    let since = T0 - Duration::hours(1);
    assert_eq!(
        store.oldest_request_by(a.agent.id, since).await.unwrap(),
        None
    );
    request_by(&store, &a, project, "late", T0 + Duration::minutes(5)).await;
    request_by(&store, &a, project, "early", T0 + Duration::seconds(90)).await;
    // Plain tasks and other agents' requests are not counted.
    insert(&store, &task_in(project, "plain", T0)).await;
    let b = IdentityService::resolve(
        &*store,
        &clock,
        IdentityInput {
            project_key: Some(PROJECT.into()),
            host: "other-host".into(),
            harness: "claude-code".into(),
            session: SessionId::new(),
            token: common::token(&*store).await,
        },
    )
    .await
    .unwrap();
    assert_ne!(b.agent.id, a.agent.id);
    request_by(&store, &b, project, "theirs", T0).await;
    assert_eq!(
        store.oldest_request_by(a.agent.id, since).await.unwrap(),
        Some(T0 + Duration::seconds(90))
    );
    assert_eq!(
        store
            .oldest_request_by(a.agent.id, T0 + Duration::minutes(2))
            .await
            .unwrap(),
        Some(T0 + Duration::minutes(5))
    );
    assert_eq!(
        store
            .oldest_request_by(a.agent.id, T0 + Duration::minutes(6))
            .await
            .unwrap(),
        None
    );
});

store_matrix!(pending_requests_count_only_requested, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock).await;
    let project = a.project.as_ref().unwrap().id;
    assert_eq!(store.count_pending_requests(project).await.unwrap(), 0);
    let first = request_by(&store, &a, project, "one", T0).await;
    request_by(&store, &a, project, "two", T0).await;
    insert(&store, &task_in(project, "plain open", T0)).await;
    assert_eq!(store.count_pending_requests(project).await.unwrap(), 2);
    assert_eq!(
        store
            .count_pending_requests(ProjectId::new())
            .await
            .unwrap(),
        0
    );
    approve(&store, first.id).await;
    assert_eq!(store.count_pending_requests(project).await.unwrap(), 1);
});

store_matrix!(
    transition_guarded_by_version,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(T0);
        let a = agent(&store, &clock).await;
        let project = a.project.as_ref().unwrap().id;
        let t = request_by(&store, &a, project, "k", T0).await;
        assert!(!t.sanctioned);

        let stale = store
            .transition_task(&transition(t.id, Some(t.version + 5), Some(true)))
            .await
            .unwrap();
        assert!(stale.is_none());
        let unchanged = store.get_task(t.id).await.unwrap().unwrap();
        assert_eq!(unchanged.status, TaskStatus::Requested);
        assert!(!unchanged.sanctioned);

        let applied = store
            .transition_task(&transition(t.id, Some(t.version), Some(true)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(applied.task.status, TaskStatus::Open);
        assert!(applied.task.sanctioned);
        assert_eq!(applied.task.version, t.version + 1);
    }
);

store_matrix!(last_wake_sent_is_the_newest_sent_row, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock).await;
    let session = a.session.id;
    assert_eq!(store.last_wake_sent(session).await.unwrap(), None);

    let task = task_in(a.project.as_ref().unwrap().id, "t", T0);
    insert(&store, &task).await;
    let mut older = attempt(session, WakeTrigger::StartNow, WakeOutcome::Sent, T0);
    older.task_id = Some(task.id);
    let mut newer = attempt(
        session,
        WakeTrigger::HumanReply,
        WakeOutcome::Sent,
        T0 + Duration::minutes(1),
    );
    newer.from_agent_id = Some(a.agent.id);
    newer.detail = Some("d".into());
    // Not `sent`, another kind, another session: none of them count.
    let later_outcome = attempt(
        session,
        WakeTrigger::Queue,
        WakeOutcome::Expired,
        T0 + Duration::minutes(2),
    );
    let mut headless = attempt(
        session,
        WakeTrigger::Queue,
        WakeOutcome::Sent,
        T0 + Duration::minutes(3),
    );
    headless.kind = WakeKind::Headless;
    let other = agent(&store, &clock).await;
    let elsewhere = attempt(
        other.session.id,
        WakeTrigger::Queue,
        WakeOutcome::Sent,
        T0 + Duration::minutes(4),
    );
    for row in [&older, &newer, &later_outcome, &headless, &elsewhere] {
        store.insert_wake_attempt(row).await.unwrap();
    }
    assert_eq!(store.last_wake_sent(session).await.unwrap(), Some(newer));
    assert_eq!(
        store.last_wake_sent(other.session.id).await.unwrap(),
        Some(elsewhere)
    );
});

store_matrix!(append_task_event_leaves_the_task_alone, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock).await;
    let task = task_in(a.project.as_ref().unwrap().id, "t", T0);
    insert(&store, &task).await;
    let before = store.get_task(task.id).await.unwrap().unwrap();

    let event = TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: task.id,
        at: T0 + Duration::seconds(5),
        actor: Actor::Human,
        kind: TaskEventKind::Routed,
        note: Some("n".into()),
        payload: Some(serde_json::json!({ "session_id": a.session.id.to_string() })),
    };
    store.append_task_event(&event).await.unwrap();

    assert_eq!(store.get_task(task.id).await.unwrap().unwrap(), before);
    let events = store.task_events(task.id).await.unwrap();
    assert_eq!(
        events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![TaskEventKind::Created, TaskEventKind::Routed]
    );
    assert_eq!(events[1], event);
});

/// Copies every migration of `backend` before 0006 (wake-up) into a fresh directory.
fn old_migrations(backend: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("axle-mig-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if name.ends_with(".sql") && name.as_str() < "0006_" {
            std::fs::copy(entry.path(), dir.join(&name)).unwrap();
        }
    }
    dir
}

#[tokio::test]
async fn migrates_populated_tasks_table_sqlite() {
    use axle::store::sqlite::SqliteStore;
    use sqlx::Row;
    use sqlx::sqlite::SqlitePoolOptions;

    let path = std::env::temp_dir().join(format!("axle-test-{}.db", uuid::Uuid::now_v7()));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    let dir = old_migrations("sqlite");
    sqlx::migrate::Migrator::new(dir.as_path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();

    let project = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id, key, display_name, created_at)
         VALUES (?, 'p', 'p', '2026-11-01T00:00:00.000000Z')",
    )
    .bind(&project)
    .execute(&pool)
    .await
    .unwrap();
    let parent = uuid::Uuid::now_v7().to_string();
    let done = uuid::Uuid::now_v7().to_string();
    let open = uuid::Uuid::now_v7().to_string();
    for (id, parent_id, status, done_at, acked_at) in [
        (&parent, None, "open", None, None),
        (
            &done,
            Some(&parent),
            "done",
            Some("2026-11-20T09:00:00.000000Z"),
            Some("2026-11-21T09:00:00.000000Z"),
        ),
        (&open, Some(&parent), "open", None, None),
    ] {
        sqlx::query(
            "INSERT INTO tasks (id, project_id, parent_id, title, status, requires_plan,
                                done_at, acked_at, created_at, updated_at)
             VALUES (?, ?, ?, 't', ?, 1, ?, ?, '2026-11-01T00:00:00.000000Z',
                     '2026-11-01T00:00:00.000000Z')",
        )
        .bind(id)
        .bind(&project)
        .bind(parent_id)
        .bind(status)
        .bind(done_at)
        .bind(acked_at)
        .execute(&pool)
        .await
        .unwrap();
    }

    let store = SqliteStore::from_pool(pool.clone()).await.unwrap();
    let kept = store
        .get_task(done.parse::<uuid::Uuid>().unwrap().into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.status, TaskStatus::Done);
    assert_eq!(kept.parent_id.map(|p| p.to_string()), Some(parent.clone()));
    assert_eq!(kept.done_at, Some(datetime!(2026-11-20 9:00 UTC)));
    assert_eq!(kept.acked_at, Some(datetime!(2026-11-21 9:00 UTC)));
    assert!(kept.requires_plan);
    assert!(!kept.sanctioned);
    assert_eq!(kept.request_depth, 0);
    assert_eq!(kept.request_chain, Vec::<ProjectId>::new());
    assert_eq!(
        store
            .list_tasks(&TaskFilter::default())
            .await
            .unwrap()
            .len(),
        3
    );

    // The partial index and the new dedupe index survived the rebuild, and `requested` is
    // admitted by the new CHECK.
    let indexes: Vec<String> =
        sqlx::query("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'tasks'")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
    for want in [
        "tasks_project_status",
        "tasks_parent",
        "tasks_owner_session",
        "tasks_done",
        "tasks_request_dedupe",
    ] {
        assert!(
            indexes.iter().any(|i| i == want),
            "missing {want}: {indexes:?}"
        );
    }
    sqlx::query(
        "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at)
         VALUES (?, ?, 't', 'requested', '2026-11-01T00:00:00.000000Z',
                 '2026-11-01T00:00:00.000000Z')",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(&project)
    .execute(&pool)
    .await
    .unwrap();

    drop(store);
    let _ = std::fs::remove_dir_all(dir);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[cfg(feature = "pg-tests")]
#[tokio::test]
async fn migrates_populated_tasks_table_postgres() {
    use axle::store::postgres::PostgresStore;

    let schema = common::pg_bare_schema().await;
    let pool = schema.pool().await;
    let dir = old_migrations("postgres");
    let outcome = common::catch_body({
        let pool = pool.clone();
        let dir = dir.clone();
        async move {
            sqlx::migrate::Migrator::new(dir.as_path())
                .await
                .unwrap()
                .run(&pool)
                .await
                .unwrap();

            let project = uuid::Uuid::now_v7();
            sqlx::query(
                "INSERT INTO projects (id, key, display_name, created_at)
                 VALUES ($1, 'p', 'p', '2026-11-01T00:00:00Z')",
            )
            .bind(project)
            .execute(&pool)
            .await
            .unwrap();
            let done = uuid::Uuid::now_v7();
            sqlx::query(
                "INSERT INTO tasks (id, project_id, title, status, requires_plan, done_at,
                                    created_at, updated_at)
                 VALUES ($1, $2, 't', 'done', true, '2026-11-20T09:00:00Z',
                         '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z')",
            )
            .bind(done)
            .bind(project)
            .execute(&pool)
            .await
            .unwrap();

            let store = PostgresStore::from_pool(pool.clone()).await.unwrap();
            let kept = store.get_task(done.into()).await.unwrap().unwrap();
            assert_eq!(kept.status, TaskStatus::Done);
            assert_eq!(kept.done_at, Some(datetime!(2026-11-20 9:00 UTC)));
            assert!(kept.requires_plan);
            assert!(!kept.sanctioned);
            assert_eq!(kept.request_depth, 0);
            assert_eq!(kept.request_chain, Vec::<ProjectId>::new());
            sqlx::query(
                "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at)
                 VALUES ($1, $2, 't', 'requested', '2026-11-01T00:00:00Z',
                         '2026-11-01T00:00:00Z')",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(project)
            .execute(&pool)
            .await
            .unwrap();
        }
    })
    .await;
    pool.close().await;
    let _ = std::fs::remove_dir_all(dir);
    schema.drop().await;
    common::resume(outcome);
}
