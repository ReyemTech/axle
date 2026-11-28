#[macro_use]
mod common;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{
    Actor, ProjectId, SessionId, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus,
};
use axle::events::{Event, EventBus};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{Caller, Completion, NewTask, TaskService};
use axle::store::{
    DoneFilter, LeaseUpdate, Store, TaskFilter, TaskOrder, TransitionRequest, Transitioned,
    like_pattern,
};
use time::macros::{datetime, format_description};
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

    async fn create_titled(&self, title: &str, parent: Option<TaskId>) -> TaskId {
        let project: ProjectId = self.agent.project.as_ref().expect("has a project").id;
        self.svc
            .create(
                Caller::Agent(&self.agent),
                NewTask {
                    project_id: project,
                    parent_id: parent,
                    title: title.into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
            .id
    }

    /// Creates, claims and completes a task at `T0 + offset`.
    async fn finish(&self, title: &str, parent: Option<TaskId>, offset: Duration) -> TaskId {
        let id = self.create_titled(title, parent).await;
        self.claim(id).await;
        self.set_time(offset);
        self.complete(id).await;
        id
    }

    async fn list(&self, filter: TaskFilter) -> Vec<Task> {
        self.store.list_tasks(&filter).await.unwrap()
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
        let moved = reopen(&env, id, now).await;
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

/// Moves a done task back to open through the store (the service never does this).
async fn reopen(env: &Env, id: TaskId, now: OffsetDateTime) -> Transitioned {
    env.store
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
        .expect("transition applies")
}

fn set(ids: impl IntoIterator<Item = TaskId>) -> HashSet<TaskId> {
    ids.into_iter().collect()
}

store_matrix!(
    ack_parent_covers_done_children_and_is_idempotent,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let p = env.create_titled("p", None).await;
        let c1 = env.create_titled("c1", Some(p)).await;
        let c2 = env.create_titled("c2", Some(p)).await;
        let g = env.create_titled("g", Some(c1)).await;
        env.svc.cancel(Caller::Agent(&env.agent), c2).await.unwrap();
        for id in [g, c1, p] {
            env.claim(id).await;
            env.complete(id).await;
        }
        let p_before = env.get(p).await;

        env.clock.advance(Duration::minutes(5));
        let now = env.clock.now();
        let acked = env.svc.ack_done(&[p]).await.unwrap();
        assert_eq!(set(acked), set([p, c1, g]));
        for id in [p, c1, g] {
            assert_eq!(env.get(id).await.acked_at, Some(now));
        }
        assert_eq!(env.get(c2).await.acked_at, None);
        let p_after = env.get(p).await;
        assert_eq!(p_after.version, p_before.version);
        assert_eq!(p_after.updated_at, p_before.updated_at);
        assert_eq!(p_after.status, TaskStatus::Done);

        env.clock.advance(Duration::minutes(1));
        assert!(env.svc.ack_done(&[p]).await.unwrap().is_empty());
        assert_eq!(env.get(p).await.acked_at, Some(now));
    }
);

store_matrix!(
    ack_reaches_done_descendants_below_non_done_intermediates,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let p = env.create_titled("p", None).await;
        let c = env.create_titled("c", Some(p)).await;
        let g = env.create_titled("g", Some(c)).await;
        let open = env.create_titled("open", Some(c)).await;
        let claimed = env.create_titled("claimed", Some(g)).await;
        env.svc.cancel(Caller::Agent(&env.agent), c).await.unwrap();
        env.claim(claimed).await;
        for id in [g, p] {
            env.claim(id).await;
            env.complete(id).await;
        }

        env.clock.advance(Duration::minutes(5));
        let now = env.clock.now();
        let acked = env.svc.ack_done(&[p]).await.unwrap();
        assert_eq!(set(acked), set([p, g]));
        assert_eq!(env.get(g).await.acked_at, Some(now));
        for id in [c, open, claimed] {
            assert_eq!(env.get(id).await.acked_at, None);
        }
    }
);

store_matrix!(
    leaving_done_clears_ack,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let id = env.create().await;
        env.claim(id).await;
        env.set_time(Duration::hours(1));
        env.complete(id).await;
        env.set_time(Duration::hours(2));
        assert_eq!(env.svc.ack_done(&[id]).await.unwrap(), [id]);
        let acked = env.get(id).await;
        assert_eq!(acked.acked_at, Some(T0 + Duration::hours(2)));
        assert_eq!(acked.done_at, Some(T0 + Duration::hours(1)));

        reopen(&env, id, T0 + Duration::hours(3)).await;
        let after = env.get(id).await;
        assert_eq!((after.done_at, after.acked_at), (None, None));
    }
);

store_matrix!(
    ack_skips_unknown_and_unfinished,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let open = env.create().await;
        let done = env.finish("done", None, Duration::ZERO).await;
        let acked = env
            .svc
            .ack_done(&[TaskId::new(), open, done])
            .await
            .unwrap();
        assert_eq!(acked, [done]);
        assert_eq!(env.get(open).await.acked_at, None);
        assert!(env.svc.ack_done(&[]).await.unwrap().is_empty());
    }
);

store_matrix!(acked_tasks_leave_both_unseen_sets, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store).await;
    let old = env.finish("old", None, Duration::ZERO).await;
    let recent = env.finish("recent", None, Duration::days(9)).await;
    let now = T0 + Duration::days(10);
    let since = now - Duration::days(7);
    let count = |done| {
        let store = env.store.clone();
        async move {
            store
                .count_tasks(&TaskFilter {
                    done,
                    ..TaskFilter::default()
                })
                .await
                .unwrap()
        }
    };
    assert_eq!(count(DoneFilter::Unseen { since }).await, 1);
    assert_eq!(count(DoneFilter::AutoAckedUnseen { since }).await, 1);

    env.svc.ack_done(&[old, recent]).await.unwrap();
    assert_eq!(count(DoneFilter::Unseen { since }).await, 0);
    assert_eq!(count(DoneFilter::AutoAckedUnseen { since }).await, 0);
});

store_matrix!(
    ack_publishes_task_changed,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store.clone()).await;
        let bus = EventBus::new(store.clone(), env.clock.clone());
        let svc = TaskService::new(
            store.clone(),
            env.clock.clone(),
            Duration::hours(12),
            Duration::hours(24),
            Duration::seconds(90),
        )
        .with_events(bus.clone());
        let t = env.finish("t", None, Duration::ZERO).await;
        let mut rx = bus.subscribe();

        // Listing an id twice publishes once; an unfinished id publishes nothing.
        let open = env.create().await;
        svc.ack_done(&[t, open, t]).await.unwrap();
        match rx.recv().await.unwrap() {
            Event::TaskChanged {
                id,
                kind: TaskEventKind::Acked,
                status: TaskStatus::Done,
                ..
            } => assert_eq!(id, t),
            other => panic!("unexpected {other:?}"),
        }
        assert!(rx.try_recv().is_err());
        let kinds: Vec<_> = env
            .store
            .task_events(t)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.iter().all(|k| *k != TaskEventKind::Acked));
    }
);

fn titles(tasks: Vec<Task>) -> Vec<String> {
    tasks.into_iter().map(|t| t.title).collect()
}

fn done_with(done: DoneFilter) -> TaskFilter {
    TaskFilter {
        status: Some(TaskStatus::Done),
        done,
        ..TaskFilter::default()
    }
}

store_matrix!(
    unseen_and_auto_acked_split_at_exactly_n_days,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        env.finish("a", None, Duration::ZERO).await;
        env.finish("b", None, Duration::seconds(1)).await;
        let n = Duration::days(7);

        // Queried at exactly T0 + N: `a` is exactly N days old, so still unseen.
        let since = (T0 + n) - n;
        assert_eq!(
            titles(env.list(done_with(DoneFilter::Unseen { since })).await),
            ["a", "b"]
        );
        assert!(
            env.list(done_with(DoneFilter::AutoAckedUnseen { since }))
                .await
                .is_empty()
        );

        // One second later `a` has aged out; `b` is exactly N days old.
        let since = (T0 + n + Duration::seconds(1)) - n;
        assert_eq!(
            titles(env.list(done_with(DoneFilter::Unseen { since })).await),
            ["b"]
        );
        assert_eq!(
            titles(
                env.list(done_with(DoneFilter::AutoAckedUnseen { since }))
                    .await
            ),
            ["a"]
        );
    }
);

store_matrix!(changing_n_moves_tasks_without_writes, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store).await;
    let a = env.finish("a", None, Duration::ZERO).await;
    let now = T0 + Duration::days(5);
    let before = env.get(a).await;

    let count = |done| {
        let store = env.store.clone();
        async move {
            store
                .count_tasks(&TaskFilter {
                    done,
                    ..TaskFilter::default()
                })
                .await
                .unwrap()
        }
    };
    let auto = |d: i64| DoneFilter::AutoAckedUnseen {
        since: now - Duration::days(d),
    };
    assert_eq!(count(auto(7)).await, 0);
    assert_eq!(count(auto(3)).await, 1);
    assert_eq!(
        count(DoneFilter::Unseen {
            since: now - Duration::days(7)
        })
        .await,
        1
    );

    let after = env.get(a).await;
    assert_eq!(
        (after.version, after.updated_at, after.acked_at),
        (before.version, before.updated_at, None)
    );
});

store_matrix!(done_filter_leaves_other_statuses_alone, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store).await;
    env.create_titled("open one", None).await;
    let claimed = env.create_titled("claimed one", None).await;
    env.claim(claimed).await;
    env.finish("done long ago", None, Duration::ZERO).await;
    let now = T0 + Duration::days(30);

    let since = now - Duration::days(7);
    let got = env
        .list(TaskFilter {
            done: DoneFilter::Unseen { since },
            ..TaskFilter::default()
        })
        .await;
    assert_eq!(titles(got), ["open one", "claimed one"]);
});

store_matrix!(done_newest_first_top_level_paged, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store).await;
    let parent = env.create_titled("parent", None).await;
    let child = env.finish("child", Some(parent), Duration::hours(1)).await;
    // `y` and `z` finish in the same instant: ties break by id, descending.
    let x = env.finish("x", None, Duration::hours(2)).await;
    let y = env.create_titled("y", None).await;
    let z = env.create_titled("z", None).await;
    env.claim(y).await;
    env.claim(z).await;
    env.set_time(Duration::hours(3));
    env.complete(y).await;
    env.complete(z).await;
    let w = env.finish("w", None, Duration::hours(4)).await;
    assert_eq!(env.get(y).await.done_at, env.get(z).await.done_at);

    let mut f = TaskFilter {
        status: Some(TaskStatus::Done),
        top_level: true,
        order: TaskOrder::DoneNewest,
        limit: Some(2),
        ..TaskFilter::default()
    };
    let page1 = env.list(f.clone()).await;
    f.after = page1.last().map(|t| t.id);
    let page2 = env.list(f.clone()).await;
    f.after = page2.last().map(|t| t.id);
    let page3 = env.list(f.clone()).await;
    assert!(page3.is_empty());
    assert_eq!(page1.len(), 2);

    let all_ids: Vec<TaskId> = page1.iter().chain(&page2).map(|t| t.id).collect();
    assert_eq!(all_ids.len(), 4);
    assert_eq!(all_ids.iter().collect::<HashSet<_>>().len(), 4);
    assert!(!all_ids.contains(&child));
    let (hi, lo) = if y.to_string() > z.to_string() {
        (y, z)
    } else {
        (z, y)
    };
    assert_eq!(all_ids, [w, hi, lo, x]);

    // The count ignores `after`, `limit` and `order`.
    assert_eq!(env.store.count_tasks(&f).await.unwrap(), 4);

    // An unknown cursor id yields an empty page, like the `Created` order.
    f.after = Some(TaskId::new());
    assert!(env.list(f.clone()).await.is_empty());
    f.order = TaskOrder::Created;
    assert!(env.list(f).await.is_empty());
});

store_matrix!(
    title_search_is_literal_and_case_insensitive,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        for t in [
            "fix the flaky test",
            "100% done",
            "a_b",
            "axb",
            "back\\slash",
        ] {
            env.create_titled(t, None).await;
        }
        let search = |q: &str| {
            env.list(TaskFilter {
                title: Some(q.into()),
                ..TaskFilter::default()
            })
        };
        assert_eq!(titles(search("FLAKY").await), ["fix the flaky test"]);
        assert_eq!(titles(search("%").await), ["100% done"]);
        assert_eq!(titles(search("a_b").await), ["a_b"]);
        assert_eq!(titles(search("\\").await), ["back\\slash"]);
        assert_eq!(titles(search("k\\s").await), ["back\\slash"]);
    }
);

store_matrix!(count_matches_list, |store: Arc<dyn Store>| async move {
    let env = Env::new(store).await;
    let parent = env.create_titled("parent flaky", None).await;
    env.finish("child flaky", Some(parent), Duration::hours(1))
        .await;
    env.finish("old flaky", None, Duration::hours(2)).await;
    env.finish("recent", None, Duration::days(10)).await;
    env.create_titled("open", None).await;
    let now = T0 + Duration::days(10);
    let since = now - Duration::days(7);
    let project = env.agent.project.as_ref().unwrap().id;

    let filters = [
        TaskFilter::default(),
        TaskFilter {
            project: Some(project),
            ..TaskFilter::default()
        },
        TaskFilter {
            top_level: true,
            ..TaskFilter::default()
        },
        done_with(DoneFilter::Unseen { since }),
        done_with(DoneFilter::AutoAckedUnseen { since }),
        TaskFilter {
            done: DoneFilter::Unseen { since },
            ..TaskFilter::default()
        },
        TaskFilter {
            title: Some("FLAKY".into()),
            top_level: true,
            ..TaskFilter::default()
        },
    ];
    for f in filters {
        let n = env.list(f.clone()).await.len() as u64;
        assert_eq!(env.store.count_tasks(&f).await.unwrap(), n, "{f:?}");
    }
    assert_eq!(like_pattern(r"a%b_c\d"), r"%a\%b\_c\\d%");
});

store_matrix!(done_newest_only_returns_done_rows, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store).await;
    env.create_titled("open", None).await;
    let claimed = env.create_titled("claimed", None).await;
    env.claim(claimed).await;
    env.finish("done", None, Duration::hours(1)).await;
    // `status` unset: the order itself restricts to done rows (and none has a NULL done_at).
    let got = env
        .list(TaskFilter {
            order: TaskOrder::DoneNewest,
            ..TaskFilter::default()
        })
        .await;
    assert_eq!(titles(got), ["done"]);
});

/// The Done queries must use the partial index `tasks_done`, not scan and sort the table.
#[tokio::test]
async fn done_queries_use_the_tasks_done_index_sqlite() {
    use axle::store::sqlite::{SqliteStore, count_tasks_sql, list_tasks_sql};
    use sqlx::Row;
    use sqlx::sqlite::SqlitePoolOptions;

    let path = std::env::temp_dir().join(format!("axle-test-{}.db", uuid::Uuid::now_v7()));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    let store = SqliteStore::from_pool(pool.clone()).await.unwrap();
    let since = T0 - Duration::days(7);

    let plan = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                .fetch_all(&pool)
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<String, _>("detail"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let list = plan(list_tasks_sql(&TaskFilter {
        status: Some(TaskStatus::Done),
        top_level: true,
        done: DoneFilter::Unseen { since },
        order: TaskOrder::DoneNewest,
        ..TaskFilter::default()
    }))
    .await;
    let count = plan(count_tasks_sql(&TaskFilter {
        status: Some(TaskStatus::Done),
        done: DoneFilter::AutoAckedUnseen { since },
        ..TaskFilter::default()
    }))
    .await;
    assert!(list.contains("USING INDEX tasks_done"), "{list}");
    assert!(count.contains("USING INDEX tasks_done"), "{count}");

    drop(store);
    pool.close().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

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
async fn backfill_sets_done_at_and_acks_old_rows_sqlite() {
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
    let old_id = uuid::Uuid::now_v7();
    let recent_id = uuid::Uuid::now_v7();
    let open_id = uuid::Uuid::now_v7();
    let recent_at = OffsetDateTime::now_utc() - Duration::hours(12);
    // The store's fixed-width microsecond format.
    let recent_text = format!(
        "{}.{:06}Z",
        recent_at
            .format(format_description!(
                "[year]-[month]-[day]T[hour]:[minute]:[second]"
            ))
            .unwrap(),
        recent_at.microsecond()
    );
    for (id, status, updated) in [
        (old_id, "done", "2020-01-01T09:00:00.000000Z"),
        (recent_id, "done", recent_text.as_str()),
        (open_id, "open", "2020-01-01T09:00:00.000000Z"),
    ] {
        sqlx::query(
            "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at)
             VALUES (?, ?, 't', ?, '2019-12-01T00:00:00.000000Z', ?)",
        )
        .bind(id.to_string())
        .bind(&project)
        .bind(status)
        .bind(updated)
        .execute(&pool)
        .await
        .unwrap();
    }

    let store = SqliteStore::from_pool(pool).await.unwrap();
    let old = store.get_task(old_id.into()).await.unwrap().unwrap();
    let recent = store.get_task(recent_id.into()).await.unwrap().unwrap();
    let open = store.get_task(open_id.into()).await.unwrap().unwrap();
    // Older than the old 7-day board window: backfilled and acked.
    assert_eq!(old.done_at, Some(datetime!(2020-01-01 9:00 UTC)));
    assert_eq!(old.acked_at, old.done_at);
    // Finished within the last day: backfilled and still unseen.
    assert_eq!(
        recent.done_at.map(|t| t.unix_timestamp()),
        Some(recent_at.unix_timestamp())
    );
    assert_eq!(recent.acked_at, None);
    assert_eq!(open.done_at, None);
    assert_eq!(open.acked_at, None);

    drop(store);
    let _ = std::fs::remove_dir_all(dir);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[cfg(feature = "pg-tests")]
#[tokio::test]
async fn backfill_sets_done_at_and_acks_old_rows_postgres() {
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
            let old_id = uuid::Uuid::now_v7();
            let recent_id = uuid::Uuid::now_v7();
            let open_id = uuid::Uuid::now_v7();
            let long_ago = datetime!(2020-01-01 9:00 UTC);
            let recent_at = OffsetDateTime::now_utc() - Duration::hours(12);
            for (id, status, updated) in [
                (old_id, "done", long_ago),
                (recent_id, "done", recent_at),
                (open_id, "open", long_ago),
            ] {
                sqlx::query(
                    "INSERT INTO tasks (id, project_id, title, status, created_at, updated_at)
                     VALUES ($1, $2, 't', $3, '2019-12-01T00:00:00Z', $4)",
                )
                .bind(id)
                .bind(project)
                .bind(status)
                .bind(updated)
                .execute(&pool)
                .await
                .unwrap();
            }

            let store = PostgresStore::from_pool(pool).await.unwrap();
            let old = store.get_task(old_id.into()).await.unwrap().unwrap();
            let recent = store.get_task(recent_id.into()).await.unwrap().unwrap();
            let open = store.get_task(open_id.into()).await.unwrap().unwrap();
            // Older than the old 7-day board window: backfilled and acked.
            assert_eq!(old.done_at, Some(long_ago));
            assert_eq!(old.acked_at, old.done_at);
            // Finished within the last day: backfilled and still unseen.
            assert_eq!(
                recent.done_at.map(|t| t.unix_timestamp()),
                Some(recent_at.unix_timestamp())
            );
            assert_eq!(recent.acked_at, None);
            assert_eq!(open.done_at, None);
            assert_eq!(open.acked_at, None);
        }
    })
    .await;
    pool.close().await;
    let _ = std::fs::remove_dir_all(dir);
    schema.drop().await;
    common::resume(outcome);
}
