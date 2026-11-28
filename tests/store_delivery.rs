#[macro_use]
mod common;

use std::path::Path;
use std::sync::Arc;

use axle::clock::TestClock;
use axle::delivery::stages::CompletionTarget;
use axle::domain::{
    Actor, Address, DeliveryStage, DeploySource, Eligibility, Importance, Message, MessageId,
    MessageKind, PlanState, ProjectDelivery, ReleaseSource, Task, TaskEvent, TaskEventKind, TaskId,
    TaskStatus, WaitingOn, short_id,
};
use axle::domain::{
    DeliverySignal, PrLinkedBy, Project, SessionId, SignalKind, SignalOutcome, SignalSource,
};
use axle::service::delivery::{DeliveryError, DeliveryService};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{Caller, Completion, NewTask, TaskError, TaskService};
use axle::store::{
    CompleteRequest, DoneFilter, LeaseUpdate, LinkOutcome, LinkPrRequest, NewTaskPr, SignalInsert,
    StageMove, StageMoved, Store, StoreError, TaskFilter, TransitionRequest,
};
use time::macros::datetime;
use uuid::Uuid;

const T0: time::OffsetDateTime = datetime!(2026-11-28 9:00 UTC);

#[test]
fn short_id_is_uuid_tail() {
    let id: TaskId = "0193b2c4-7a10-7c3e-8b1d-9f1a4deae8e3".parse().unwrap();
    assert_eq!(short_id(id), "axl-4deae8e3");
    let upper: TaskId = "0193B2C4-7A10-7C3E-8B1D-9F1A4DEAE8E3".parse().unwrap();
    assert_eq!(short_id(upper), "axl-4deae8e3");
}

#[test]
fn delivery_stages_are_ordered() {
    let ranks: Vec<u8> = DeliveryStage::ALL.iter().map(|s| s.rank()).collect();
    assert_eq!(ranks, [1, 2, 3, 4]);
    assert!(DeliveryStage::InPr < DeliveryStage::Deployed);
    assert_eq!(DeliveryStage::InPr.label(), "In PR");
    for s in DeliveryStage::ALL {
        assert_eq!(s.as_str().parse::<DeliveryStage>().unwrap(), s);
    }
}

store_matrix!(delivering_round_trips, |store: Arc<dyn Store>| async move {
    let project = store
        .upsert_project("github.com/reyemtech/axle", "axle", T0)
        .await
        .unwrap();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id,
        parent_id: None,
        title: "ship it".into(),
        body: String::new(),
        status: TaskStatus::Delivering,
        owner_session_id: None,
        owner_agent_id: None,
        lease_expires_at: None,
        lease_epoch: 0,
        lease_ttl: None,
        eligibility: Eligibility::default(),
        created_by: Actor::from_ids(None, None),
        created_at: T0,
        updated_at: T0,
        done_at: None,
        acked_at: None,
        version: 0,
        requires_plan: false,
        plan_state: PlanState::None,
        block: None,
        delivery_stage: Some(DeliveryStage::Merged),
    };
    let event = TaskEvent {
        id: Uuid::now_v7(),
        task_id: task.id,
        at: T0,
        actor: Actor::from_ids(None, None),
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
        created_at: T0,
        kind: MessageKind::Message,
        options: Vec::new(),
    };
    assert!(
        store
            .insert_task(&task, &[], None, &event, &first)
            .await
            .unwrap()
    );
    let read = store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(read, task);
    assert_eq!(read.status, TaskStatus::Delivering);
    assert_eq!(read.delivery_stage, Some(DeliveryStage::Merged));
});

/// Copies the migrations of `backend` numbered up to and including 0007 into a fresh
/// directory, so a database can be built at the state before the delivery migration.
fn migrations_through_0007(backend: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("axle-mig-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if [
            "0001_", "0002_", "0003_", "0004_", "0005_", "0006_", "0007_",
        ]
        .iter()
        .any(|p| name.starts_with(p))
        {
            std::fs::copy(entry.path(), dir.join(&name)).unwrap();
        }
    }
    dir
}

/// The delivery migration rebuilds `tasks` on SQLite: a done task keeps `done_at` and
/// `acked_at`, a blocked task keeps its 0006 block detail, a 0007 link survives, and every
/// earlier index survives next to the new one.
#[tokio::test]
async fn migration_keeps_earlier_columns_sqlite() {
    use axle::store::sqlite::SqliteStore;
    use sqlx::Row;
    use sqlx::sqlite::SqlitePoolOptions;

    let path = std::env::temp_dir().join(format!("axle-test-{}.db", Uuid::now_v7()));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    let dir = migrations_through_0007("sqlite");
    sqlx::migrate::Migrator::new(dir.as_path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();

    let project = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id, key, display_name, created_at)
         VALUES (?, 'p', 'p', '2026-11-01T00:00:00.000000Z')",
    )
    .bind(&project)
    .execute(&pool)
    .await
    .unwrap();
    let done_id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at, done_at,
                            acked_at, requires_plan, plan_state)
         VALUES (?, ?, 't', 'done', '2026-11-20T00:00:00.000000Z', '2026-11-26T09:00:00.000000Z',
                 '2026-11-26T09:00:00.000000Z', '2026-11-27T09:00:00.000000Z', 1, 'approved')",
    )
    .bind(done_id.to_string())
    .bind(&project)
    .execute(&pool)
    .await
    .unwrap();
    // History referencing the task must survive the table rebuild.
    let event_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO task_events (id, task_id, kind, note, created_at)
         VALUES (?, ?, 'done', 'shipped', '2026-11-26T09:00:00.000000Z')",
    )
    .bind(&event_id)
    .bind(done_id.to_string())
    .execute(&pool)
    .await
    .unwrap();

    // A blocked task waiting on the done one (0006) and linked to it (0007).
    let blocked_id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at, block_reason,
                            blocked_at, waiting_kind, waiting_task_id, waiting_text, unblock_text)
         VALUES (?, ?, 'b', 'blocked', '2026-11-20T00:00:00.000000Z',
                 '2026-11-26T10:00:00.000000Z', 'needs the API', '2026-11-26T10:00:00.000000Z',
                 'task', ?, NULL, 'the API task ships')",
    )
    .bind(blocked_id.to_string())
    .bind(&project)
    .bind(done_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_links (task_id, target_id, kind, created_at)
         VALUES (?, ?, 'follow-up', '2026-11-26T10:00:00.000000Z')",
    )
    .bind(blocked_id.to_string())
    .bind(done_id.to_string())
    .execute(&pool)
    .await
    .unwrap();

    let mut before: Vec<String> = sqlx::query("SELECT name FROM pragma_index_list('tasks')")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();

    // Opening the store runs 0009.
    let store = SqliteStore::from_pool(pool.clone()).await.unwrap();
    let events = store.task_events(done_id).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.to_string(), event_id);
    assert_eq!(events[0].note.as_deref(), Some("shipped"));
    let dangling = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(
        dangling.is_empty(),
        "{} foreign key violations",
        dangling.len()
    );
    let done_index: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'tasks_done'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(done_index.contains("WHERE status = 'done'"), "{done_index}");
    let task = store.get_task(done_id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.done_at, Some(datetime!(2026-11-26 9:00 UTC)));
    assert_eq!(task.acked_at, Some(datetime!(2026-11-27 9:00 UTC)));
    assert!(task.requires_plan);
    assert_eq!(task.plan_state, PlanState::Approved);
    assert_eq!(task.delivery_stage, None);
    let blocked = store.get_task(blocked_id).await.unwrap().unwrap();
    assert_eq!(blocked.status, TaskStatus::Blocked);
    let block = blocked.block.expect("block detail survives the rebuild");
    assert_eq!(block.reason, "needs the API");
    assert_eq!(block.since, Some(datetime!(2026-11-26 10:00 UTC)));
    assert_eq!(block.waiting_on, Some(WaitingOn::Task(done_id)));
    assert_eq!(block.unblock.as_deref(), Some("the API task ships"));
    let links: Vec<(String, String, String)> =
        sqlx::query_as("SELECT task_id, target_id, kind FROM task_links")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        links,
        vec![(
            blocked_id.to_string(),
            done_id.to_string(),
            "follow-up".to_string()
        )]
    );
    // The CHECK on `waiting_kind` came through the rebuild.
    assert!(
        sqlx::query("UPDATE tasks SET waiting_kind = 'bogus' WHERE id = ?")
            .bind(blocked_id.to_string())
            .execute(&pool)
            .await
            .is_err()
    );
    let waiting_index: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'tasks_waiting_task'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        waiting_index.contains("WHERE waiting_task_id IS NOT NULL"),
        "{waiting_index}"
    );

    let mut indexes: Vec<String> = sqlx::query("SELECT name FROM pragma_index_list('tasks')")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    indexes.sort();
    before.push("tasks_delivery".to_string());
    before.sort();
    assert_eq!(indexes, before, "the rebuild keeps every index on tasks");
    // Every index on `tasks` after 0007 (plus the primary key's), and the new one.
    assert_eq!(
        indexes,
        [
            "sqlite_autoindex_tasks_1",
            "tasks_delivery",
            "tasks_done",
            "tasks_owner_session",
            "tasks_parent",
            "tasks_project_status",
            "tasks_waiting_task",
        ]
    );
    // A delivering task is now allowed; an unknown stage is not.
    sqlx::query("UPDATE tasks SET status = 'delivering', delivery_stage = 'merged' WHERE id = ?")
        .bind(done_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE tasks SET delivery_stage = 'bogus' WHERE id = ?")
            .bind(done_id.to_string())
            .execute(&pool)
            .await
            .is_err()
    );

    drop(store);
    pool.close().await;
    let _ = std::fs::remove_dir_all(dir);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[cfg(feature = "pg-tests")]
#[tokio::test]
async fn migration_keeps_earlier_columns_postgres() {
    use axle::store::postgres::PostgresStore;

    let schema = common::pg_bare_schema().await;
    let pool = schema.pool().await;
    let dir = migrations_through_0007("postgres");
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
            let project = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO projects (id, key, display_name, created_at)
                 VALUES ($1, 'p', 'p', '2026-11-01T00:00:00Z')",
            )
            .bind(project)
            .execute(&pool)
            .await
            .unwrap();
            let done_id = TaskId::new();
            sqlx::query(
                "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at,
                                    done_at, acked_at)
                 VALUES ($1, $2, 't', 'done', '2026-11-20T00:00:00Z', '2026-11-26T09:00:00Z',
                         '2026-11-26T09:00:00Z', '2026-11-27T09:00:00Z')",
            )
            .bind(done_id.as_uuid())
            .bind(project)
            .execute(&pool)
            .await
            .unwrap();

            let store = PostgresStore::from_pool(pool.clone()).await.unwrap();
            let task = store.get_task(done_id).await.unwrap().unwrap();
            assert_eq!(task.status, TaskStatus::Done);
            assert_eq!(task.done_at, Some(datetime!(2026-11-26 9:00 UTC)));
            assert_eq!(task.acked_at, Some(datetime!(2026-11-27 9:00 UTC)));
            assert_eq!(task.delivery_stage, None);

            sqlx::query(
                "UPDATE tasks SET status = 'delivering', delivery_stage = 'merged'
                  WHERE id = $1",
            )
            .bind(done_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
            assert!(
                sqlx::query("UPDATE tasks SET delivery_stage = 'bogus' WHERE id = $1")
                    .bind(done_id.as_uuid())
                    .execute(&pool)
                    .await
                    .is_err()
            );
            let indexed: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_indexes
                  WHERE schemaname = current_schema() AND tablename = 'tasks'
                    AND indexname IN ('tasks_project_status', 'tasks_parent',
                                      'tasks_owner_session', 'tasks_done', 'tasks_delivery')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(indexed, 5);
        }
    })
    .await;
    pool.close().await;
    let _ = std::fs::remove_dir_all(dir);
    schema.drop().await;
    common::resume(outcome);
}

const T1: time::OffsetDateTime = datetime!(2026-11-28 10:00 UTC);
const T2: time::OffsetDateTime = datetime!(2026-11-28 11:00 UTC);

/// Inserts a `delivering` task at `stage` and, when `pr` is set, its `task_prs` row as PR
/// `#pr` (raw SQL: nothing writes `task_prs` through the store yet).
async fn delivering_task(
    store: &dyn Store,
    raw: &common::RawDb,
    project: axle::domain::ProjectId,
    stage: DeliveryStage,
    pr: Option<i64>,
) -> TaskId {
    let task = Task {
        id: TaskId::new(),
        project_id: project,
        parent_id: None,
        title: format!("at {}", stage.as_str()),
        body: String::new(),
        status: TaskStatus::Delivering,
        owner_session_id: None,
        owner_agent_id: None,
        lease_expires_at: None,
        lease_epoch: 0,
        lease_ttl: None,
        eligibility: Eligibility::default(),
        created_by: Actor::from_ids(None, None),
        created_at: T0,
        updated_at: T0,
        done_at: None,
        acked_at: None,
        version: 0,
        requires_plan: false,
        plan_state: PlanState::None,
        block: None,
        delivery_stage: Some(stage),
    };
    let event = TaskEvent {
        id: Uuid::now_v7(),
        task_id: task.id,
        at: T0,
        actor: Actor::from_ids(None, None),
        kind: TaskEventKind::Created,
        note: None,
        payload: None,
    };
    let first = Message {
        id: MessageId::new(),
        project_id: Some(project),
        from: task.created_by,
        to: Address::Task(task.id),
        subject: Some(task.title.clone()),
        body: String::new(),
        importance: Importance::Normal,
        task_id: Some(task.id),
        reply_to: None,
        created_at: T0,
        kind: MessageKind::Message,
        options: Vec::new(),
    };
    assert!(
        store
            .insert_task(&task, &[], None, &event, &first)
            .await
            .unwrap()
    );
    if let Some(n) = pr {
        raw.execute(&format!(
            "INSERT INTO task_prs (task_id, project_id, repo, number, url, linked_by)
             VALUES ('{}', '{project}', 'reyemtech/axle', {n},
                     'https://github.com/reyemtech/axle/pull/{n}', 'agent')",
            task.id
        ))
        .await;
    }
    // The task's own stage lives in `task_stages`.
    reach_stage(raw, task.id, stage).await;
    task.id
}

/// Saves `project`'s default settings with `stages`, returning the tasks the recompute moved.
async fn settings(
    store: &dyn Store,
    project: &axle::domain::Project,
    stages: &[DeliveryStage],
    at: time::OffsetDateTime,
) -> Vec<Task> {
    let mut d = ProjectDelivery::defaults(project, at);
    d.stages = stages.to_vec();
    store
        .set_project_delivery(&d, None, None)
        .await
        .unwrap()
        .moved
}

fn delivered_notes(events: &[TaskEvent]) -> Vec<Option<String>> {
    events
        .iter()
        .filter(|e| e.kind == TaskEventKind::Delivered)
        .map(|e| e.note.clone())
        .collect()
}

store_matrix!(
    delivery_settings_default_absent,
    |store: Arc<dyn Store>| async move {
        let project = store
            .upsert_project("github.com/reyemtech/axle", "axle", T0)
            .await
            .unwrap();
        assert_eq!(store.get_project_delivery(project.id).await.unwrap(), None);
        assert!(store.list_project_delivery().await.unwrap().is_empty());
    }
);

store_matrix!(
    delivery_settings_round_trip,
    |store: Arc<dyn Store>| async move {
        let project = store
            .upsert_project("github.com/reyemtech/axle", "axle", T0)
            .await
            .unwrap();
        let other = store
            .upsert_project("gitlab.com/acme/tool", "tool", T0)
            .await
            .unwrap();
        let mut d = ProjectDelivery::defaults(&project, T1);
        d.stages = vec![DeliveryStage::InPr, DeliveryStage::Released];
        d.release_source = ReleaseSource::GithubTags;
        d.tag_pattern = Some("web-v*".into());
        d.deploy_source = DeploySource::None;
        d.required_environment = "prod-eu".into();
        assert!(
            store
                .set_project_delivery(&d, None, None)
                .await
                .unwrap()
                .moved
                .is_empty()
        );
        assert_eq!(
            store.get_project_delivery(project.id).await.unwrap(),
            Some(d.clone())
        );

        // The upsert replaces every field.
        let mut changed = d.clone();
        changed.stages = Vec::new();
        changed.repo = None;
        changed.github_enabled = false;
        changed.release_source = ReleaseSource::Generic;
        changed.tag_pattern = None;
        changed.deploy_source = DeploySource::Generic;
        changed.required_environment = "staging".into();
        changed.updated_at = T2;
        store
            .set_project_delivery(&changed, None, None)
            .await
            .unwrap();
        assert_eq!(
            store.get_project_delivery(project.id).await.unwrap(),
            Some(changed.clone())
        );

        let o = ProjectDelivery::defaults(&other, T1);
        store.set_project_delivery(&o, None, None).await.unwrap();
        let mut all = store.list_project_delivery().await.unwrap();
        all.sort_by_key(|d| d.project_id.to_string());
        let mut want = vec![changed, o];
        want.sort_by_key(|d| d.project_id.to_string());
        assert_eq!(all, want);
    }
);

store_matrix_raw!(
    stages_off_recomputes_delivering,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let project = store
            .upsert_project("github.com/reyemtech/axle", "axle", T0)
            .await
            .unwrap();
        assert!(
            settings(&*store, &project, &[InPr, Merged, Released], T0)
                .await
                .is_empty()
        );
        let at_pr = delivering_task(&*store, &raw, project.id, InPr, Some(12)).await;
        let at_merged = delivering_task(&*store, &raw, project.id, Merged, Some(13)).await;

        let moved = settings(&*store, &project, &[InPr, Merged], T1).await;
        assert_eq!(moved.iter().map(|t| t.id).collect::<Vec<_>>(), [at_merged]);
        let merged = store.get_task(at_merged).await.unwrap().unwrap();
        assert_eq!(moved[0], merged);
        assert_eq!(merged.status, TaskStatus::Done);
        assert_eq!(merged.done_at, Some(T1));
        assert_eq!(merged.updated_at, T1);
        assert_eq!(merged.version, 1);
        assert_eq!(merged.delivery_stage, Some(Merged));
        let events = store.task_events(at_merged).await.unwrap();
        assert_eq!(
            delivered_notes(&events),
            [Some("Merged is now the last delivery stage".to_string())]
        );
        let delivered = events.last().unwrap();
        assert_eq!(delivered.at, T1);
        assert_eq!(delivered.actor, Actor::from_ids(None, None));

        let pr = store.get_task(at_pr).await.unwrap().unwrap();
        assert_eq!(pr.status, TaskStatus::Delivering);
        assert_eq!(pr.done_at, None);
        assert!(delivered_notes(&store.task_events(at_pr).await.unwrap()).is_empty());

        let moved = settings(&*store, &project, &[], T2).await;
        assert_eq!(moved.iter().map(|t| t.id).collect::<Vec<_>>(), [at_pr]);
        let pr = store.get_task(at_pr).await.unwrap().unwrap();
        assert_eq!(pr.status, TaskStatus::Done);
        assert_eq!(pr.done_at, Some(T2));
        assert_eq!(
            delivered_notes(&store.task_events(at_pr).await.unwrap()),
            [Some("delivery stages turned off".to_string())]
        );
        // The task already done is untouched.
        assert_eq!(store.get_task(at_merged).await.unwrap().unwrap(), merged);
    }
);

store_matrix_raw!(
    enabling_stages_reopens_nothing,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let project = store
            .upsert_project("github.com/reyemtech/axle", "axle", T0)
            .await
            .unwrap();
        settings(&*store, &project, &[InPr, Merged, Released], T0).await;
        let id = delivering_task(&*store, &raw, project.id, Merged, Some(12)).await;
        assert_eq!(
            settings(&*store, &project, &[InPr, Merged], T1).await.len(),
            1
        );
        let done = store.get_task(id).await.unwrap().unwrap();
        assert_eq!(done.done_at, Some(T1));

        assert!(
            settings(&*store, &project, &[InPr, Merged, Released, Deployed], T2)
                .await
                .is_empty()
        );
        assert_eq!(store.get_task(id).await.unwrap().unwrap(), done);
    }
);

// ---- PR links, completion and the parent rule ----

const PROJECT: &str = "github.com/reyemtech/axle";
const T3: time::OffsetDateTime = datetime!(2026-11-28 12:00 UTC);

/// A project with one agent session that creates and claims tasks through the service.
struct Fx {
    store: Arc<dyn Store>,
    svc: TaskService,
    owner: Resolved,
    project: Project,
}

impl Fx {
    async fn new(store: Arc<dyn Store>) -> Self {
        let clock = Arc::new(TestClock::new(T0));
        let svc = TaskService::new(
            store.clone(),
            clock.clone(),
            time::Duration::minutes(15),
            time::Duration::hours(2),
            time::Duration::seconds(90),
        );
        let owner = IdentityService::resolve(
            &*store,
            &*clock,
            IdentityInput {
                project_key: Some(PROJECT.into()),
                host: "mac".into(),
                harness: "claude-code".into(),
                session: SessionId::new(),
                token: common::token(&*store).await,
            },
        )
        .await
        .unwrap();
        let project = owner.project.clone().unwrap();
        Fx {
            store,
            svc,
            owner,
            project,
        }
    }

    /// A task (a child of `parent` when set) claimed by the owner.
    async fn claimed(&self, title: &str, parent: Option<TaskId>) -> Task {
        let task = self
            .svc
            .create(
                Caller::Agent(&self.owner),
                NewTask {
                    project_id: self.project.id,
                    parent_id: parent,
                    title: title.into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();
        self.svc.claim(&self.owner, task.id).await.unwrap();
        self.store.get_task(task.id).await.unwrap().unwrap()
    }

    async fn stages(&self, stages: &[DeliveryStage]) -> Vec<Task> {
        settings(&*self.store, &self.project, stages, T0).await
    }

    fn link_req(&self, task: TaskId, n: i64, replace: bool) -> LinkPrRequest {
        LinkPrRequest {
            task,
            pr: pr(n, PrLinkedBy::Agent),
            actor: Actor::Agent(self.owner.agent.id, self.owner.session.id),
            token: Some(self.owner.session.token_id),
            now: T1,
            replace,
        }
    }

    async fn link(&self, task: TaskId, n: i64) -> LinkOutcome {
        self.store
            .link_pr(&self.link_req(task, n, true))
            .await
            .unwrap()
    }

    /// The owner's `task_complete` write at `at`, with `link` from its links.
    fn complete_req(
        &self,
        task: TaskId,
        at: time::OffsetDateTime,
        link: Option<NewTaskPr>,
    ) -> CompleteRequest {
        CompleteRequest {
            transition: TransitionRequest {
                task,
                from: vec![TaskStatus::Claimed, TaskStatus::Review],
                to: Some(TaskStatus::Done),
                owner: Some(self.owner.session.id),
                lease: LeaseUpdate::Clear,
                clear_owner: false,
                now: at,
                token: Some(self.owner.session.token_id),
                event: TaskEvent {
                    id: Uuid::now_v7(),
                    task_id: task,
                    at,
                    actor: Actor::Agent(self.owner.agent.id, self.owner.session.id),
                    kind: TaskEventKind::Done,
                    note: Some("shipped".into()),
                    payload: Some(serde_json::json!({ "summary": "shipped" })),
                },
                plan_from: None,
                plan_to: None,
                message: None,
                latest_plan: None,
                archive_plans: false,
                block: None,
                unblock: None,
            },
            link,
        }
    }

    async fn complete(&self, task: TaskId, at: time::OffsetDateTime) -> (Task, CompletionTarget) {
        let done = self
            .store
            .complete_task(&self.complete_req(task, at, None))
            .await
            .unwrap()
            .expect("completion applies");
        (done.transitioned.task, done.target)
    }

    /// A human-driven `delivering → done` (what Mark done does), through `transition_task`.
    async fn deliver(&self, task: TaskId, at: time::OffsetDateTime) -> Task {
        self.store
            .transition_task(&TransitionRequest {
                task,
                from: vec![TaskStatus::Delivering],
                to: Some(TaskStatus::Done),
                owner: None,
                lease: LeaseUpdate::Keep,
                clear_owner: false,
                now: at,
                token: None,
                event: TaskEvent {
                    id: Uuid::now_v7(),
                    task_id: task,
                    at,
                    actor: Actor::Human,
                    kind: TaskEventKind::Delivered,
                    note: Some("marked done by the human".into()),
                    payload: None,
                },
                plan_from: None,
                plan_to: None,
                message: None,
                latest_plan: None,
                archive_plans: false,
                block: None,
                unblock: None,
            })
            .await
            .unwrap()
            .expect("delivering task")
            .task
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
}

fn pr(n: i64, linked_by: PrLinkedBy) -> NewTaskPr {
    NewTaskPr {
        repo: "reyemtech/axle".into(),
        number: Some(n),
        url: format!("https://github.com/reyemtech/axle/pull/{n}"),
        linked_by,
    }
}

/// What a signal moving the task's own PR to `stage` writes (raw: `advance_stage` is later):
/// the `task_stages` row, the task's own stage, and `delivery_stage`.
async fn set_stage(raw: &common::RawDb, task: TaskId, stage: DeliveryStage) {
    raw.execute(&format!(
        "UPDATE tasks SET delivery_stage = '{stage}' WHERE id = '{task}'"
    ))
    .await;
    reach_stage(raw, task, stage).await;
}

/// Records only the `task_stages` row (the task's own stage), leaving `delivery_stage`.
async fn reach_stage(raw: &common::RawDb, task: TaskId, stage: DeliveryStage) {
    raw.execute(&format!(
        "INSERT INTO task_stages (task_id, stage, reached_at)
         VALUES ('{task}', '{stage}', '2026-11-28T09:30:00.000000Z')
         ON CONFLICT (task_id, stage) DO NOTHING"
    ))
    .await;
}

fn last_of(events: &[TaskEvent], kind: TaskEventKind) -> TaskEvent {
    events
        .iter()
        .rev()
        .find(|e| e.kind == kind)
        .cloned()
        .unwrap_or_else(|| panic!("no {kind} event"))
}

store_matrix!(link_and_relink, |store: Arc<dyn Store>| async move {
    let fx = Fx::new(store).await;
    let task = fx.claimed("ship", None).await;

    let LinkOutcome::Linked(linked) = fx.link(task.id, 12).await else {
        panic!("expected Linked");
    };
    assert_eq!(linked.delivery_stage, Some(DeliveryStage::InPr));
    assert_eq!(linked.version, task.version + 1);
    assert_eq!(linked.updated_at, T1);
    assert_eq!(linked, fx.get(task.id).await);
    let events = fx.store.task_events(task.id).await.unwrap();
    let ev = last_of(&events, TaskEventKind::PrLinked);
    assert_eq!(
        ev.payload,
        Some(serde_json::json!({ "url": "https://github.com/reyemtech/axle/pull/12" }))
    );
    let stored = fx.store.task_pr(task.id).await.unwrap().unwrap();
    assert_eq!(stored.number, Some(12));
    assert_eq!(stored.linked_by, PrLinkedBy::Agent);
    assert_eq!(stored.project_id, fx.project.id);

    // The same PR again changes nothing.
    assert_eq!(
        fx.link(task.id, 12).await,
        LinkOutcome::Unchanged(linked.clone())
    );

    let LinkOutcome::Relinked {
        task: relinked,
        previous,
    } = fx.link(task.id, 13).await
    else {
        panic!("expected Relinked");
    };
    assert_eq!(previous, stored);
    assert_eq!(relinked.delivery_stage, Some(DeliveryStage::InPr));
    assert_eq!(relinked.version, linked.version + 1);
    let ev = last_of(
        &fx.store.task_events(task.id).await.unwrap(),
        TaskEventKind::PrRelinked,
    );
    assert_eq!(
        ev.payload,
        Some(serde_json::json!({
            "url": "https://github.com/reyemtech/axle/pull/13",
            "previous_url": "https://github.com/reyemtech/axle/pull/12",
        }))
    );
    assert_eq!(
        fx.store.task_pr(task.id).await.unwrap().unwrap().number,
        Some(13)
    );
    let stages = fx.store.task_stages(task.id).await.unwrap();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].stage, DeliveryStage::InPr);
    assert_eq!(stages[0].reached_at, T1);
    assert!(!stages[0].implied);
    // The old PR is free again.
    let other = fx.claimed("other", None).await;
    assert!(matches!(
        fx.link(other.id, 12).await,
        LinkOutcome::Linked(_)
    ));
    let both = fx.store.task_prs(&[task.id, other.id]).await.unwrap();
    assert_eq!(both.len(), 2);
});

store_matrix!(link_uniqueness, |store: Arc<dyn Store>| async move {
    let fx = Fx::new(store).await;
    let a = fx.claimed("a", None).await;
    let b = fx.claimed("b", None).await;
    assert!(matches!(fx.link(a.id, 12).await, LinkOutcome::Linked(_)));
    assert_eq!(fx.link(b.id, 12).await, LinkOutcome::TakenBy(a.id));
    // The same URL without a number is the same PR.
    let mut req = fx.link_req(b.id, 12, true);
    req.pr.number = None;
    assert_eq!(
        fx.store.link_pr(&req).await.unwrap(),
        LinkOutcome::TakenBy(a.id)
    );
    assert_eq!(fx.store.task_pr(b.id).await.unwrap(), None);
    assert_eq!(fx.get(b.id).await, b);

    // Done tasks are closed to links; unknown tasks are not found.
    fx.complete(a.id, T2).await;
    assert_eq!(
        fx.link(a.id, 14).await,
        LinkOutcome::Closed(TaskStatus::Done)
    );
    assert_eq!(fx.link(TaskId::new(), 15).await, LinkOutcome::NotFound);
});

store_matrix!(
    link_race_has_one_winner,
    |store: Arc<dyn Store>| async move {
        let fx = Fx::new(store).await;
        let a = fx.claimed("a", None).await;
        let b = fx.claimed("b", None).await;
        let (ra, rb) = (fx.link_req(a.id, 40, true), fx.link_req(b.id, 40, true));
        let (x, y) = tokio::join!(fx.store.link_pr(&ra), fx.store.link_pr(&rb));
        let outcomes = [x.unwrap(), y.unwrap()];
        let linked = outcomes
            .iter()
            .filter(|o| matches!(o, LinkOutcome::Linked(_)))
            .count();
        let taken = outcomes
            .iter()
            .filter(|o| matches!(o, LinkOutcome::TakenBy(_)))
            .count();
        assert_eq!((linked, taken), (1, 1), "{outcomes:?}");
    }
);

store_matrix!(
    subtask_link_and_unlink_racing_a_heartbeat_do_not_deadlock,
    |store: Arc<dyn Store>| async move {
        let fx = Fx::new(store).await;
        fx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
            .await;
        let ttl = time::Duration::minutes(15);
        for round in 0..10 {
            // One session holds both: the heartbeat updates the parent and the child in one
            // statement, while linking the child locks the child, then its parent.
            let parent = fx.claimed("parent", None).await;
            let child = fx.claimed("child", Some(parent.id)).await;
            let req = fx.link_req(child.id, 300 + round, true);
            let session = fx.owner.session.id;
            let (linked, renewed) = tokio::join!(
                fx.store.link_pr(&req),
                fx.store.extend_session_leases(session, T0, ttl)
            );
            let linked = linked.unwrap_or_else(|e| panic!("round {round}: link: {e}"));
            assert!(matches!(linked, LinkOutcome::Linked(_)), "{linked:?}");
            renewed.unwrap_or_else(|e| panic!("round {round}: heartbeat: {e}"));
            let (unlinked, renewed) = tokio::join!(
                fx.store.unlink_pr(child.id, Actor::Human, None, T1),
                fx.store.extend_session_leases(session, T0, ttl)
            );
            let unlinked = unlinked.unwrap_or_else(|e| panic!("round {round}: unlink: {e}"));
            assert!(unlinked.is_some());
            renewed.unwrap_or_else(|e| panic!("round {round}: heartbeat: {e}"));
        }
    }
);

store_matrix_raw!(
    relink_refused_after_merge,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        let fx = Fx::new(store).await;
        let task = fx.claimed("ship", None).await;
        fx.link(task.id, 12).await;
        set_stage(&raw, task.id, DeliveryStage::Merged).await;
        assert_eq!(
            fx.link(task.id, 13).await,
            LinkOutcome::AlreadyMerged { number: Some(12) }
        );
        assert_eq!(
            fx.store.task_pr(task.id).await.unwrap().unwrap().number,
            Some(12)
        );
    }
);

store_matrix!(
    fallback_never_replaces,
    |store: Arc<dyn Store>| async move {
        let fx = Fx::new(store).await;
        let task = fx.claimed("ship", None).await;
        fx.link(task.id, 12).await;
        let mut req = fx.link_req(task.id, 13, false);
        req.pr.linked_by = PrLinkedBy::BranchMatch;
        assert_eq!(fx.store.link_pr(&req).await.unwrap(), LinkOutcome::HasLink);
        assert_eq!(
            fx.store.task_pr(task.id).await.unwrap().unwrap().number,
            Some(12)
        );
        // Without a PR the fallback links.
        let other = fx.claimed("other", None).await;
        let mut req = fx.link_req(other.id, 13, false);
        req.pr.linked_by = PrLinkedBy::BranchMatch;
        assert!(matches!(
            fx.store.link_pr(&req).await.unwrap(),
            LinkOutcome::Linked(_)
        ));
        assert_eq!(
            fx.store.task_pr(other.id).await.unwrap().unwrap().linked_by,
            PrLinkedBy::BranchMatch
        );
    }
);

store_matrix_raw!(
    unlink_rules,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged]).await;
        let unlink = |id: TaskId| {
            let store = fx.store.clone();
            async move { store.unlink_pr(id, Actor::Human, None, T3).await.unwrap() }
        };

        let delivering = fx.claimed("delivering", None).await;
        fx.link(delivering.id, 12).await;
        fx.complete(delivering.id, T2).await;
        assert_eq!(unlink(delivering.id).await, None);
        assert!(fx.store.task_pr(delivering.id).await.unwrap().is_some());

        let merged = fx.claimed("merged", None).await;
        fx.link(merged.id, 13).await;
        set_stage(&raw, merged.id, Merged).await;
        assert_eq!(unlink(merged.id).await, None);

        let review = fx.claimed("review", None).await;
        fx.link(review.id, 14).await;
        fx.svc.to_review(&fx.owner, review.id, None).await.unwrap();
        let before = fx.get(review.id).await;
        let pr = fx.store.task_pr(review.id).await.unwrap().unwrap();
        assert_eq!(unlink(review.id).await, Some(pr));
        let after = fx.get(review.id).await;
        assert_eq!(after.status, TaskStatus::Review);
        assert_eq!(after.delivery_stage, None);
        assert_eq!(after.version, before.version + 1);
        assert_eq!(after.updated_at, T3);
        assert_eq!(fx.store.task_pr(review.id).await.unwrap(), None);
        let ev = last_of(
            &fx.store.task_events(review.id).await.unwrap(),
            TaskEventKind::PrUnlinked,
        );
        assert_eq!(ev.actor, Actor::Human);
        assert_eq!(
            ev.payload,
            Some(serde_json::json!({
                "previous_url": "https://github.com/reyemtech/axle/pull/14",
            }))
        );
        // Nothing left to unlink.
        assert_eq!(unlink(review.id).await, None);
    }
);

store_matrix!(complete_without_stages_is_unchanged, |store: Arc<
    dyn Store,
>| async move {
    let fx = Fx::new(store).await;
    let task = fx.claimed("ship", None).await;
    let (done, target) = fx.complete(task.id, T2).await;
    assert_eq!(target, CompletionTarget::Done);
    assert_eq!(done.status, TaskStatus::Done);
    assert_eq!(done.done_at, Some(T2));
    assert_eq!(done.updated_at, T2);
    assert_eq!(done.version, task.version + 1);
    assert_eq!(done.lease_expires_at, None);
    assert_eq!(done.owner_session_id, Some(fx.owner.session.id));
    assert_eq!(done.delivery_stage, None);
    assert_eq!(done, fx.get(task.id).await);
    let ev = last_of(
        &fx.store.task_events(task.id).await.unwrap(),
        TaskEventKind::Done,
    );
    assert_eq!(ev.note.as_deref(), Some("shipped"));
    assert_eq!(
        ev.payload,
        Some(serde_json::json!({ "summary": "shipped" }))
    );
    // The guard still holds: a done task does not complete again.
    assert!(
        fx.store
            .complete_task(&fx.complete_req(task.id, T3, None))
            .await
            .unwrap()
            .is_none()
    );
});

store_matrix_raw!(
    complete_table,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged]).await;

        // Stages on, no PR: the task skips the stages.
        let bare = fx.claimed("bare", None).await;
        let (done, target) = fx.complete(bare.id, T2).await;
        assert_eq!(target, CompletionTarget::Done);
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.done_at, Some(T2));

        // A PR: delivering at In PR, no done_at yet.
        let with_pr = fx.claimed("with pr", None).await;
        fx.link(with_pr.id, 12).await;
        let (delivering, target) = fx.complete(with_pr.id, T2).await;
        assert_eq!(target, CompletionTarget::Delivering(InPr));
        assert_eq!(delivering.status, TaskStatus::Delivering);
        assert_eq!(delivering.delivery_stage, Some(InPr));
        assert_eq!(delivering.done_at, None);
        assert_eq!(delivering.acked_at, None);
        assert_eq!(delivering.lease_expires_at, None);
        assert_eq!(delivering.owner_agent_id, Some(fx.owner.agent.id));
        assert_eq!(delivering, fx.get(with_pr.id).await);
        let ev = last_of(
            &fx.store.task_events(with_pr.id).await.unwrap(),
            TaskEventKind::Done,
        );
        assert_eq!(ev.note.as_deref(), Some("shipped"));
        assert_eq!(
            ev.payload,
            Some(serde_json::json!({ "summary": "shipped", "delivering": "in_pr" }))
        );

        // The PR already merged and the project stops at Merged: straight to done.
        let merged = fx.claimed("merged", None).await;
        fx.link(merged.id, 13).await;
        set_stage(&raw, merged.id, Merged).await;
        let (done, target) = fx.complete(merged.id, T2).await;
        assert_eq!(target, CompletionTarget::Done);
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.done_at, Some(T2));
        assert_eq!(done.delivery_stage, Some(Merged));
    }
);

store_matrix!(
    complete_links_first_pr,
    |store: Arc<dyn Store>| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged]).await;
        let link = || Some(pr(30, PrLinkedBy::CompleteLinks));

        // A guard miss writes nothing, not even the link.
        let task = fx.claimed("ship", None).await;
        let mut wrong = fx.complete_req(task.id, T2, link());
        wrong.transition.owner = Some(SessionId::new());
        assert!(fx.store.complete_task(&wrong).await.unwrap().is_none());
        assert_eq!(fx.store.task_pr(task.id).await.unwrap(), None);
        assert_eq!(fx.get(task.id).await, task);

        let done = fx
            .store
            .complete_task(&fx.complete_req(task.id, T2, link()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.target, CompletionTarget::Delivering(InPr));
        assert_eq!(done.transitioned.task.status, TaskStatus::Delivering);
        assert_eq!(done.transitioned.task.delivery_stage, Some(InPr));
        let linked = fx.store.task_pr(task.id).await.unwrap().unwrap();
        assert_eq!(linked.number, Some(30));
        assert_eq!(linked.linked_by, PrLinkedBy::CompleteLinks);
        assert_eq!(fx.store.task_stages(task.id).await.unwrap().len(), 1);
        let kinds = fx.kinds(task.id).await;
        assert!(kinds.contains(&TaskEventKind::PrLinked), "{kinds:?}");
        assert!(kinds.contains(&TaskEventKind::Done), "{kinds:?}");

        // The same PR on another task is taken: skipped, and that task is simply done.
        let other = fx.claimed("other", None).await;
        let done = fx
            .store
            .complete_task(&fx.complete_req(other.id, T2, link()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.target, CompletionTarget::Done);
        assert_eq!(done.transitioned.task.status, TaskStatus::Done);
        assert_eq!(fx.store.task_pr(other.id).await.unwrap(), None);
        assert!(!fx.kinds(other.id).await.contains(&TaskEventKind::PrLinked));

        // A task with a PR keeps it; the completion's link is ignored.
        let third = fx.claimed("third", None).await;
        fx.link(third.id, 31).await;
        fx.store
            .complete_task(&fx.complete_req(third.id, T2, Some(pr(32, PrLinkedBy::CompleteLinks))))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fx.store.task_pr(third.id).await.unwrap().unwrap().number,
            Some(31)
        );
    }
);

store_matrix_raw!(
    parent_follows_minimum_child,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&DeliveryStage::ALL).await;
        let parent = fx.claimed("parent", None).await;
        let released = fx.claimed("released", Some(parent.id)).await;
        let merged = fx.claimed("merged", Some(parent.id)).await;
        let cancelled = fx.claimed("cancelled", Some(parent.id)).await;

        fx.link(released.id, 21).await;
        set_stage(&raw, released.id, Released).await;
        assert_eq!(
            fx.complete(released.id, T1).await.1,
            CompletionTarget::Delivering(Released)
        );
        fx.link(merged.id, 22).await;
        set_stage(&raw, merged.id, Merged).await;
        assert_eq!(
            fx.complete(merged.id, T1).await.1,
            CompletionTarget::Delivering(Merged)
        );
        fx.svc.cancel(Caller::Human, cancelled.id).await.unwrap();
        // Delivering children are still open: no children_complete, the parent stays claimed.
        assert_eq!(fx.get(parent.id).await.status, TaskStatus::Claimed);
        assert!(
            !fx.kinds(parent.id)
                .await
                .contains(&TaskEventKind::ChildrenComplete)
        );

        let (p, target) = fx.complete(parent.id, T2).await;
        assert_eq!(target, CompletionTarget::Delivering(Merged));
        assert_eq!(p.status, TaskStatus::Delivering);
        assert_eq!(p.delivery_stage, Some(Merged));
        assert_eq!(p.done_at, None);

        // The Merged child finishes: the parent moves forward to the slowest one left,
        // Released, and that delivering child holds it back from done.
        fx.deliver(merged.id, T2).await;
        let p = fx.get(parent.id).await;
        assert_eq!(p.status, TaskStatus::Delivering);
        assert_eq!(p.delivery_stage, Some(Released));
        assert_eq!(p.done_at, None);
        let events = fx.store.task_events(parent.id).await.unwrap();
        let reached = last_of(&events, TaskEventKind::StageReached);
        assert_eq!(
            reached.payload,
            Some(serde_json::json!({ "stage": "released" }))
        );
        assert!(events.iter().any(|e| e.kind == TaskEventKind::ChildDone));

        // The last child finishes: the parent is done, with done_at.
        fx.deliver(released.id, T3).await;
        let p = fx.get(parent.id).await;
        assert_eq!(p.status, TaskStatus::Done);
        assert_eq!(p.done_at, Some(T3));
        assert_eq!(p.updated_at, T3);
        let delivered = last_of(
            &fx.store.task_events(parent.id).await.unwrap(),
            TaskEventKind::Delivered,
        );
        assert_eq!(delivered.at, T3);
        assert_eq!(delivered.actor, Actor::Human);
        // A delivering parent records no children_complete.
        let kinds = fx.kinds(parent.id).await;
        assert!(!kinds.contains(&TaskEventKind::ChildrenComplete));
        // Exactly one child_done per delivering child: completing into `delivering` does not
        // bubble, the later `delivered` does.
        let count = |k: TaskEventKind| kinds.iter().filter(|e| **e == k).count();
        assert_eq!(count(TaskEventKind::ChildDone), 2, "{kinds:?}");
        assert_eq!(count(TaskEventKind::ChildCancelled), 1, "{kinds:?}");
    }
);

store_matrix!(
    settings_save_racing_a_child_change_does_not_deadlock,
    |store: Arc<dyn Store>| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged, Released]).await;
        for round in 0..10 {
            let parent = fx.claimed("parent", None).await;
            let a = fx.claimed("a", Some(parent.id)).await;
            let b = fx.claimed("b", Some(parent.id)).await;
            fx.link(a.id, 200 + 2 * round).await;
            fx.link(b.id, 201 + 2 * round).await;
            fx.complete(a.id, T1).await;
            fx.complete(b.id, T1).await;
            fx.complete(parent.id, T1).await;
            // The save locks every delivering row (children first); the child change locks
            // the child, then the parent. Both must succeed.
            let mut d = ProjectDelivery::defaults(&fx.project, T2);
            d.stages = vec![InPr, Merged, Released];
            let (saved, _) = tokio::join!(
                fx.store.set_project_delivery(&d, None, None),
                fx.deliver(a.id, T2)
            );
            saved.unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert_eq!(fx.get(parent.id).await.status, TaskStatus::Delivering);
        }
    }
);

store_matrix!(
    siblings_finishing_together_finish_the_parent_once,
    |store: Arc<dyn Store>| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged]).await;
        for round in 0..5 {
            let parent = fx.claimed("parent", None).await;
            let a = fx.claimed("a", Some(parent.id)).await;
            let b = fx.claimed("b", Some(parent.id)).await;
            fx.link(a.id, 100 + 2 * round).await;
            fx.link(b.id, 101 + 2 * round).await;
            fx.complete(a.id, T1).await;
            fx.complete(b.id, T1).await;
            fx.complete(parent.id, T1).await;
            // Each child's transaction reads its sibling under the parent's lock, so the last
            // to commit always sees both done.
            tokio::join!(fx.deliver(a.id, T2), fx.deliver(b.id, T2));
            let p = fx.get(parent.id).await;
            assert_eq!(p.status, TaskStatus::Done, "round {round}");
            let delivered = fx
                .kinds(parent.id)
                .await
                .iter()
                .filter(|k| **k == TaskEventKind::Delivered)
                .count();
            assert_eq!(delivered, 1, "round {round}");
        }
    }
);

store_matrix_raw!(
    delivering_parent_links_its_own_pr,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged, Released]).await;
        let parent = fx.claimed("parent", None).await;
        let child = fx.claimed("child", Some(parent.id)).await;
        fx.link(child.id, 80).await;
        set_stage(&raw, child.id, Merged).await;
        fx.complete(child.id, T1).await;
        let (p, _) = fx.complete(parent.id, T1).await;
        // Shown at the child's Merged, but the parent itself has reached nothing.
        assert_eq!(p.delivery_stage, Some(Merged));

        // Its own PR is not refused as "already merged" by the derived stage.
        let LinkOutcome::Linked(linked) = fx.link(parent.id, 81).await else {
            panic!("expected Linked");
        };
        // The recompute is forward only: the shown stage is not lowered to In PR.
        assert_eq!(linked.status, TaskStatus::Delivering);
        assert_eq!(linked.delivery_stage, Some(Merged));
        assert_eq!(linked, fx.get(parent.id).await);

        // Its own PR (In PR) now holds it back after the child is done.
        fx.deliver(child.id, T2).await;
        let p = fx.get(parent.id).await;
        assert_eq!(p.status, TaskStatus::Delivering);
        assert_eq!(p.delivery_stage, Some(Merged));
    }
);

store_matrix_raw!(
    parent_own_stage_comes_from_task_stages,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&DeliveryStage::ALL).await;
        let parent = fx.claimed("parent", None).await;
        let child = fx.claimed("child", Some(parent.id)).await;
        fx.link(parent.id, 90).await;
        // Only the task_stages row: `delivery_stage` stays In PR.
        reach_stage(&raw, parent.id, Released).await;
        assert_eq!(fx.get(parent.id).await.delivery_stage, Some(InPr));
        fx.link(child.id, 91).await;
        set_stage(&raw, child.id, Merged).await;
        fx.complete(child.id, T1).await;
        // The minimum of its own Released (from task_stages) and the child's Merged.
        let (p, target) = fx.complete(parent.id, T1).await;
        assert_eq!(target, CompletionTarget::Delivering(Merged));
        assert_eq!(p.delivery_stage, Some(Merged));

        // The child is done: the parent moves forward to its own Released, read from
        // task_stages (its delivery_stage says Merged), and waits for Deployed.
        fx.deliver(child.id, T2).await;
        let p = fx.get(parent.id).await;
        assert_eq!(p.status, TaskStatus::Delivering);
        assert_eq!(p.delivery_stage, Some(Released));

        // A task_stages row alone (no delivery_stage write) is the own stage too.
        reach_stage(&raw, parent.id, Deployed).await;
        assert_eq!(
            fx.link(parent.id, 92).await,
            LinkOutcome::AlreadyMerged { number: Some(90) }
        );
    }
);

store_matrix!(parent_with_pr_is_held_by_its_own_stage, |store: Arc<
    dyn Store,
>| async move {
    use DeliveryStage::*;
    let fx = Fx::new(store).await;
    fx.stages(&[InPr, Merged]).await;
    let parent = fx.claimed("parent", None).await;
    let child = fx.claimed("child", Some(parent.id)).await;
    fx.link(parent.id, 50).await;
    fx.link(child.id, 51).await;
    fx.complete(child.id, T1).await;
    assert_eq!(
        fx.complete(parent.id, T1).await.1,
        CompletionTarget::Delivering(InPr)
    );
    // Every child done, but the parent's own PR is only In PR.
    fx.deliver(child.id, T2).await;
    let p = fx.get(parent.id).await;
    assert_eq!(p.status, TaskStatus::Delivering);
    assert_eq!(p.delivery_stage, Some(InPr));
});

store_matrix_raw!(
    parent_waits_for_child_in_other_project,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged]).await;
        let other = fx
            .store
            .upsert_project("github.com/reyemtech/other", "other", T0)
            .await
            .unwrap();
        settings(&*fx.store, &other, &DeliveryStage::ALL, T0).await;
        let parent = fx.claimed("parent", None).await;
        let child = fx.claimed("child", Some(parent.id)).await;
        fx.link(child.id, 60).await;
        // The child lives in the other project, which delivers up to Deployed.
        raw.execute(&format!(
            "UPDATE tasks SET project_id = '{}' WHERE id = '{}'",
            other.id, child.id
        ))
        .await;
        fx.complete(child.id, T1).await;
        set_stage(&raw, child.id, Released).await;

        // Released is past the parent project's last stage, but the child still delivers.
        let (p, target) = fx.complete(parent.id, T2).await;
        assert_eq!(target, CompletionTarget::Delivering(Released));
        assert_eq!(p.status, TaskStatus::Delivering);
        assert_eq!(p.done_at, None);

        // The child reaching done finishes the parent.
        fx.deliver(child.id, T3).await;
        let p = fx.get(parent.id).await;
        assert_eq!(p.status, TaskStatus::Done);
        assert_eq!(p.done_at, Some(T3));
    }
);

store_matrix!(complete_without_stages_ignores_links, |store: Arc<
    dyn Store,
>| async move {
    let fx = Fx::new(store).await;
    let task = fx.claimed("ship", None).await;
    let done = fx
        .store
        .complete_task(&fx.complete_req(task.id, T2, Some(pr(70, PrLinkedBy::CompleteLinks))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.target, CompletionTarget::Done);
    assert_eq!(done.transitioned.task.status, TaskStatus::Done);
    assert_eq!(done.transitioned.task.delivery_stage, None);
    assert_eq!(fx.store.task_pr(task.id).await.unwrap(), None);
    assert!(fx.store.task_stages(task.id).await.unwrap().is_empty());
    assert!(!fx.kinds(task.id).await.contains(&TaskEventKind::PrLinked));
});

store_matrix!(
    complete_checks_latest_plan,
    |store: Arc<dyn Store>| async move {
        let fx = Fx::new(store).await;
        fx.stages(&[DeliveryStage::InPr]).await;
        let task = fx.claimed("ship", None).await;
        let mut stale = fx.complete_req(task.id, T2, None);
        stale.transition.latest_plan = Some(MessageId::new());
        assert!(fx.store.complete_task(&stale).await.unwrap().is_none());
        assert_eq!(fx.get(task.id).await, task);
    }
);

store_matrix!(
    concurrent_unlinks_record_once,
    |store: Arc<dyn Store>| async move {
        let fx = Fx::new(store).await;
        fx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
            .await;
        for round in 0..5 {
            let task = fx.claimed("ship", None).await;
            fx.link(task.id, 80 + round).await;
            let unlink = || fx.store.unlink_pr(task.id, Actor::Human, None, T2);
            let (a, b) = tokio::join!(unlink(), unlink());
            let removed = [a.unwrap(), b.unwrap()];
            assert_eq!(removed.iter().filter(|r| r.is_some()).count(), 1);
            let unlinked = fx
                .kinds(task.id)
                .await
                .into_iter()
                .filter(|k| *k == TaskEventKind::PrUnlinked)
                .count();
            assert_eq!(unlinked, 1);
        }
    }
);

store_matrix!(stage_change_moves_parent_of_finished_child, |store: Arc<
    dyn Store,
>| async move {
    use DeliveryStage::*;
    let fx = Fx::new(store).await;
    fx.stages(&[InPr, Merged]).await;
    let parent = fx.claimed("parent", None).await;
    let child = fx.claimed("child", Some(parent.id)).await;
    fx.link(child.id, 60).await;
    fx.complete(child.id, T1).await;
    assert_eq!(
        fx.complete(parent.id, T1).await.1,
        CompletionTarget::Delivering(InPr)
    );
    // Still delivering under the new stages: a delivering child holds the parent back.
    assert!(
        settings(&*fx.store, &fx.project, &[InPr, Merged, Released], T2)
            .await
            .is_empty()
    );
    assert_eq!(fx.get(parent.id).await.status, TaskStatus::Delivering);

    // In PR is now the last stage: the child is done, and the parent rule finishes the
    // parent in the same write; both are returned.
    let moved = settings(&*fx.store, &fx.project, &[InPr], T3).await;
    let mut ids: Vec<String> = moved.iter().map(|t| t.id.to_string()).collect();
    ids.sort();
    let mut want = vec![parent.id.to_string(), child.id.to_string()];
    want.sort();
    assert_eq!(ids, want);
    let p = fx.get(parent.id).await;
    assert_eq!(p.status, TaskStatus::Done);
    assert_eq!(p.done_at, Some(T3));
    assert!(moved.contains(&p));
});

store_matrix!(delivering_not_hidden_by_task_list, |store: Arc<
    dyn Store,
>| async move {
    use DeliveryStage::*;
    let fx = Fx::new(store).await;
    fx.stages(&[InPr, Merged]).await;
    let task = fx.claimed("ship", None).await;
    fx.link(task.id, 12).await;
    fx.complete(task.id, T1).await;
    for done in [
        DoneFilter::Any,
        DoneFilter::Unseen { since: T3 },
        DoneFilter::AutoAckedUnseen { since: T0 },
    ] {
        let filter = TaskFilter {
            project: Some(fx.project.id),
            done,
            ..TaskFilter::default()
        };
        let ids: Vec<TaskId> = fx
            .store
            .list_tasks(&filter)
            .await
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, [task.id], "{done:?}");
        assert_eq!(fx.store.count_tasks(&filter).await.unwrap(), 1);
    }
});

// ---- stage moves, Mark done and the signal log ----

/// A stage move of `task` to `to` at `at`, by the human, with no signal.
fn stage_move(
    task: TaskId,
    to: DeliveryStage,
    implied: &[DeliveryStage],
    at: time::OffsetDateTime,
) -> StageMove {
    StageMove {
        task,
        to,
        implied: implied.to_vec(),
        signal_id: None,
        detail: None,
        actor: Actor::Human,
        token: None,
        now: at,
        unblock: None,
    }
}

impl Fx {
    async fn advance(
        &self,
        task: TaskId,
        to: DeliveryStage,
        at: time::OffsetDateTime,
    ) -> Option<StageMoved> {
        self.store
            .advance_stage(&stage_move(task, to, &[], at))
            .await
            .unwrap()
    }

    /// A claimed task with PR `#n`, completed into `delivering` at `T1`.
    async fn delivering(&self, title: &str, parent: Option<TaskId>, n: i64) -> Task {
        let task = self.claimed(title, parent).await;
        self.link(task.id, n).await;
        let (task, target) = self.complete(task.id, T1).await;
        assert!(matches!(target, CompletionTarget::Delivering(_)));
        task
    }
}

fn signal(project: axle::domain::ProjectId, delivery_id: Option<&str>) -> DeliverySignal {
    DeliverySignal {
        id: Uuid::now_v7(),
        project_id: project,
        kind: SignalKind::Merged,
        source: SignalSource::Webhook,
        version: None,
        sha: Some("abc123".into()),
        environment: None,
        url: None,
        delivery_id: delivery_id.map(str::to_string),
        outcome: SignalOutcome::Applied,
        matched: 1,
        received_at: T1,
    }
}

store_matrix!(advance_forward_only, |store: Arc<dyn Store>| async move {
    use DeliveryStage::*;
    let fx = Fx::new(store).await;
    fx.stages(&[InPr, Merged, Released]).await;
    let task = fx.claimed("ship", None).await;
    fx.link(task.id, 12).await;
    let before = fx.get(task.id).await;

    let moved = fx.advance(task.id, Merged, T2).await.expect("forward move");
    assert!(!moved.delivered);
    assert_eq!(moved.task.delivery_stage, Some(Merged));
    assert_eq!(moved.task.status, TaskStatus::Claimed);
    assert_eq!(moved.task.version, before.version + 1);
    assert_eq!(moved.task.updated_at, T2);
    assert_eq!(moved.task, fx.get(task.id).await);
    let ev = last_of(
        &fx.store.task_events(task.id).await.unwrap(),
        TaskEventKind::StageReached,
    );
    assert_eq!(
        ev.payload,
        Some(serde_json::json!({ "stage": "merged", "implied": [] }))
    );

    // Back to In PR, or Merged again: nothing changes.
    assert!(fx.advance(task.id, InPr, T3).await.is_none());
    assert!(fx.advance(task.id, Merged, T3).await.is_none());
    assert_eq!(fx.get(task.id).await, moved.task);
    // An unknown task misses the guard too.
    assert!(fx.advance(TaskId::new(), Merged, T3).await.is_none());
});

store_matrix!(
    advance_race_has_one_winner,
    |store: Arc<dyn Store>| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged, Released]).await;
        let task = fx.delivering("ship", None, 12).await;
        let (a, b) = (fx.store.clone(), fx.store.clone());
        let (ma, mb) = (
            stage_move(task.id, Merged, &[], T2),
            stage_move(task.id, Merged, &[], T2),
        );
        let (ra, rb) = tokio::join!(a.advance_stage(&ma), b.advance_stage(&mb));
        let winners = [ra.unwrap(), rb.unwrap()]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(winners, 1);
        let reached = fx
            .kinds(task.id)
            .await
            .into_iter()
            .filter(|k| *k == TaskEventKind::StageReached)
            .count();
        assert_eq!(reached, 1);
        let t = fx.get(task.id).await;
        assert_eq!(t.delivery_stage, Some(Merged));
        assert_eq!(t.version, task.version + 1);
    }
);

store_matrix!(
    implied_stages_recorded,
    |store: Arc<dyn Store>| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged, Released, Deployed]).await;
        let task = fx.delivering("ship", None, 12).await;
        let signal_id = Uuid::now_v7();
        let detail = serde_json::json!({ "version": "v1.2.0" });
        let m = StageMove {
            signal_id: Some(signal_id),
            detail: Some(detail.clone()),
            ..stage_move(task.id, Released, &[Merged], T2)
        };
        let moved = fx.store.advance_stage(&m).await.unwrap().expect("moves");
        assert_eq!(moved.task.delivery_stage, Some(Released));
        assert_eq!(moved.task.status, TaskStatus::Delivering);

        let rows = fx.store.task_stages(task.id).await.unwrap();
        let stages: Vec<DeliveryStage> = rows.iter().map(|r| r.stage).collect();
        assert_eq!(stages, [InPr, Merged, Released]);
        let merged = &rows[1];
        assert!(merged.implied);
        assert_eq!(merged.reached_at, T2);
        assert_eq!(merged.signal_id, Some(signal_id));
        assert_eq!(merged.detail, None);
        let released = &rows[2];
        assert!(!released.implied);
        assert_eq!(released.reached_at, T2);
        assert_eq!(released.signal_id, Some(signal_id));
        assert_eq!(released.detail, Some(detail));
        let ev = last_of(
            &fx.store.task_events(task.id).await.unwrap(),
            TaskEventKind::StageReached,
        );
        assert_eq!(
            ev.payload,
            Some(serde_json::json!({ "stage": "released", "implied": ["merged"] }))
        );
    }
);

store_matrix_raw!(
    pre_completion_stage_recorded,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged]).await;
        let task = fx.claimed("ship", None).await;
        fx.link(task.id, 12).await;
        raw.execute(&format!(
            "UPDATE tasks SET status = 'review' WHERE id = '{}'",
            task.id
        ))
        .await;

        // Merged is the last stage, but a task in review only records it.
        let moved = fx.advance(task.id, Merged, T2).await.expect("moves");
        assert!(!moved.delivered);
        assert_eq!(moved.task.status, TaskStatus::Review);
        assert_eq!(moved.task.delivery_stage, Some(Merged));
        assert_eq!(moved.task.done_at, None);
        let stages: Vec<DeliveryStage> = fx
            .store
            .task_stages(task.id)
            .await
            .unwrap()
            .iter()
            .map(|r| r.stage)
            .collect();
        assert_eq!(stages, [InPr, Merged]);

        // Completing it later goes straight to done.
        let (done, target) = fx.complete(task.id, T3).await;
        assert_eq!(target, CompletionTarget::Done);
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.done_at, Some(T3));
        assert_eq!(done.delivery_stage, Some(Merged));
    }
);

store_matrix!(last_stage_delivers, |store: Arc<dyn Store>| async move {
    use DeliveryStage::*;
    let fx = Fx::new(store).await;
    fx.stages(&[InPr, Merged]).await;
    let task = fx.delivering("ship", None, 12).await;
    assert_eq!(task.delivery_stage, Some(InPr));

    let moved = fx.advance(task.id, Merged, T2).await.expect("moves");
    assert!(moved.delivered);
    assert_eq!(moved.task.status, TaskStatus::Done);
    assert_eq!(moved.task.done_at, Some(T2));
    assert_eq!(moved.task.delivery_stage, Some(Merged));
    assert_eq!(moved.task, fx.get(task.id).await);
    let kinds = fx.kinds(task.id).await;
    assert_eq!(
        kinds[kinds.len() - 2..],
        [TaskEventKind::StageReached, TaskEventKind::Delivered]
    );
    let ev = last_of(
        &fx.store.task_events(task.id).await.unwrap(),
        TaskEventKind::Delivered,
    );
    assert_eq!(ev.at, T2);
    assert_eq!(ev.payload, Some(serde_json::json!({ "stage": "merged" })));

    // A done task never moves again.
    assert!(fx.advance(task.id, Released, T3).await.is_none());
});

store_matrix!(
    done_at_only_at_final_stage,
    |store: Arc<dyn Store>| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged, Released]).await;
        let task = fx.delivering("ship", None, 12).await;
        assert_eq!(task.status, TaskStatus::Delivering);
        assert_eq!(task.done_at, None);

        let moved = fx.advance(task.id, Merged, T2).await.expect("moves");
        assert!(!moved.delivered);
        assert_eq!(moved.task.status, TaskStatus::Delivering);
        assert_eq!(moved.task.done_at, None);

        // A later signal skipping past the last stage finishes it too.
        let moved = fx.advance(task.id, Deployed, T3).await.expect("moves");
        assert!(moved.delivered);
        assert_eq!(moved.task.status, TaskStatus::Done);
        assert_eq!(moved.task.done_at, Some(T3));
    }
);

store_matrix!(
    advance_parent_keeps_minimum,
    |store: Arc<dyn Store>| async move {
        use DeliveryStage::*;
        let fx = Fx::new(store).await;
        fx.stages(&[InPr, Merged, Released]).await;
        let parent = fx.claimed("parent", None).await;
        fx.link(parent.id, 70).await;
        let child = fx.delivering("child", Some(parent.id), 71).await;
        let (p, target) = fx.complete(parent.id, T1).await;
        assert_eq!(target, CompletionTarget::Delivering(InPr));
        assert_eq!(p.delivery_stage, Some(InPr));

        // The parent's own PR reaches Released: its shown stage stays the child's In PR.
        let moved = fx.advance(parent.id, Released, T2).await.expect("moves");
        assert!(!moved.delivered);
        assert_eq!(moved.task.status, TaskStatus::Delivering);
        assert_eq!(moved.task.delivery_stage, Some(InPr));
        // Its own stage is Released, so Released again misses.
        assert!(fx.advance(parent.id, Released, T2).await.is_none());

        // The child moving pulls the parent forward to the new minimum.
        fx.advance(child.id, Merged, T2).await.expect("moves");
        let p = fx.get(parent.id).await;
        assert_eq!(p.status, TaskStatus::Delivering);
        assert_eq!(p.delivery_stage, Some(Merged));

        // The child finishing finishes the parent (its own PR is past the last stage).
        let moved = fx.advance(child.id, Released, T3).await.expect("moves");
        assert!(moved.delivered);
        let p = fx.get(parent.id).await;
        assert_eq!(p.status, TaskStatus::Done);
        assert_eq!(p.done_at, Some(T3));
    }
);

store_matrix!(mark_delivered_guarded, |store: Arc<dyn Store>| async move {
    use DeliveryStage::*;
    let fx = Fx::new(store).await;
    fx.stages(&[InPr, Merged, Released]).await;
    let claimed = fx.claimed("not yet", None).await;
    assert!(
        fx.store
            .mark_delivered(claimed.id, Actor::Human, None, T2, None)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fx.get(claimed.id).await, claimed);

    let parent = fx.claimed("parent", None).await;
    let task = fx.delivering("ship", Some(parent.id), 12).await;
    let done = fx
        .store
        .mark_delivered(task.id, Actor::Human, None, T2, None)
        .await
        .unwrap()
        .expect("delivering task")
        .task;
    assert_eq!(done.status, TaskStatus::Done);
    assert_eq!(done.done_at, Some(T2));
    assert_eq!(done.updated_at, T2);
    assert_eq!(done.version, task.version + 1);
    assert_eq!(done.delivery_stage, Some(InPr));
    assert_eq!(done, fx.get(task.id).await);
    let ev = last_of(
        &fx.store.task_events(task.id).await.unwrap(),
        TaskEventKind::Delivered,
    );
    assert_eq!(ev.note.as_deref(), Some("marked done by the human"));
    assert_eq!(ev.actor, Actor::Human);

    // The parent rule ran: the last open child closed.
    let kinds = fx.kinds(parent.id).await;
    assert!(kinds.contains(&TaskEventKind::ChildDone), "{kinds:?}");
    assert!(
        kinds.contains(&TaskEventKind::ChildrenComplete),
        "{kinds:?}"
    );
    assert_eq!(fx.get(parent.id).await.status, TaskStatus::Review);

    // Done already: nothing more.
    assert!(
        fx.store
            .mark_delivered(task.id, Actor::Human, None, T3, None)
            .await
            .unwrap()
            .is_none()
    );
});

store_matrix!(signal_dedupe, |store: Arc<dyn Store>| async move {
    let project = store.upsert_project(PROJECT, "axle", T0).await.unwrap();
    let other = store
        .upsert_project("github.com/reyemtech/other", "other", T0)
        .await
        .unwrap();
    assert_eq!(
        store
            .insert_signal(&signal(project.id, Some("d-1")))
            .await
            .unwrap(),
        SignalInsert::Inserted
    );
    assert_eq!(
        store
            .insert_signal(&signal(project.id, Some("d-1")))
            .await
            .unwrap(),
        SignalInsert::Duplicate
    );
    // Another project's delivery ids are its own.
    assert_eq!(
        store
            .insert_signal(&signal(other.id, Some("d-1")))
            .await
            .unwrap(),
        SignalInsert::Inserted
    );
    // Signals without a delivery id never collide.
    for _ in 0..2 {
        assert_eq!(
            store
                .insert_signal(&signal(project.id, None))
                .await
                .unwrap(),
            SignalInsert::Inserted
        );
    }
});

// ---- DeliveryService ----

/// `Fx` plus a `DeliveryService` over the same store (its clock reads `T1`).
struct Dx {
    fx: Fx,
    svc: DeliveryService,
}

fn pr_url(n: i64) -> String {
    format!("https://github.com/reyemtech/axle/pull/{n}")
}

impl Dx {
    async fn new(store: Arc<dyn Store>) -> Self {
        let svc = DeliveryService::new(store.clone(), Arc::new(TestClock::new(T1)));
        Dx {
            fx: Fx::new(store).await,
            svc,
        }
    }

    async fn agent(&self, host: &str) -> Resolved {
        IdentityService::resolve(
            &*self.fx.store,
            &TestClock::new(T0),
            IdentityInput {
                project_key: Some(PROJECT.into()),
                host: host.into(),
                harness: "claude-code".into(),
                session: SessionId::new(),
                token: common::token(&*self.fx.store).await,
            },
        )
        .await
        .unwrap()
    }

    async fn stages(&self, stages: &[DeliveryStage]) {
        self.svc
            .set_stages(&self.fx.project, stages.to_vec())
            .await
            .unwrap();
    }

    /// The owner completes a claimed task whose `links` hold PR `n`.
    async fn delivering(&self, title: &str, n: i64) -> Task {
        let task = self.fx.claimed(title, None).await;
        self.fx
            .svc
            .complete(
                &self.fx.owner,
                task.id,
                Completion {
                    summary: "shipped".into(),
                    links: vec![pr_url(n)],
                    ..Completion::default()
                },
            )
            .await
            .unwrap()
    }
}

store_matrix!(agent_link_permissions, |store: Arc<dyn Store>| async move {
    let dx = Dx::new(store).await;
    let other = dx.agent("other-host").await;
    let stranger = dx.agent("stranger-host").await;
    // Created by the owner agent, then released and claimed by `other`.
    let task = dx.fx.claimed("work", None).await;
    dx.fx.svc.force_release(task.id).await.unwrap();
    dx.fx.svc.claim(&other, task.id).await.unwrap();

    // A third agent may not report it; the creator and the owner may.
    let err = dx
        .svc
        .link_pr(Caller::Agent(&stranger), task.id, &pr_url(7))
        .await
        .unwrap_err();
    assert!(matches!(err, DeliveryError::NotAllowed), "{err}");
    assert_eq!(
        err.to_string(),
        "only the task's owner or creator can report its PR"
    );
    let linked = dx
        .svc
        .link_pr(Caller::Agent(&dx.fx.owner), task.id, &pr_url(7))
        .await
        .unwrap();
    assert_eq!(linked.delivery_stage, Some(DeliveryStage::InPr));
    dx.svc
        .link_pr(Caller::Agent(&other), task.id, &pr_url(8))
        .await
        .unwrap();
    let pr = dx.fx.store.task_pr(task.id).await.unwrap().unwrap();
    assert_eq!(pr.number, Some(8));
    assert_eq!(pr.linked_by, PrLinkedBy::Agent);

    // The human may link any task.
    let human = dx.fx.claimed("other work", None).await;
    dx.svc
        .link_pr(Caller::Human, human.id, &pr_url(9))
        .await
        .unwrap();
    assert_eq!(
        dx.fx
            .store
            .task_pr(human.id)
            .await
            .unwrap()
            .unwrap()
            .linked_by,
        PrLinkedBy::Human
    );

    // A done task is closed.
    let finished = dx.fx.claimed("finished", None).await;
    dx.fx.complete(finished.id, T1).await;
    let err = dx
        .svc
        .link_pr(Caller::Agent(&dx.fx.owner), finished.id, &pr_url(7))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DeliveryError::Task(TaskError::Closed(TaskStatus::Done))
        ),
        "{err}"
    );
});

store_matrix_raw!(
    link_error_messages,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        let dx = Dx::new(store).await;
        let me = Caller::Agent(&dx.fx.owner);
        let first = dx.fx.claimed("first", None).await;
        let second = dx.fx.claimed("second", None).await;

        let err = dx
            .svc
            .link_pr(me, first.id, "https://github.com/other/repo/pull/1")
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "pr must be a pull request in github.com/reyemtech/axle"
        );

        dx.svc.link_pr(me, first.id, &pr_url(7)).await.unwrap();
        let err = dx.svc.link_pr(me, second.id, &pr_url(7)).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("PR #7 is already linked to task {}", first.id)
        );

        set_stage(&raw, first.id, DeliveryStage::Merged).await;
        let err = dx.svc.link_pr(me, first.id, &pr_url(8)).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "this task was already merged via #7; open a new task for follow-up work"
        );

        assert_eq!(
            DeliveryError::Delivering.to_string(),
            "this task is delivering; use Mark done or Force release"
        );
        assert_eq!(
            DeliveryError::NoNextStage.to_string(),
            "this task is already at its last delivery stage"
        );
        assert_eq!(
            DeliveryError::NotDelivering.to_string(),
            "this task is not delivering"
        );
        assert_eq!(
            DeliveryError::TakenBy {
                number: None,
                task: first.id
            }
            .to_string(),
            format!("this PR is already linked to task {}", first.id)
        );
    }
);

store_matrix_raw!(
    advance_moves_one_column,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        let dx = Dx::new(store).await;
        dx.stages(&[DeliveryStage::Deployed, DeliveryStage::InPr])
            .await;
        let task = dx.delivering("ship", 7).await;
        assert_eq!(task.status, TaskStatus::Delivering);
        assert_eq!(task.delivery_stage, Some(DeliveryStage::InPr));

        let done = dx.svc.advance(task.id).await.unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.delivery_stage, Some(DeliveryStage::Deployed));
        assert_eq!(done.done_at, Some(T1));
        let stages = dx.fx.store.task_stages(task.id).await.unwrap();
        let reached: Vec<_> = stages.iter().map(|s| (s.stage, s.implied)).collect();
        assert_eq!(
            reached,
            vec![
                (DeliveryStage::InPr, false),
                (DeliveryStage::Merged, true),
                (DeliveryStage::Released, true),
                (DeliveryStage::Deployed, false),
            ]
        );
        assert_eq!(
            raw.count(
                "SELECT COUNT(*) FROM delivery_signals
                 WHERE source = 'human' AND kind = 'deployed' AND outcome = 'applied'
                   AND matched = 1"
            )
            .await,
            1
        );
        assert!(
            dx.fx
                .kinds(task.id)
                .await
                .contains(&TaskEventKind::Delivered)
        );

        // A done task is not delivering.
        let err = dx.svc.advance(task.id).await.unwrap_err();
        assert!(matches!(err, DeliveryError::NotDelivering), "{err}");
        let err = dx.svc.mark_done(task.id).await.unwrap_err();
        assert!(matches!(err, DeliveryError::NotDelivering), "{err}");
    }
);

store_matrix_raw!(
    advance_at_waiting_goes_to_first,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        let dx = Dx::new(store).await;
        dx.stages(&[DeliveryStage::Released]).await;
        let task = dx.delivering("ship", 7).await;
        assert_eq!(task.status, TaskStatus::Delivering);
        let view = dx.svc.view(&task).await.unwrap().expect("has stages");
        assert_eq!(view.column, Some((DeliveryStage::Released, true)));
        assert_eq!(view.pr.map(|p| p.number), Some(Some(7)));

        let done = dx.svc.advance(task.id).await.unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        let stages = dx.fx.store.task_stages(task.id).await.unwrap();
        let merged = stages.iter().find(|s| s.stage == DeliveryStage::Merged);
        assert!(merged.is_some_and(|s| s.implied));
        assert_eq!(
            raw.count("SELECT COUNT(*) FROM delivery_signals WHERE kind = 'released'")
                .await,
            1
        );
    }
);

store_matrix_raw!(
    no_next_stage_and_mark_done,
    |store: Arc<dyn Store>, raw: common::RawDb| async move {
        let dx = Dx::new(store).await;
        dx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
            .await;
        // A delivering task already past every enabled stage has nowhere to advance to.
        let stuck = dx.delivering("stuck", 7).await;
        set_stage(&raw, stuck.id, DeliveryStage::Deployed).await;
        let err = dx.svc.advance(stuck.id).await.unwrap_err();
        assert!(matches!(err, DeliveryError::NoNextStage), "{err}");

        // The human may mark it done, skipping the stages.
        let done = dx.svc.mark_done(stuck.id).await.unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.done_at, Some(T1));

        // An open task is not delivering.
        let open = dx.fx.claimed("open", None).await;
        let err = dx.svc.mark_done(open.id).await.unwrap_err();
        assert!(matches!(err, DeliveryError::NotDelivering), "{err}");
    }
);

store_matrix!(force_release_keeps_pr, |store: Arc<dyn Store>| async move {
    let dx = Dx::new(store).await;
    dx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
        .await;
    let task = dx.delivering("ship", 7).await;
    assert_eq!(task.status, TaskStatus::Delivering);

    // The PR cannot be unlinked while delivering.
    let err = dx.svc.unlink_pr(task.id).await.unwrap_err();
    assert!(matches!(err, DeliveryError::Delivering), "{err}");

    let released = dx.fx.svc.force_release(task.id).await.unwrap();
    assert_eq!(released.status, TaskStatus::Open);
    assert_eq!(released.delivery_stage, Some(DeliveryStage::InPr));
    assert!(dx.fx.store.task_pr(task.id).await.unwrap().is_some());

    // A delivering task may also be cancelled.
    let other = dx.delivering("other", 9).await;
    assert_eq!(other.status, TaskStatus::Delivering);
    let cancelled = dx.fx.svc.cancel(Caller::Human, other.id).await.unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
});

store_matrix!(complete_reads_links, |store: Arc<dyn Store>| async move {
    let dx = Dx::new(store).await;
    dx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
        .await;
    let task = dx.fx.claimed("ship", None).await;
    let done = dx
        .fx
        .svc
        .complete(
            &dx.fx.owner,
            task.id,
            Completion {
                summary: "shipped".into(),
                // Only the third is a PR of this project's repo.
                links: vec![
                    "https://example.com/notes".into(),
                    "https://github.com/other/repo/pull/1".into(),
                    "https://github.com/reyemtech/axle/pull/7".into(),
                ],
                ..Completion::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Delivering);
    assert_eq!(done.done_at, None);
    let pr = dx.fx.store.task_pr(task.id).await.unwrap().unwrap();
    assert_eq!(pr.number, Some(7));
    assert_eq!(pr.linked_by, PrLinkedBy::CompleteLinks);

    // Without stages a completion is a plain done, as before.
    let plain = dx.fx.claimed("plain", None).await;
    dx.stages(&[]).await;
    let done = dx
        .fx
        .svc
        .complete(&dx.fx.owner, plain.id, Completion::default())
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Done);
});

store_matrix!(
    set_stages_unknown_project,
    |store: Arc<dyn Store>| async move {
        let svc = DeliveryService::new(store, Arc::new(TestClock::new(T1)));
        let ghost = Project {
            id: axle::domain::ProjectId::new(),
            key: "github.com/ghost/none".into(),
            display_name: "ghost".into(),
            created_at: T0,
        };
        let err = svc
            .set_stages(&ghost, vec![DeliveryStage::InPr])
            .await
            .unwrap_err();
        assert!(
            matches!(err, DeliveryError::Store(StoreError::NotFound("project"))),
            "{err}"
        );
    }
);

store_matrix!(child_through_delivery_notifies_parent, |store: Arc<
    dyn Store,
>| async move {
    let dx = Dx::new(store).await;
    dx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
        .await;
    let parent = dx.fx.claimed("parent", None).await;
    let child = dx.fx.claimed("child", Some(parent.id)).await;
    dx.fx
        .svc
        .complete(
            &dx.fx.owner,
            child.id,
            Completion {
                summary: "shipped".into(),
                links: vec![pr_url(7)],
                ..Completion::default()
            },
        )
        .await
        .unwrap();
    // Finishing through delivery tells the parent's thread, as a plain done does.
    dx.svc.advance(child.id).await.unwrap();
    let subjects: Vec<_> = dx
        .fx
        .store
        .task_thread(parent.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|m| m.subject)
        .collect();
    assert!(
        subjects.contains(&"child task delivering".to_string()),
        "{subjects:?}"
    );
    assert!(
        subjects.contains(&"child task done".to_string()),
        "{subjects:?}"
    );
});

/// The last open child of a claimed parent finishing through delivery closes the parent's
/// children like a plain done: the high-importance message, and the parent's move to review
/// published.
async fn last_child_through_delivery(store: Arc<dyn Store>, mark_done: bool) {
    let clock = Arc::new(TestClock::new(T1));
    let bus = axle::events::EventBus::new(store.clone(), clock.clone());
    let mut rx = bus.subscribe();
    let svc = DeliveryService::new(store.clone(), clock).with_events(bus);
    let dx = Dx {
        fx: Fx::new(store).await,
        svc,
    };
    dx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
        .await;
    let parent = dx.fx.claimed("parent", None).await;
    let child = dx.fx.claimed("child", Some(parent.id)).await;
    dx.fx
        .svc
        .complete(
            &dx.fx.owner,
            child.id,
            Completion {
                summary: "shipped".into(),
                links: vec![pr_url(7)],
                ..Completion::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(dx.fx.get(parent.id).await.status, TaskStatus::Claimed);
    while rx.try_recv().is_ok() {}

    if mark_done {
        dx.svc.mark_done(child.id).await.unwrap();
    } else {
        dx.svc.advance(child.id).await.unwrap();
    }
    assert_eq!(dx.fx.get(parent.id).await.status, TaskStatus::Review);
    let subjects: Vec<_> = dx
        .fx
        .store
        .task_thread(parent.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|m| m.subject)
        .collect();
    assert!(
        subjects.contains(&"child tasks complete".to_string()),
        "{subjects:?}"
    );
    let mut parent_changes = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let axle::events::Event::TaskChanged {
            id, status, kind, ..
        } = e
            && id == parent.id
        {
            parent_changes.push((status, kind));
        }
    }
    assert_eq!(
        parent_changes,
        vec![(TaskStatus::Review, TaskEventKind::ChildrenComplete)]
    );
}

store_matrix!(last_child_advanced_closes_parent_children, |store: Arc<
    dyn Store,
>| async move {
    last_child_through_delivery(store, false).await
});

store_matrix!(
    last_child_marked_done_closes_parent_children,
    |store: Arc<dyn Store>| async move { last_child_through_delivery(store, true).await }
);

/// Delivery finishing a leaf can finish its `delivering` parent and close the grandparent's
/// last open child: the grandparent's coordinator is told both, and its move is published.
/// How the leaf is finished in `cascade_notifies_grandparent`.
#[derive(Clone, Copy)]
enum LeafFinish {
    MarkDone,
    Advance,
    /// The project's stages are turned off, which finishes every delivering task.
    StagesOff,
}

async fn cascade_notifies_grandparent(store: Arc<dyn Store>, how: LeafFinish) {
    let clock = Arc::new(TestClock::new(T1));
    let bus = axle::events::EventBus::new(store.clone(), clock.clone());
    let mut rx = bus.subscribe();
    let svc = DeliveryService::new(store.clone(), clock).with_events(bus);
    let dx = Dx {
        fx: Fx::new(store).await,
        svc,
    };
    dx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
        .await;
    let grandparent = dx.fx.claimed("grandparent", None).await;
    let parent = dx.fx.claimed("parent", Some(grandparent.id)).await;
    let leaf = dx.fx.delivering("leaf", Some(parent.id), 7).await;
    let (parent, target) = dx.fx.complete(parent.id, T1).await;
    assert!(matches!(target, CompletionTarget::Delivering(_)));
    assert_eq!(parent.status, TaskStatus::Delivering);
    while rx.try_recv().is_ok() {}

    match how {
        LeafFinish::MarkDone => {
            dx.svc.mark_done(leaf.id).await.unwrap();
        }
        LeafFinish::Advance => {
            dx.svc.advance(leaf.id).await.unwrap();
        }
        LeafFinish::StagesOff => dx.stages(&[]).await,
    }
    assert_eq!(dx.fx.get(leaf.id).await.status, TaskStatus::Done);
    assert_eq!(dx.fx.get(parent.id).await.status, TaskStatus::Done);
    assert_eq!(dx.fx.get(grandparent.id).await.status, TaskStatus::Review);
    let subjects: Vec<_> = dx
        .fx
        .store
        .task_thread(grandparent.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|m| m.subject)
        .collect();
    assert!(
        subjects.contains(&"child task done".to_string()),
        "{subjects:?}"
    );
    assert!(
        subjects.contains(&"child tasks complete".to_string()),
        "{subjects:?}"
    );
    let mut changes = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let axle::events::Event::TaskChanged {
            id, status, kind, ..
        } = e
            && (id == parent.id || id == grandparent.id)
        {
            changes.push((id, status, kind));
        }
    }
    assert!(
        changes.contains(&(parent.id, TaskStatus::Done, TaskEventKind::Delivered)),
        "{changes:?}"
    );
    assert!(
        changes.contains(&(
            grandparent.id,
            TaskStatus::Review,
            TaskEventKind::ChildrenComplete
        )),
        "{changes:?}"
    );
}

store_matrix!(cascade_mark_done_notifies_grandparent, |store: Arc<
    dyn Store,
>| async move {
    cascade_notifies_grandparent(store, LeafFinish::MarkDone).await
});

store_matrix!(cascade_advance_notifies_grandparent, |store: Arc<
    dyn Store,
>| async move {
    cascade_notifies_grandparent(store, LeafFinish::Advance).await
});

store_matrix!(cascade_stages_off_notifies_grandparent, |store: Arc<
    dyn Store,
>| async move {
    cascade_notifies_grandparent(store, LeafFinish::StagesOff).await
});

// A parent delivering only through its child stays delivering when that child is
// force-released and the project's stages are saved again: it still has an open child.
store_matrix!(
    stages_save_keeps_parent_with_reopened_child,
    |store: Arc<dyn Store>| async move {
        let dx = Dx::new(store).await;
        let stages = [DeliveryStage::InPr, DeliveryStage::Merged];
        dx.stages(&stages).await;
        let parent = dx.fx.claimed("parent", None).await;
        let child = dx.fx.delivering("child", Some(parent.id), 7).await;
        let (parent, _) = dx.fx.complete(parent.id, T1).await;
        assert_eq!(parent.status, TaskStatus::Delivering);
        dx.fx.svc.force_release(child.id).await.unwrap();
        assert_eq!(dx.fx.get(child.id).await.status, TaskStatus::Open);

        let moved = dx
            .svc
            .set_stages(&dx.fx.project, stages.to_vec())
            .await
            .unwrap();
        assert!(moved.is_empty(), "{moved:?}");
        assert_eq!(dx.fx.get(parent.id).await.status, TaskStatus::Delivering);
        // Turning delivery off does not finish it either while the child is open.
        dx.stages(&[]).await;
        assert_eq!(dx.fx.get(parent.id).await.status, TaskStatus::Delivering);
    }
);

// ---- a task delivery finishes unblocks the tasks waiting on it ----

/// How the waited-on task is finished in `unblock_through_delivery`.
#[derive(Clone, Copy)]
enum Finish {
    MarkDone,
    LastStage,
    StagesOff,
    /// The target is a `delivering` parent; its last child's **Mark done** finishes it.
    ParentClosed,
}

/// A task blocked on a delivering task is unblocked when delivery makes that task `done`,
/// exactly as after a plain `done`: the `unblocked` event, the thread note and a published
/// `TaskChanged(Unblocked)`.
async fn unblock_through_delivery(store: Arc<dyn Store>, how: Finish) {
    let clock = Arc::new(TestClock::new(T1));
    let bus = axle::events::EventBus::new(store.clone(), clock.clone());
    let mut rx = bus.subscribe();
    let svc = DeliveryService::new(store.clone(), clock).with_events(bus);
    let dx = Dx {
        fx: Fx::new(store).await,
        svc,
    };
    dx.stages(&[DeliveryStage::InPr, DeliveryStage::Merged])
        .await;
    let (target, child) = match how {
        Finish::ParentClosed => {
            let parent = dx.fx.claimed("parent", None).await;
            let child = dx.fx.delivering("child", Some(parent.id), 7).await;
            let (parent, target) = dx.fx.complete(parent.id, T1).await;
            assert!(matches!(target, CompletionTarget::Delivering(_)));
            (parent, Some(child))
        }
        _ => (dx.delivering("target", 7).await, None),
    };
    assert_eq!(target.status, TaskStatus::Delivering);
    let waiter = dx.fx.claimed("waiter", None).await;
    dx.fx
        .svc
        .block_with(
            &dx.fx.owner,
            waiter.id,
            axle::service::tasks::BlockInput {
                reason: Some("needs the target".into()),
                waiting_on: Some(WaitingOn::Task(target.id)),
                unblock: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(dx.fx.get(waiter.id).await.status, TaskStatus::Blocked);
    while rx.try_recv().is_ok() {}

    match how {
        Finish::MarkDone => drop(dx.svc.mark_done(target.id).await.unwrap()),
        Finish::LastStage => drop(dx.svc.advance(target.id).await.unwrap()),
        Finish::StagesOff => dx.stages(&[]).await,
        Finish::ParentClosed => drop(dx.svc.mark_done(child.unwrap().id).await.unwrap()),
    }
    assert_eq!(dx.fx.get(target.id).await.status, TaskStatus::Done);
    let after = dx.fx.get(waiter.id).await;
    assert!(
        matches!(after.status, TaskStatus::Claimed | TaskStatus::Open),
        "{:?}",
        after.status
    );
    assert!(after.block.is_none());
    let events = dx.fx.store.task_events(waiter.id).await.unwrap();
    let unblocked = last_of(&events, TaskEventKind::Unblocked);
    assert_eq!(
        unblocked.payload.unwrap()["waiting_task_id"],
        target.id.to_string()
    );
    let thread = dx.fx.store.task_thread(waiter.id).await.unwrap();
    assert!(
        thread
            .iter()
            .any(|m| m.body.starts_with("unblocked: \"") && m.body.contains("is done")),
        "{thread:?}"
    );
    let mut published = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let axle::events::Event::TaskChanged { id, kind, .. } = e
            && id == waiter.id
        {
            published.push(kind);
        }
    }
    assert!(
        published.contains(&TaskEventKind::Unblocked),
        "{published:?}"
    );
}

store_matrix!(
    mark_done_unblocks_waiting,
    |store: Arc<dyn Store>| async move { unblock_through_delivery(store, Finish::MarkDone).await }
);

store_matrix!(
    last_stage_unblocks_waiting,
    |store: Arc<dyn Store>| async move { unblock_through_delivery(store, Finish::LastStage).await }
);

store_matrix!(
    stages_off_unblocks_waiting,
    |store: Arc<dyn Store>| async move { unblock_through_delivery(store, Finish::StagesOff).await }
);

store_matrix!(delivering_parent_closing_unblocks_waiting, |store: Arc<
    dyn Store,
>| async move {
    unblock_through_delivery(store, Finish::ParentClosed).await
});

store_matrix!(advance_parent_follows_its_own_stage, |store: Arc<
    dyn Store,
>| async move {
    use DeliveryStage::*;
    let dx = Dx::new(store).await;
    dx.stages(&[InPr, Merged, Released]).await;
    let parent = dx.fx.claimed("parent", None).await;
    let child = dx.fx.claimed("child", Some(parent.id)).await;
    let complete = |id: TaskId, n: i64| {
        let dx = &dx;
        async move {
            dx.fx
                .svc
                .complete(
                    &dx.fx.owner,
                    id,
                    Completion {
                        summary: "shipped".into(),
                        links: vec![pr_url(n)],
                        ..Completion::default()
                    },
                )
                .await
                .unwrap()
        }
    };
    complete(child.id, 51).await;
    let p = complete(parent.id, 50).await;
    assert_eq!(p.status, TaskStatus::Delivering);

    // The parent's own PR merges; the slower child holds its shown stage at In PR.
    let p = dx.svc.advance(parent.id).await.unwrap();
    assert_eq!(p.delivery_stage, Some(InPr));
    // The next advance goes on from the parent's own Merged, to Released.
    let p = dx.svc.advance(parent.id).await.unwrap();
    assert_eq!(p.status, TaskStatus::Delivering);
    let own: Vec<_> = dx
        .fx
        .store
        .task_stages(parent.id)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.stage)
        .collect();
    assert_eq!(own, vec![InPr, Merged, Released]);
    let err = dx.svc.advance(parent.id).await.unwrap_err();
    assert!(matches!(err, DeliveryError::NoNextStage), "{err}");
});
