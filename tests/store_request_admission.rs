//! Work-request admission: the dedupe lookup, the caps and the insert run in one guarded
//! transaction, so concurrent `task_request` calls cannot duplicate a card or pass a cap.

#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::TestClock;
use axle::config::ServerConfig;
use axle::domain::{
    Actor, Address, Eligibility, Importance, Message, MessageId, MessageKind, PlanState, ProjectId,
    SessionId, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus,
};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{NewRequest, TaskService};
use axle::store::{RequestAdmission, RequestInsert, Store, TaskFilter};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-28 9:00 UTC);

fn config() -> ServerConfig {
    ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap()
}

async fn agent(store: &Arc<dyn Store>, clock: &TestClock, project: &str, host: &str) -> Resolved {
    IdentityService::resolve(
        &**store,
        clock,
        IdentityInput {
            project_key: Some(project.into()),
            host: host.into(),
            harness: "claude-code".into(),
            session: SessionId::new(),
            token: common::token(&**store).await,
        },
    )
    .await
    .unwrap()
}

/// A `requested` task from `by` with dedupe key `key`, as the service would build it.
fn request(by: &Resolved, key: &str, at: OffsetDateTime) -> (Task, TaskEvent, Message) {
    let project = by.project.as_ref().unwrap().id;
    let actor = Actor::Agent(by.agent.id, by.session.id);
    let task = Task {
        id: TaskId::new(),
        project_id: project,
        parent_id: None,
        title: key.into(),
        body: String::new(),
        status: TaskStatus::Requested,
        owner_session_id: None,
        owner_agent_id: None,
        lease_expires_at: None,
        lease_epoch: 0,
        lease_ttl: None,
        eligibility: Eligibility::default(),
        created_by: actor,
        created_at: at,
        updated_at: at,
        done_at: None,
        acked_at: None,
        version: 0,
        requires_plan: false,
        plan_state: PlanState::None,
        sanctioned: false,
        request_depth: 1,
        origin_task_id: None,
        request_chain: vec![project],
        request_dedupe_key: Some(key.into()),
        block: None,
    };
    let event = TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: task.id,
        at,
        actor,
        kind: TaskEventKind::RequestCreated,
        note: None,
        payload: None,
    };
    let first = Message {
        id: MessageId::new(),
        project_id: Some(project),
        from: actor,
        to: Address::Task(task.id),
        subject: Some(task.title.clone()),
        body: String::new(),
        importance: Importance::Normal,
        task_id: Some(task.id),
        reply_to: None,
        created_at: at,
        kind: MessageKind::Message,
        options: Vec::new(),
    };
    (task, event, first)
}

fn admission(now: OffsetDateTime, per_hour: u32, pending_cap: u32) -> RequestAdmission {
    RequestAdmission {
        dedupe_since: now - Duration::minutes(10),
        rate_since: now - Duration::hours(1),
        per_hour,
        pending_cap,
    }
}

async fn admit(
    store: &Arc<dyn Store>,
    by: &Resolved,
    key: &str,
    at: OffsetDateTime,
    limits: RequestAdmission,
) -> (Task, RequestInsert) {
    let (task, event, first) = request(by, key, at);
    let out = store
        .insert_request(&task, None, &event, &first, &limits)
        .await
        .unwrap();
    (task, out)
}

async fn requested_in(store: &Arc<dyn Store>, project: ProjectId) -> usize {
    store
        .list_tasks(&TaskFilter {
            project: Some(project),
            status: Some(TaskStatus::Requested),
            ..TaskFilter::default()
        })
        .await
        .unwrap()
        .len()
}

store_matrix!(insert_request_dedupes_within_window, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock, "github.com/acme/a", "h").await;
    let (first, out) = admit(&store, &a, "k", T0, admission(T0, 10, 20)).await;
    assert_eq!(out, RequestInsert::Inserted);
    assert_eq!(store.get_task(first.id).await.unwrap().unwrap(), first);

    let later = T0 + Duration::minutes(9);
    let (dup, out) = admit(&store, &a, "k", later, admission(later, 10, 20)).await;
    assert_eq!(out, RequestInsert::Existing(Box::new(first.clone())));
    assert!(store.get_task(dup.id).await.unwrap().is_none());

    // Past the window the same key is a new request.
    let after = T0 + Duration::minutes(11);
    let (again, out) = admit(&store, &a, "k", after, admission(after, 10, 20)).await;
    assert_eq!(out, RequestInsert::Inserted);
    assert!(store.get_task(again.id).await.unwrap().is_some());
});

store_matrix!(insert_request_applies_the_hourly_rate, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock, "github.com/acme/a", "h").await;
    for i in 0..3 {
        let at = T0 + Duration::minutes(i);
        let (_, out) = admit(&store, &a, &format!("k{i}"), at, admission(at, 3, 20)).await;
        assert_eq!(out, RequestInsert::Inserted);
    }
    let at = T0 + Duration::minutes(5);
    let (refused, out) = admit(&store, &a, "k3", at, admission(at, 3, 20)).await;
    assert_eq!(out, RequestInsert::TooMany { oldest: T0 });
    assert!(store.get_task(refused.id).await.unwrap().is_none());
});

store_matrix!(insert_request_applies_the_pending_cap, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(T0);
    let a = agent(&store, &clock, "github.com/acme/a", "h1").await;
    let b = agent(&store, &clock, "github.com/acme/a", "h2").await;
    let project = a.project.as_ref().unwrap().id;
    for key in ["k0", "k1"] {
        let (_, out) = admit(&store, &a, key, T0, admission(T0, 10, 2)).await;
        assert_eq!(out, RequestInsert::Inserted);
    }
    let (refused, out) = admit(&store, &b, "k2", T0, admission(T0, 10, 2)).await;
    assert_eq!(out, RequestInsert::ProjectFull);
    assert!(store.get_task(refused.id).await.unwrap().is_none());
    assert_eq!(requested_in(&store, project).await, 2);
});

fn plain_request(project: ProjectId, title: &str) -> NewRequest {
    NewRequest {
        project_id: project,
        title: title.into(),
        body: "please".into(),
        eligibility: Eligibility::default(),
        importance: Importance::Normal,
        parent_id: None,
    }
}

const ROUNDS: usize = 20;
const RACERS: usize = 8;

store_matrix!(
    concurrent_identical_requests_insert_one_task,
    |store: Arc<dyn Store>| async move {
        let clock = Arc::new(TestClock::new(T0));
        let tasks = Arc::new(TaskService::from_config(
            store.clone(),
            clock.clone(),
            &config(),
        ));
        for round in 0..ROUNDS {
            let target = format!("github.com/acme/t{round}");
            let target_id = agent(&store, &clock, &target, "t")
                .await
                .project
                .unwrap()
                .id;
            let a = agent(&store, &clock, &format!("github.com/acme/r{round}"), "r").await;
            let mut racers = Vec::new();
            for _ in 0..RACERS {
                let (tasks, a) = (tasks.clone(), a.clone());
                racers.push(tokio::spawn(async move {
                    tasks
                        .request(&a, plain_request(target_id, "the same work"))
                        .await
                        .unwrap()
                        .task
                        .id
                }));
            }
            let mut ids = Vec::new();
            for r in racers {
                ids.push(r.await.unwrap());
            }
            assert!(
                ids.iter().all(|id| *id == ids[0]),
                "round {round}: every racer gets the one card"
            );
            assert_eq!(requested_in(&store, target_id).await, 1, "round {round}");
        }
    }
);

store_matrix!(concurrent_requests_stop_at_the_pending_cap, |store: Arc<
    dyn Store,
>| async move {
    let clock = Arc::new(TestClock::new(T0));
    let tasks = Arc::new(TaskService::from_config(
        store.clone(),
        clock.clone(),
        &config(),
    ));
    for round in 0..ROUNDS {
        let target = format!("github.com/acme/p{round}");
        let target_id = agent(&store, &clock, &target, "t")
            .await
            .project
            .unwrap()
            .id;
        // 18 of the project's 20 pending slots taken, by two agents under their rate.
        for (host, n) in [("f1", 9), ("f2", 9)] {
            let filler = agent(&store, &clock, &format!("github.com/acme/f{round}"), host).await;
            for i in 0..n {
                tasks
                    .request(&filler, plain_request(target_id, &format!("{host}-{i}")))
                    .await
                    .unwrap();
            }
        }
        let mut racers = Vec::new();
        for i in 0..RACERS {
            let a = agent(
                &store,
                &clock,
                &format!("github.com/acme/q{round}"),
                &format!("q{i}"),
            )
            .await;
            let tasks = tasks.clone();
            racers.push(tokio::spawn(async move {
                tasks
                    .request(&a, plain_request(target_id, &format!("race-{i}")))
                    .await
                    .is_ok()
            }));
        }
        let mut admitted = 0;
        for r in racers {
            admitted += usize::from(r.await.unwrap());
        }
        assert_eq!(admitted, 2, "round {round}: only the free slots are taken");
        assert_eq!(requested_in(&store, target_id).await, 20, "round {round}");
    }
});
