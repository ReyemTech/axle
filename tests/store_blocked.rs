//! Blocked tasks say why and what unblocks them
//! (docs/superpowers/specs/2026-11-28-blocked-explained-design.md): the stored block detail,
//! updates while blocked, wait validation and the automatic unblock when the awaited task is
//! done.

#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{
    Address, Eligibility, MessageKind, SessionId, Task, TaskEventKind, TaskStatus, WaitingOn,
};
use axle::events::{Event, EventBus};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{
    BlockInput, Caller, ClaimOutcome, Completion, NewTask, TaskError, TaskService,
};
use axle::store::{BlockWrite, InboxFilter, LeaseUpdate, Recipient, Store, TransitionRequest};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-28 9:00 UTC);
const PROJECT: &str = "github.com/reyemtech/axle";
const TTL: Duration = Duration::minutes(15);

struct Env {
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    svc: Arc<TaskService>,
}

impl Env {
    fn new(store: Arc<dyn Store>) -> Self {
        let clock = Arc::new(TestClock::new(T0));
        let svc = Arc::new(TaskService::new(
            store.clone(),
            clock.clone(),
            TTL,
            Duration::hours(2),
            Duration::seconds(90),
        ));
        Self { store, clock, svc }
    }

    async fn session(&self, host: &str) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(PROJECT.into()),
                folder: None,
                host: host.into(),
                harness: "claude-code".into(),
                session: SessionId::new(),
                token: common::token(&*self.store).await,
            },
        )
        .await
        .unwrap()
    }

    async fn task(&self, by: &Resolved, title: &str, parent: Option<&Task>) -> Task {
        self.svc
            .create(
                Caller::Agent(by),
                NewTask {
                    project_id: by.project.as_ref().unwrap().id,
                    parent_id: parent.map(|p| p.id),
                    title: title.into(),
                    eligibility: Eligibility::default(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    /// A task `by` created and claimed.
    async fn claimed(&self, by: &Resolved, title: &str, parent: Option<&Task>) -> Task {
        let t = self.task(by, title, parent).await;
        match self.svc.claim(by, t.id).await.unwrap() {
            ClaimOutcome::Claimed(t) => t,
            other => panic!("expected Claimed, got {other:?}"),
        }
    }

    async fn get(&self, t: &Task) -> Task {
        self.store.get_task(t.id).await.unwrap().unwrap()
    }

    async fn wait(&self, by: &Resolved, t: &Task, on: WaitingOn) -> Result<Task, TaskError> {
        self.svc
            .block_with(
                by,
                t.id,
                BlockInput {
                    reason: Some("waiting".into()),
                    waiting_on: Some(on),
                    unblock: None,
                },
            )
            .await
    }

    async fn complete(&self, by: &Resolved, t: &Task) -> Task {
        self.svc
            .complete(
                by,
                t.id,
                Completion {
                    summary: "shipped".into(),
                    ..Completion::default()
                },
            )
            .await
            .unwrap()
    }
}

store_matrix!(block_detail_is_stored_and_cleared, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac").await;
    let t = env.claimed(&a, "deploy", None).await;
    let blocked = env
        .svc
        .block_with(
            &a,
            t.id,
            BlockInput {
                reason: Some("  needs the staging DB password\nsecond line ".into()),
                waiting_on: Some(WaitingOn::External("  ops ticket OPS-12 ".into())),
                unblock: Some("the password in the vault".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(blocked.status, TaskStatus::Blocked);
    let b = env.get(&t).await.block.expect("block detail stored");
    assert_eq!(b.reason, "needs the staging DB password\nsecond line");
    assert_eq!(b.since, Some(T0));
    assert_eq!(
        b.waiting_on,
        Some(WaitingOn::External("ops ticket OPS-12".into()))
    );
    assert_eq!(b.unblock.as_deref(), Some("the password in the vault"));
    let events = env.svc.get(t.id).await.unwrap().events;
    let last = events.last().unwrap();
    assert_eq!(last.kind, TaskEventKind::Blocked);
    assert_eq!(
        last.note.as_deref(),
        Some("needs the staging DB password\nsecond line")
    );
    assert_eq!(
        last.payload.as_ref().unwrap()["waiting_on"]["external"],
        "ops ticket OPS-12"
    );

    // Leaving `blocked` (here by reclaiming it) clears the detail; a new block starts empty.
    match env.svc.claim(&a, t.id).await.unwrap() {
        ClaimOutcome::Claimed(t) => assert_eq!(t.block, None),
        other => panic!("expected Claimed, got {other:?}"),
    }
    env.svc.block(&a, t.id, "again").await.unwrap();
    let b = env.get(&t).await.block.unwrap();
    assert_eq!(b.reason, "again");
    assert_eq!(b.waiting_on, None);
    assert_eq!(b.unblock, None);

    // To review, then a release: both clear it too.
    env.svc.to_review(&a, t.id, None).await.unwrap();
    assert_eq!(env.get(&t).await.block, None);

    // Unblock text is capped.
    let u = env.claimed(&a, "other", None).await;
    assert!(matches!(
        env.svc
            .block_with(
                &a,
                u.id,
                BlockInput {
                    reason: Some("x".into()),
                    waiting_on: None,
                    unblock: Some("u".repeat(501)),
                },
            )
            .await,
        Err(TaskError::InvalidField {
            field: "unblock",
            ..
        })
    ));
    assert_eq!(env.get(&u).await.status, TaskStatus::Claimed);
});

store_matrix!(
    owner_updates_a_blocked_task,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac").await;
        let other = env.session("linux").await;
        let t = env.claimed(&a, "deploy", None).await;
        env.svc.block(&a, t.id, "needs creds").await.unwrap();

        env.clock.advance(Duration::minutes(5));
        let updated = env
            .svc
            .block_with(
                &a,
                t.id,
                BlockInput {
                    reason: None,
                    waiting_on: Some(WaitingOn::Human),
                    unblock: Some("paste the token in the thread".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.status, TaskStatus::Blocked);
        let b = updated.block.unwrap();
        assert_eq!(
            b.reason, "needs creds",
            "an update keeps what it does not set"
        );
        assert_eq!(b.waiting_on, Some(WaitingOn::Human));
        assert_eq!(b.unblock.as_deref(), Some("paste the token in the thread"));
        assert_eq!(b.since, Some(T0), "blocked_at is when it entered blocked");

        let events = env.svc.get(t.id).await.unwrap().events;
        let last = events.last().unwrap();
        assert_eq!(last.kind, TaskEventKind::BlockUpdated);
        let p = last.payload.as_ref().unwrap();
        assert_eq!(p["previous_reason"], "needs creds");
        assert_eq!(p["previous_waiting_on"], serde_json::Value::Null);
        assert_eq!(p["waiting_on"]["human"], true);

        let reworded = env
            .svc
            .block_with(
                &a,
                t.id,
                BlockInput {
                    reason: Some("needs the prod token".into()),
                    ..BlockInput::default()
                },
            )
            .await
            .unwrap();
        let b = reworded.block.unwrap();
        assert_eq!(b.reason, "needs the prod token");
        assert_eq!(b.waiting_on, Some(WaitingOn::Human));

        // Nothing to change, or someone else: refused, nothing written.
        assert!(matches!(
            env.svc.block_with(&a, t.id, BlockInput::default()).await,
            Err(TaskError::NothingToUpdate)
        ));
        assert!(matches!(
            env.svc.block(&other, t.id, "mine now").await,
            Err(TaskError::NotOwner)
        ));
        assert_eq!(
            env.get(&t).await.block.unwrap().reason,
            "needs the prod token"
        );
    }
);

store_matrix!(waits_that_could_never_end_are_refused, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac").await;
    let x = env.claimed(&a, "x", None).await;
    let y = env.claimed(&a, "y", None).await;
    let z = env.claimed(&a, "z", None).await;

    assert!(matches!(
        env.wait(&a, &x, WaitingOn::Task(x.id)).await,
        Err(TaskError::WaitOnSelf)
    ));
    assert!(matches!(
        env.wait(&a, &x, WaitingOn::Task(axle::domain::TaskId::new()))
            .await,
        Err(TaskError::WaitTargetNotFound)
    ));

    // x waits on y, y waits on z: z waiting on x closes the loop.
    env.wait(&a, &x, WaitingOn::Task(y.id)).await.unwrap();
    env.wait(&a, &y, WaitingOn::Task(z.id)).await.unwrap();
    assert!(matches!(
        env.wait(&a, &z, WaitingOn::Task(x.id)).await,
        Err(TaskError::WaitCycle(id)) if id == x.id
    ));
    assert_eq!(env.get(&z).await.status, TaskStatus::Claimed);
    // An update that would close the loop is refused too.
    assert!(matches!(
        env.wait(&a, &y, WaitingOn::Task(x.id)).await,
        Err(TaskError::WaitCycle(_))
    ));
    assert_eq!(
        env.get(&y).await.block.unwrap().waiting_on,
        Some(WaitingOn::Task(z.id))
    );

    // The store enforces the rule itself, whatever the service checked.
    let raw = env
        .store
        .transition_task(&TransitionRequest {
            task: z.id,
            from: vec![TaskStatus::Claimed],
            to: Some(TaskStatus::Blocked),
            owner: None,
            lease: LeaseUpdate::Clear,
            clear_owner: false,
            now: env.clock.now(),
            token: None,
            event: axle::domain::TaskEvent {
                id: uuid::Uuid::now_v7(),
                task_id: z.id,
                at: env.clock.now(),
                actor: axle::domain::Actor::Human,
                kind: TaskEventKind::Blocked,
                note: Some("raw".into()),
                payload: None,
            },
            plan_from: None,
            plan_to: None,
            message: None,
            latest_plan: None,
            archive_plans: false,
            block: Some(BlockWrite {
                reason: Some("raw".into()),
                waiting_on: Some(WaitingOn::Task(x.id)),
                unblock: None,
                record_previous: false,
            }),
            unblock: None,
        })
        .await
        .unwrap();
    assert!(raw.is_none(), "a cycle is a guard miss");

    // A subtask may not wait on its parent; a parent may wait on its subtask.
    let parent = env.claimed(&a, "parent", None).await;
    let child = env.claimed(&a, "child", Some(&parent)).await;
    let grandchild = env.claimed(&a, "grandchild", Some(&child)).await;
    assert!(matches!(
        env.wait(&a, &grandchild, WaitingOn::Task(parent.id)).await,
        Err(TaskError::WaitOnAncestor)
    ));
    env.wait(&a, &parent, WaitingOn::Task(child.id))
        .await
        .unwrap();

    // A finished task never unblocks anything.
    let done = env.claimed(&a, "done", None).await;
    env.complete(&a, &done).await;
    let late = env.claimed(&a, "late", None).await;
    assert!(matches!(
        env.wait(&a, &late, WaitingOn::Task(done.id)).await,
        Err(TaskError::WaitOnClosed(TaskStatus::Done))
    ));
});

store_matrix!(
    done_unblocks_its_waiters,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac").await;
        let b = env.session("linux").await;
        let schema = env.claimed(&b, "schema", None).await;
        let api = env.claimed(&a, "api", None).await;
        let ui = env.claimed(&a, "ui", None).await;
        env.wait(&a, &api, WaitingOn::Task(schema.id))
            .await
            .unwrap();
        env.wait(&a, &ui, WaitingOn::Task(schema.id)).await.unwrap();

        env.clock.advance(Duration::seconds(30));
        env.complete(&b, &schema).await;
        let now = env.clock.now();
        for t in [&api, &ui] {
            let t = env.get(t).await;
            assert_eq!(t.status, TaskStatus::Claimed, "the owner's session is live");
            assert_eq!(t.owner_session_id, Some(a.session.id));
            assert_eq!(t.lease_expires_at, Some(now + TTL));
            assert_eq!(t.block, None);
            let events = env.svc.get(t.id).await.unwrap().events;
            let last = events.last().unwrap();
            assert_eq!(last.kind, TaskEventKind::Unblocked);
            assert!(
                last.note
                    .as_deref()
                    .unwrap()
                    .starts_with("unblocked: \"schema\"")
            );
            let note = env
                .store
                .task_thread(t.id)
                .await
                .unwrap()
                .into_iter()
                .last()
                .unwrap();
            assert_eq!(note.kind, MessageKind::System);
            assert_eq!(note.to, Address::Task(t.id));
            assert!(
                note.body
                    .starts_with(&format!("unblocked: \"schema\" ({}) is done", schema.id))
            );
        }

        // The owner's inbox carries the note, even when the owner finished the awaited task.
        let mine = env.claimed(&a, "mine", None).await;
        let waiter = env.claimed(&a, "waiter", None).await;
        env.wait(&a, &waiter, WaitingOn::Task(mine.id))
            .await
            .unwrap();
        env.complete(&a, &mine).await;
        let inbox = env
            .store
            .inbox(
                Recipient::Agent {
                    agent: a.agent.id,
                    session: a.session.id,
                },
                &InboxFilter {
                    task: Some(waiter.id),
                    ..InboxFilter::default()
                },
                env.clock.now(),
            )
            .await
            .unwrap();
        assert!(
            inbox
                .iter()
                .any(|i| i.message.body.starts_with("unblocked: \"mine\"")),
            "{inbox:?}"
        );
    }
);

store_matrix!(done_reopens_a_waiter_whose_owner_left, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac").await;
    let b = env.session("linux").await;
    let schema = env.claimed(&b, "schema", None).await;
    let api = env.claimed(&a, "api", None).await;
    env.wait(&a, &api, WaitingOn::Task(schema.id))
        .await
        .unwrap();

    // Both sessions go quiet; `b` still finishes its task (a guarded write, no heartbeat).
    env.clock.advance(Duration::minutes(10));
    env.complete(&b, &schema).await;
    let t = env.get(&api).await;
    assert_eq!(t.status, TaskStatus::Open);
    assert_eq!(t.owner_session_id, None);
    assert_eq!(t.owner_agent_id, None);
    assert_eq!(t.lease_expires_at, None);
    assert_eq!(t.block, None);
    let events = env.svc.get(t.id).await.unwrap().events;
    let last = events.last().unwrap();
    assert_eq!(last.kind, TaskEventKind::Unblocked);
    assert_eq!(last.payload.as_ref().unwrap()["reopened"], true);
    assert!(last.note.as_deref().unwrap().contains("open again"));
});

store_matrix!(
    unblocking_publishes_events,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store.clone());
        let bus = EventBus::new(store.clone(), env.clock.clone());
        let svc = TaskService::new(
            store.clone(),
            env.clock.clone(),
            TTL,
            Duration::hours(2),
            Duration::seconds(90),
        )
        .with_events(bus.clone());
        let a = env.session("mac").await;
        let schema = env.claimed(&a, "schema", None).await;
        let api = env.claimed(&a, "api", None).await;
        env.wait(&a, &api, WaitingOn::Task(schema.id))
            .await
            .unwrap();
        let mut rx = bus.subscribe();
        svc.complete(&a, schema.id, Completion::default())
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        assert!(
            seen.iter().any(|e| matches!(e, Event::TaskChanged {
            id, kind: TaskEventKind::Unblocked, status: TaskStatus::Claimed, ..
        } if *id == api.id)),
            "{seen:?}"
        );
        assert!(
            seen.iter().any(|e| matches!(e, Event::MessageCreated {
            to: Address::Task(t), ..
        } if *t == api.id)),
            "{seen:?}"
        );
    }
);

store_matrix!(a_cancelled_dependency_does_not_unblock, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac").await;
    let schema = env.claimed(&a, "schema", None).await;
    let api = env.claimed(&a, "api", None).await;
    env.wait(&a, &api, WaitingOn::Task(schema.id))
        .await
        .unwrap();
    env.svc.cancel(Caller::Agent(&a), schema.id).await.unwrap();

    let t = env.get(&api).await;
    assert_eq!(t.status, TaskStatus::Blocked, "cancelled is not done");
    assert_eq!(
        t.block.unwrap().waiting_on,
        Some(WaitingOn::Task(schema.id))
    );
    // The owner can still re-point it.
    env.wait(&a, &api, WaitingOn::External("a new plan".into()))
        .await
        .unwrap();
});

// Completing the last child locks the parent row (it moves to review) and then takes the
// wait lock; blocking the parent on a task takes the wait lock and then the parent row. Both
// at once must never deadlock.
store_matrix!(
    completing_a_child_while_its_parent_blocks_on_a_task,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac").await;
        let b = env.session("linux").await;
        for i in 0..20 {
            let parent = env.claimed(&a, &format!("parent {i}"), None).await;
            let child = env.claimed(&b, &format!("child {i}"), Some(&parent)).await;
            let other = env.task(&b, &format!("other {i}"), None).await;
            let (svc, by) = (env.svc.clone(), b.clone());
            let complete =
                tokio::spawn(
                    async move { svc.complete(&by, child.id, Completion::default()).await },
                );
            let (svc, by) = (env.svc.clone(), a.clone());
            let block = tokio::spawn(async move {
                svc.block_with(
                    &by,
                    parent.id,
                    BlockInput {
                        reason: Some("waiting".into()),
                        waiting_on: Some(WaitingOn::Task(other.id)),
                        unblock: None,
                    },
                )
                .await
            });
            complete.await.unwrap().expect("the child completes");
            match block.await.unwrap() {
                Ok(_) => {}
                // The child finished first and moved the parent to review.
                Err(TaskError::InvalidTransition {
                    from: TaskStatus::Review,
                    ..
                }) => {}
                Err(e) => panic!("iteration {i}: {e:?}"),
            }
        }
    }
);

// Each `block_updated` event records the values it actually replaced, even when two updates
// race.
store_matrix!(
    concurrent_block_updates_record_what_they_replaced,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac").await;
        for i in 0..10 {
            let t = env.claimed(&a, &format!("t {i}"), None).await;
            env.svc.block(&a, t.id, "r0").await.unwrap();
            let mut handles = Vec::new();
            for reason in ["r1", "r2"] {
                let (svc, by) = (env.svc.clone(), a.clone());
                handles.push(tokio::spawn(async move {
                    svc.block_with(
                        &by,
                        t.id,
                        BlockInput {
                            reason: Some(reason.into()),
                            ..BlockInput::default()
                        },
                    )
                    .await
                }));
            }
            for h in handles {
                h.await.unwrap().unwrap();
            }
            let updates: Vec<(String, String)> = env
                .svc
                .get(t.id)
                .await
                .unwrap()
                .events
                .into_iter()
                .filter(|e| e.kind == TaskEventKind::BlockUpdated)
                .map(|e| {
                    (
                        e.note.unwrap(),
                        e.payload.unwrap()["previous_reason"]
                            .as_str()
                            .unwrap()
                            .to_string(),
                    )
                })
                .collect();
            assert_eq!(updates.len(), 2, "{updates:?}");
            let current = env.get(&t).await.block.unwrap().reason;
            // One replaced r0; the other replaced what the first wrote, and is what is stored.
            let first = updates.iter().find(|(_, prev)| prev == "r0");
            let (first_new, _) = first.unwrap_or_else(|| panic!("iteration {i}: {updates:?}"));
            let second = updates
                .iter()
                .find(|(new, _)| new != first_new)
                .unwrap_or_else(|| panic!("iteration {i}: {updates:?}"));
            assert_eq!(&second.1, first_new, "iteration {i}: {updates:?}");
            assert_eq!(second.0, current, "iteration {i}: {updates:?}");
        }
    }
);

// Siblings A and B share a parent and B waits on A. Completing A locks the parent and then
// the waiter B; B's owner reclaiming B locks B and then the parent. Both at once must never
// deadlock.
store_matrix!(
    completing_a_sibling_while_its_waiter_is_reclaimed,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let p = env.session("lead").await;
        let a = env.session("mac").await;
        let b = env.session("linux").await;
        for i in 0..25 {
            let parent = env.claimed(&p, &format!("parent {i}"), None).await;
            let sib_a = env.claimed(&a, &format!("a {i}"), Some(&parent)).await;
            let sib_b = env.claimed(&b, &format!("b {i}"), Some(&parent)).await;
            env.wait(&b, &sib_b, WaitingOn::Task(sib_a.id))
                .await
                .unwrap();
            let (svc, by) = (env.svc.clone(), a.clone());
            let complete =
                tokio::spawn(
                    async move { svc.complete(&by, sib_a.id, Completion::default()).await },
                );
            let (svc, by) = (env.svc.clone(), b.clone());
            let claim = tokio::spawn(async move { svc.claim(&by, sib_b.id).await });
            complete.await.unwrap().expect("A completes");
            match claim.await.unwrap() {
                Ok(ClaimOutcome::Claimed(t)) => assert_eq!(t.status, TaskStatus::Claimed),
                other => panic!("iteration {i}: {other:?}"),
            }
            let t = env.get(&sib_b).await;
            assert_eq!(t.status, TaskStatus::Claimed, "iteration {i}");
            assert_eq!(t.block, None, "iteration {i}");
        }
    }
);
