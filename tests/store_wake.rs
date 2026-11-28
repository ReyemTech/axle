#[macro_use]
mod common;

use std::path::Path;
use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{
    Actor, Address, Eligibility, Importance, Message, MessageId, MessageKind, PlanState, ProjectId,
    SessionId, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus, TurnState, WakeChannel,
};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::store::{ClaimRequest, Claimant, Store, TaskFilter};
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

/// Copies migrations 0001..=0004 of `backend` into a fresh directory.
fn old_migrations(backend: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("axle-mig-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if ["0001_", "0002_", "0003_", "0004_"]
            .iter()
            .any(|p| name.starts_with(p))
        {
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
