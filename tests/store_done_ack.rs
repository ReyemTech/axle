#[macro_use]
mod common;

use std::path::Path;
use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{
    Actor, ProjectId, SessionId, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus,
};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{Caller, Completion, NewTask, TaskService};
use axle::store::{LeaseUpdate, Store, TransitionRequest};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-27 9:00 UTC);
const PROJECT: &str = "github.com/reyemtech/axle";

struct Env {
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    svc: Arc<TaskService>,
    agent: Resolved,
}

impl Env {
    async fn new(store: Arc<dyn Store>) -> Self {
        let clock = Arc::new(TestClock::new(T0));
        let svc = Arc::new(TaskService::new(
            store.clone(),
            clock.clone(),
            Duration::hours(12),
            Duration::hours(24),
            Duration::seconds(90),
        ));
        let agent = IdentityService::resolve(
            &*store,
            &*clock,
            IdentityInput {
                project_key: Some(PROJECT.into()),
                host: "host".into(),
                harness: "claude-code".into(),
                session: SessionId::new(),
                token: common::token(&*store).await,
            },
        )
        .await
        .unwrap();
        Self {
            store,
            clock,
            svc,
            agent,
        }
    }

    fn set_time(&self, offset: Duration) {
        let delta = T0 + offset - self.clock.now();
        self.clock.advance(delta);
    }

    async fn create(&self) -> TaskId {
        let project: ProjectId = self.agent.project.as_ref().expect("has a project").id;
        self.svc
            .create(
                Caller::Agent(&self.agent),
                NewTask {
                    project_id: project,
                    title: "fix the flaky test".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
            .id
    }

    async fn claim(&self, id: TaskId) {
        self.svc.claim(&self.agent, id).await.unwrap();
    }

    async fn complete(&self, id: TaskId) {
        self.svc
            .complete(
                &self.agent,
                id,
                Completion {
                    summary: "done".into(),
                    changed_files: vec![],
                    tests: vec![],
                    links: vec![],
                },
            )
            .await
            .unwrap();
    }

    async fn get(&self, id: TaskId) -> Task {
        self.store.get_task(id).await.unwrap().unwrap()
    }
}

store_matrix!(
    done_at_set_on_complete,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let open = env.create().await;
        let id = env.create().await;
        env.claim(id).await;
        assert_eq!(env.get(id).await.done_at, None);

        env.set_time(Duration::hours(1));
        env.complete(id).await;

        let t = env.get(id).await;
        assert_eq!(t.status, TaskStatus::Done);
        assert_eq!(t.done_at, Some(T0 + Duration::hours(1)));
        assert_eq!(t.acked_at, None);
        let o = env.get(open).await;
        assert_eq!(o.done_at, None);
        assert_eq!(o.acked_at, None);
    }
);

store_matrix!(
    leaving_done_clears_done_and_ack,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let id = env.create().await;
        env.claim(id).await;
        env.set_time(Duration::hours(1));
        env.complete(id).await;
        assert_eq!(env.get(id).await.done_at, Some(T0 + Duration::hours(1)));

        // Reopen (the service never does this; the store rule must hold regardless).
        let now = T0 + Duration::hours(2);
        let moved = env
            .store
            .transition_task(&TransitionRequest {
                task: id,
                from: vec![TaskStatus::Done],
                to: Some(TaskStatus::Open),
                owner: None,
                lease: LeaseUpdate::Clear,
                clear_owner: true,
                now,
                token: None,
                event: TaskEvent {
                    id: uuid::Uuid::now_v7(),
                    task_id: id,
                    at: now,
                    actor: Actor::from_ids(None, None),
                    kind: TaskEventKind::Released,
                    note: None,
                    payload: None,
                },
                plan_from: None,
                plan_to: None,
                message: None,
                latest_plan: None,
                archive_plans: false,
            })
            .await
            .unwrap()
            .expect("transition applies");
        assert_eq!(moved.task.status, TaskStatus::Open);
        let after = env.get(id).await;
        assert_eq!(after.done_at, None);
        assert_eq!(after.acked_at, None);

        // A second trip into done is a fresh finish.
        env.set_time(Duration::hours(3));
        env.claim(id).await;
        env.complete(id).await;
        let again = env.get(id).await;
        assert_eq!(again.done_at, Some(T0 + Duration::hours(3)));
        assert_eq!(again.acked_at, None);
    }
);

/// Copies migrations 0001..=0003 of `backend` into a fresh directory.
fn old_migrations(backend: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("axle-mig-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if ["0001_", "0002_", "0003_"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            std::fs::copy(entry.path(), dir.join(&name)).unwrap();
        }
    }
    dir
}

#[tokio::test]
async fn backfill_sets_done_at_from_updated_at_sqlite() {
    use axle::store::sqlite::SqliteStore;
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
    let done_id = uuid::Uuid::now_v7();
    let open_id = uuid::Uuid::now_v7();
    for (id, status) in [(done_id, "done"), (open_id, "open")] {
        sqlx::query(
            "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at)
             VALUES (?, ?, 't', ?, '2026-11-01T00:00:00.000000Z', '2026-11-20T09:00:00.000000Z')",
        )
        .bind(id.to_string())
        .bind(&project)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }

    let store = SqliteStore::from_pool(pool).await.unwrap();
    let done = store.get_task(done_id.into()).await.unwrap().unwrap();
    let open = store.get_task(open_id.into()).await.unwrap().unwrap();
    assert_eq!(done.done_at, Some(datetime!(2026-11-20 9:00 UTC)));
    assert_eq!(done.acked_at, None);
    assert_eq!(open.done_at, None);

    drop(store);
    let _ = std::fs::remove_dir_all(dir);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[cfg(feature = "pg-tests")]
#[tokio::test]
async fn backfill_sets_done_at_from_updated_at_postgres() {
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
            let done_id = uuid::Uuid::now_v7();
            let open_id = uuid::Uuid::now_v7();
            for (id, status) in [(done_id, "done"), (open_id, "open")] {
                sqlx::query(
                    "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at)
                     VALUES ($1, $2, 't', $3, '2026-11-01T00:00:00Z', '2026-11-20T09:00:00Z')",
                )
                .bind(id)
                .bind(project)
                .bind(status)
                .execute(&pool)
                .await
                .unwrap();
            }

            let store = PostgresStore::from_pool(pool).await.unwrap();
            let done = store.get_task(done_id.into()).await.unwrap().unwrap();
            let open = store.get_task(open_id.into()).await.unwrap().unwrap();
            assert_eq!(done.done_at, Some(datetime!(2026-11-20 9:00 UTC)));
            assert_eq!(done.acked_at, None);
            assert_eq!(open.done_at, None);
        }
    })
    .await;
    pool.close().await;
    let _ = std::fs::remove_dir_all(dir);
    schema.drop().await;
    common::resume(outcome);
}
