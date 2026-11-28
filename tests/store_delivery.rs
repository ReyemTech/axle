#[macro_use]
mod common;

use std::path::Path;
use std::sync::Arc;

use axle::domain::{
    Actor, Address, DeliveryStage, DeploySource, Eligibility, Importance, Message, MessageId,
    MessageKind, PlanState, ProjectDelivery, ReleaseSource, Task, TaskEvent, TaskEventKind, TaskId,
    TaskStatus, short_id,
};
use axle::store::Store;
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

/// Copies the migrations of `backend` numbered up to and including 0005 into a fresh
/// directory, so a database can be built at the state before the delivery migration.
fn migrations_through_0005(backend: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("axle-mig-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if ["0001_", "0002_", "0003_", "0004_", "0005_"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            std::fs::copy(entry.path(), dir.join(&name)).unwrap();
        }
    }
    dir
}

/// The delivery migration rebuilds `tasks` on SQLite: a done task keeps `done_at` and
/// `acked_at`, and every earlier index survives next to the new one.
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
    let dir = migrations_through_0005("sqlite");
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

    // Opening the store runs 0008 (0006 and 0007 are reserved gaps).
    let store = SqliteStore::from_pool(pool.clone()).await.unwrap();
    let task = store.get_task(done_id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.done_at, Some(datetime!(2026-11-26 9:00 UTC)));
    assert_eq!(task.acked_at, Some(datetime!(2026-11-27 9:00 UTC)));
    assert!(task.requires_plan);
    assert_eq!(task.plan_state, PlanState::Approved);
    assert_eq!(task.delivery_stage, None);

    let indexes: Vec<String> = sqlx::query("SELECT name FROM pragma_index_list('tasks')")
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
        "tasks_delivery",
    ] {
        assert!(indexes.iter().any(|i| i == want), "{want} in {indexes:?}");
    }
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
    let dir = migrations_through_0005("postgres");
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
    store.set_project_delivery(&d, None).await.unwrap()
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
                .set_project_delivery(&d, None)
                .await
                .unwrap()
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
        store.set_project_delivery(&changed, None).await.unwrap();
        assert_eq!(
            store.get_project_delivery(project.id).await.unwrap(),
            Some(changed.clone())
        );

        let o = ProjectDelivery::defaults(&other, T1);
        store.set_project_delivery(&o, None).await.unwrap();
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
