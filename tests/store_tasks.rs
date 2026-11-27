#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{
    Actor, Address, Eligibility, Importance, ProjectId, SessionId, Task, TaskEventKind, TaskId,
    TaskStatus,
};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{
    Caller, ClaimOutcome, Completion, NewTask, Progress, TaskError, TaskService,
};
use axle::store::{InboxFilter, Recipient, Store, TaskFilter};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-27 9:00 UTC);
const PROJECT: &str = "github.com/reyemtech/axle";

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
            Duration::minutes(15),
            Duration::hours(2),
            Duration::seconds(90),
        ));
        Self { store, clock, svc }
    }

    /// A new session of the agent `(PROJECT, host, harness)`.
    async fn session(&self, host: &str, harness: &str) -> Resolved {
        self.session_in(Some(PROJECT), host, harness).await
    }

    async fn session_in(&self, project: Option<&str>, host: &str, harness: &str) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: project.map(str::to_string),
                host: host.into(),
                harness: harness.into(),
                session: SessionId::new(),
                token: common::token(&*self.store).await,
            },
        )
        .await
        .unwrap()
    }

    async fn task(&self, by: &Resolved, eligibility: Eligibility) -> Task {
        self.svc
            .create(
                Caller::Agent(by),
                NewTask {
                    project_id: project_of(by),
                    title: "fix the flaky test".into(),
                    eligibility,
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    fn set_time(&self, offset: Duration) {
        let delta = T0 + offset - self.clock.now();
        self.clock.advance(delta);
    }
}

fn project_of(r: &Resolved) -> ProjectId {
    r.project.as_ref().expect("has a project").id
}

fn claimed(outcome: ClaimOutcome) -> Task {
    match outcome {
        ClaimOutcome::Claimed(t) => t,
        other => panic!("expected Claimed, got {other:?}"),
    }
}

async fn kinds(env: &Env, task: &Task) -> Vec<TaskEventKind> {
    env.svc
        .get(task.id)
        .await
        .unwrap()
        .events
        .iter()
        .map(|e| e.kind)
        .collect()
}

store_matrix!(concurrent_claims_have_one_winner, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let creator = env.session("creator", "claude-code").await;
    let task = env.task(&creator, Eligibility::default()).await;

    let mut agents = Vec::new();
    for i in 0..20 {
        agents.push(env.session(&format!("host-{i}"), "claude-code").await);
    }
    let handles: Vec<_> = agents
        .iter()
        .cloned()
        .map(|r| {
            let svc = env.svc.clone();
            tokio::spawn(async move { svc.claim(&r, task.id).await.unwrap() })
        })
        .collect();
    let mut outcomes = Vec::new();
    for h in handles {
        outcomes.push(h.await.unwrap());
    }

    let winners: Vec<&Task> = outcomes
        .iter()
        .filter_map(|o| match o {
            ClaimOutcome::Claimed(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(winners.len(), 1, "exactly one winner: {outcomes:?}");
    let winner = winners[0].owner_agent_id.unwrap();
    let lost = outcomes
        .iter()
        .filter(|o| {
            matches!(o, ClaimOutcome::AlreadyOwned { by, until }
                    if *by == winner && *until == Some(T0 + Duration::minutes(15)))
        })
        .count();
    assert_eq!(lost, 19, "{outcomes:?}");

    let stored = env.store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(stored.owner_agent_id, Some(winner));
    assert_eq!(stored.lease_epoch, 1);
    assert_eq!(
        kinds(&env, &task).await,
        vec![TaskEventKind::Created, TaskEventKind::Claimed]
    );
});

store_matrix!(lease_boundary, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session("linux", "claude-code").await;
    let task = env.task(&a, Eligibility::default()).await;

    let held = claimed(env.svc.claim(&a, task.id).await.unwrap());
    assert_eq!(held.lease_expires_at, Some(T0 + Duration::minutes(15)));

    env.set_time(Duration::minutes(15));
    assert_eq!(
        env.svc.claim(&b, task.id).await.unwrap(),
        ClaimOutcome::AlreadyOwned {
            by: a.agent.id,
            until: Some(T0 + Duration::minutes(15)),
        },
        "a lease is still owned at exactly its expiry instant"
    );

    env.set_time(Duration::minutes(15) + Duration::seconds(1));
    let taken = claimed(env.svc.claim(&b, task.id).await.unwrap());
    assert_eq!(taken.owner_session_id, Some(b.session.id));
    assert_eq!(taken.lease_epoch, 2);

    let events = env.svc.get(task.id).await.unwrap().events;
    let last = events.last().unwrap();
    assert_eq!(last.kind, TaskEventKind::LeaseExpiredTakeover);
    assert_eq!(last.actor, Actor::Agent(b.agent.id, b.session.id));
    let payload = last.payload.as_ref().unwrap();
    assert_eq!(payload["previous_agent_id"], a.agent.id.to_string());
    assert_eq!(payload["previous_session_id"], a.session.id.to_string());
    assert!(
        last.note
            .as_deref()
            .unwrap()
            .contains(&a.agent.id.to_string())
    );

    assert!(matches!(
        env.svc.renew(&a, task.id).await,
        Err(TaskError::LeaseLost)
    ));
});

store_matrix!(same_agent_takeover_after_stale_session, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await; // last heartbeat at t0
    let other = env.session("linux", "claude-code").await;
    let task = env.task(&a, Eligibility::default()).await;
    claimed(env.svc.claim(&a, task.id).await.unwrap());

    // Past the 90 s presence window the old session is stale: the `--resume` case.
    env.set_time(Duration::minutes(2));
    assert!(matches!(
        env.svc.claim(&other, task.id).await.unwrap(),
        ClaimOutcome::AlreadyOwned { .. }
    ));

    let a2 = env.session("mac", "claude-code").await;
    assert_eq!(a2.agent.id, a.agent.id);
    assert_ne!(a2.session.id, a.session.id);
    let taken = claimed(env.svc.claim(&a2, task.id).await.unwrap());
    assert_eq!(taken.owner_session_id, Some(a2.session.id));
    assert_eq!(taken.lease_expires_at, Some(T0 + Duration::minutes(17)));
    let last = env.svc.get(task.id).await.unwrap().events.pop().unwrap();
    assert_eq!(last.kind, TaskEventKind::Claimed);
    assert!(
        last.note.unwrap().starts_with("same-agent takeover"),
        "takeover is recorded"
    );

    assert!(matches!(
        env.svc.renew(&a, task.id).await,
        Err(TaskError::LeaseLost)
    ));
    assert!(matches!(
        env.svc.release(&a, task.id, None).await,
        Err(TaskError::LeaseLost)
    ));
    env.svc.renew(&a2, task.id).await.unwrap();
});

store_matrix!(same_agent_live_sibling_is_refused, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let blocked = env.task(&a, Eligibility::default()).await;
    let task = env.task(&a, Eligibility::default()).await;
    claimed(env.svc.claim(&a, task.id).await.unwrap());
    claimed(env.svc.claim(&a, blocked.id).await.unwrap());
    env.svc.block(&a, blocked.id, "waiting").await.unwrap();

    // A second live session of the same agent is refused like any other agent, for
    // claimed, blocked and review tasks alike.
    env.set_time(Duration::seconds(60));
    let sibling = env.session("mac", "claude-code").await;
    assert_eq!(sibling.agent.id, a.agent.id);
    assert_eq!(
        env.svc.claim(&sibling, task.id).await.unwrap(),
        ClaimOutcome::AlreadyOwned {
            by: a.agent.id,
            until: Some(T0 + Duration::minutes(15)),
        }
    );
    assert_eq!(
        env.svc.claim(&sibling, blocked.id).await.unwrap(),
        ClaimOutcome::AlreadyOwned {
            by: a.agent.id,
            until: None,
        }
    );

    // Heartbeats keep the owner live well past the window.
    env.set_time(Duration::minutes(10));
    env.store
        .touch_heartbeat(a.session.id, env.clock.now())
        .await
        .unwrap();
    env.set_time(Duration::minutes(11));
    assert!(matches!(
        env.svc.claim(&sibling, task.id).await.unwrap(),
        ClaimOutcome::AlreadyOwned { .. }
    ));

    // The owner itself always may claim again (a renewal).
    let again = claimed(env.svc.claim(&a, task.id).await.unwrap());
    assert_eq!(again.owner_session_id, Some(a.session.id));
    let stored = env.store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(stored.owner_session_id, Some(a.session.id));
});

store_matrix!(
    same_agent_two_sessions_race,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a1 = env.session("mac", "claude-code").await;
        let task = env.task(&a1, Eligibility::default()).await;
        claimed(env.svc.claim(&a1, task.id).await.unwrap());

        // a1 goes stale; two fresh sessions of the same agent race for its task.
        env.set_time(Duration::minutes(2));
        let a2 = env.session("mac", "claude-code").await;
        let a3 = env.session("mac", "claude-code").await;
        let spawn_claim = |r: Resolved| {
            let svc = env.svc.clone();
            tokio::spawn(async move { svc.claim(&r, task.id).await.unwrap() })
        };
        let (h2, h3) = (spawn_claim(a2.clone()), spawn_claim(a3.clone()));
        let outcomes = [h2.await.unwrap(), h3.await.unwrap()];

        // Exactly one takes it over; the other then meets a live owner and is refused.
        let winners: Vec<&Task> = outcomes
            .iter()
            .filter_map(|o| match o {
                ClaimOutcome::Claimed(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(winners.len(), 1, "exactly one owner: {outcomes:?}");
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, ClaimOutcome::AlreadyOwned { by, .. } if *by == a1.agent.id)),
            "{outcomes:?}"
        );
        let (owner, loser) = if winners[0].owner_session_id == Some(a2.session.id) {
            (&a2, &a3)
        } else {
            (&a3, &a2)
        };
        let stored = env.store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(stored.owner_session_id, Some(owner.session.id));
        assert_eq!(stored.lease_epoch, 2);

        assert!(matches!(
            env.svc.renew(loser, task.id).await,
            Err(TaskError::NotOwner)
        ));
        env.svc.renew(owner, task.id).await.unwrap();
    }
);

store_matrix!(
    blocked_and_review_never_expire,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let b = env.session("linux", "claude-code").await;
        let blocked = env.task(&a, Eligibility::default()).await;
        let review = env.task(&a, Eligibility::default()).await;
        claimed(env.svc.claim(&a, blocked.id).await.unwrap());
        claimed(env.svc.claim(&a, review.id).await.unwrap());

        assert!(matches!(
            env.svc.block(&a, blocked.id, "  ").await,
            Err(TaskError::ReasonRequired)
        ));
        let t = env
            .svc
            .block(&a, blocked.id, "waiting on CI")
            .await
            .unwrap();
        assert_eq!(t.status, TaskStatus::Blocked);
        assert_eq!(t.lease_expires_at, None);
        env.svc
            .to_review(&a, review.id, Some("ready".into()))
            .await
            .unwrap();

        env.set_time(Duration::hours(5));
        for task in [&blocked, &review] {
            assert_eq!(
                env.svc.claim(&b, task.id).await.unwrap(),
                ClaimOutcome::AlreadyOwned {
                    by: a.agent.id,
                    until: None
                }
            );
        }

        let block_event = env.svc.get(blocked.id).await.unwrap().events;
        assert_eq!(block_event.last().unwrap().kind, TaskEventKind::Blocked);
        assert_eq!(
            block_event.last().unwrap().note.as_deref(),
            Some("waiting on CI")
        );

        // The owner resumes a blocked task by claiming it again.
        let resumed = claimed(env.svc.claim(&a, blocked.id).await.unwrap());
        assert_eq!(resumed.status, TaskStatus::Claimed);
        assert_eq!(
            resumed.lease_expires_at,
            Some(T0 + Duration::hours(5) + Duration::minutes(15))
        );
    }
);

store_matrix!(
    heartbeat_renews_until_idle_cap,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await; // last tool call at t0
        let task = env.task(&a, Eligibility::default()).await;
        claimed(env.svc.claim(&a, task.id).await.unwrap());

        // Heartbeats every 10 minutes keep the lease alive up to t0+1h50m.
        for i in 1..=11 {
            env.set_time(Duration::minutes(10 * i));
            assert_eq!(env.svc.renew_by_heartbeat(a.session.id).await.unwrap(), 1);
        }
        let lease = |t: Task| t.lease_expires_at.unwrap();
        let until = T0 + Duration::minutes(110) + Duration::minutes(15);
        assert_eq!(
            lease(env.store.get_task(task.id).await.unwrap().unwrap()),
            until
        );

        // Past the 2h idle cap the heartbeat no longer renews.
        env.set_time(Duration::hours(2) + Duration::minutes(1));
        assert_eq!(env.svc.renew_by_heartbeat(a.session.id).await.unwrap(), 0);
        assert_eq!(
            lease(env.store.get_task(task.id).await.unwrap().unwrap()),
            until
        );

        // A tool call makes the session active again.
        env.store
            .touch_tool_call(a.session.id, env.clock.now())
            .await
            .unwrap();
        assert_eq!(env.svc.renew_by_heartbeat(a.session.id).await.unwrap(), 1);
        assert_eq!(
            lease(env.store.get_task(task.id).await.unwrap().unwrap()),
            T0 + Duration::hours(2) + Duration::minutes(16)
        );

        // Heartbeats never revive an expired lease.
        env.set_time(Duration::hours(3));
        env.store
            .touch_tool_call(a.session.id, env.clock.now())
            .await
            .unwrap();
        assert_eq!(env.svc.renew_by_heartbeat(a.session.id).await.unwrap(), 0);
    }
);

store_matrix!(eligibility_filters, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let claude = env.session("mac", "claude-code").await;
    let codex = env.session("mac", "codex").await;
    let homeless = env.session_in(None, "mac", "codex").await;
    let elsewhere = env
        .session_in(Some("github.com/x/other"), "mac", "codex")
        .await;
    let task = env
        .task(
            &claude,
            Eligibility {
                harness: Some("codex".into()),
                host: None,
            },
        )
        .await;

    assert_eq!(
        env.svc.claim(&claude, task.id).await.unwrap(),
        ClaimOutcome::NotEligible
    );
    assert_eq!(
        env.svc
            .claim_next(&claude, project_of(&claude))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        env.svc.claim(&homeless, task.id).await.unwrap(),
        ClaimOutcome::NotEligible,
        "no home project → not eligible for project-scoped claims"
    );
    assert_eq!(
        env.svc.claim(&elsewhere, task.id).await.unwrap(),
        ClaimOutcome::NotEligible,
        "agents of other projects are not eligible"
    );
    let next = env
        .svc
        .claim_next(&codex, project_of(&codex))
        .await
        .unwrap()
        .expect("codex is eligible");
    assert_eq!(next.id, task.id);
    assert_eq!(next.owner_agent_id, Some(codex.agent.id));
    assert_eq!(
        env.svc
            .claim(&codex, axle::domain::TaskId::new())
            .await
            .unwrap(),
        ClaimOutcome::NotFound
    );
});

store_matrix!(invalid_transition, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session("linux", "claude-code").await;
    let task = env.task(&a, Eligibility::default()).await;

    let err = env
        .svc
        .complete(&a, task.id, Completion::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            TaskError::InvalidTransition {
                from: TaskStatus::Open,
                to: TaskStatus::Done
            }
        ),
        "{err:?}"
    );

    claimed(env.svc.claim(&a, task.id).await.unwrap());
    assert!(matches!(
        env.svc.release(&b, task.id, None).await,
        Err(TaskError::NotOwner)
    ));
    assert!(matches!(
        env.svc.renew(&a, axle::domain::TaskId::new()).await,
        Err(TaskError::NotFound)
    ));
    env.svc
        .complete(&a, task.id, Completion::default())
        .await
        .unwrap();
    assert!(matches!(
        env.svc.claim(&b, task.id).await.unwrap(),
        ClaimOutcome::Closed(TaskStatus::Done)
    ));
});

store_matrix!(history_kept, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session("linux", "codex").await;
    let task = env.task(&a, Eligibility::default()).await;

    claimed(env.svc.claim(&a, task.id).await.unwrap());
    env.clock.advance(Duration::minutes(1));
    let released = env
        .svc
        .release(&a, task.id, Some("handing off".into()))
        .await
        .unwrap();
    assert_eq!(released.status, TaskStatus::Open);
    assert_eq!(released.owner_session_id, None);
    assert_eq!(released.lease_expires_at, None);
    env.clock.advance(Duration::minutes(1));
    claimed(env.svc.claim(&b, task.id).await.unwrap());
    env.clock.advance(Duration::minutes(1));
    let done = env
        .svc
        .complete(
            &b,
            task.id,
            Completion {
                summary: "fixed".into(),
                changed_files: vec!["src/lib.rs".into()],
                tests: vec!["cargo test".into()],
                links: vec![],
            },
        )
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Done);
    assert_eq!(done.owner_agent_id, Some(b.agent.id));

    let detail = env.svc.get(task.id).await.unwrap();
    let kinds: Vec<_> = detail.events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            TaskEventKind::Created,
            TaskEventKind::Claimed,
            TaskEventKind::Released,
            TaskEventKind::Claimed,
            TaskEventKind::Done,
        ]
    );
    assert_eq!(detail.events[2].note.as_deref(), Some("handing off"));
    let report = detail.events[4].payload.as_ref().unwrap();
    assert_eq!(report["summary"], "fixed");
    assert_eq!(report["changed_files"][0], "src/lib.rs");
    assert_eq!(detail.task, done);
});

store_matrix!(claim_next_concurrent_takes_distinct_tasks, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let creator = env.session("creator", "claude-code").await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(env.task(&creator, Eligibility::default()).await.id);
        env.clock.advance(Duration::seconds(1));
    }
    let project = project_of(&creator);

    let mut handles = Vec::new();
    for i in 0..6 {
        let r = env.session(&format!("host-{i}"), "claude-code").await;
        let svc = env.svc.clone();
        handles.push(tokio::spawn(async move {
            svc.claim_next(&r, project).await.unwrap()
        }));
    }
    let mut got: Vec<_> = Vec::new();
    for h in handles {
        if let Some(t) = h.await.unwrap() {
            got.push(t.id);
        }
    }
    got.sort();
    ids.sort();
    assert_eq!(got, ids, "each open task claimed exactly once");
});

store_matrix!(cancel_force_release_and_children, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session("linux", "claude-code").await;
    let parent = env.task(&a, Eligibility::default()).await;
    let child = env
        .svc
        .create(
            Caller::Human,
            NewTask {
                project_id: project_of(&a),
                parent_id: Some(parent.id),
                title: "child".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(child.created_by, Actor::Human);
    let detail = env.svc.get(parent.id).await.unwrap();
    assert_eq!(detail.children, vec![child.clone()]);

    // Only the human or the creating agent may cancel.
    assert!(matches!(
        env.svc.cancel(Caller::Agent(&b), parent.id).await,
        Err(TaskError::NotOwner)
    ));
    claimed(env.svc.claim(&b, child.id).await.unwrap());
    let reopened = env.svc.force_release(child.id).await.unwrap();
    assert_eq!(reopened.status, TaskStatus::Open);
    assert_eq!(reopened.owner_agent_id, None);
    assert_eq!(
        kinds(&env, &child).await.last(),
        Some(&TaskEventKind::ForceReleased)
    );

    let cancelled = env.svc.cancel(Caller::Agent(&a), parent.id).await.unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    env.svc.cancel(Caller::Human, child.id).await.unwrap();
    assert!(matches!(
        env.svc.cancel(Caller::Human, child.id).await,
        Err(TaskError::InvalidTransition {
            from: TaskStatus::Cancelled,
            to: TaskStatus::Cancelled
        })
    ));

    let open = env
        .svc
        .list(TaskFilter {
            project: Some(project_of(&a)),
            status: Some(TaskStatus::Cancelled),
            ..TaskFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(open.len(), 2);
});

store_matrix!(per_task_lease_ttl, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session("linux", "claude-code").await;
    let new = |ttl: Option<Duration>| NewTask {
        project_id: project_of(&a),
        title: "long migration".into(),
        lease_ttl: ttl,
        ..NewTask::default()
    };
    assert!(matches!(
        env.svc
            .create(Caller::Agent(&a), new(Some(Duration::milliseconds(500))))
            .await,
        Err(TaskError::InvalidLeaseTtl)
    ));
    let long = env
        .svc
        .create(Caller::Agent(&a), new(Some(Duration::hours(1))))
        .await
        .unwrap();
    assert_eq!(long.lease_ttl, Some(Duration::hours(1)));
    assert_eq!(
        env.store
            .get_task(long.id)
            .await
            .unwrap()
            .unwrap()
            .lease_ttl,
        Some(Duration::hours(1)),
        "persisted"
    );

    // claim uses the task's TTL
    let t = claimed(env.svc.claim(&a, long.id).await.unwrap());
    assert_eq!(t.lease_expires_at, Some(T0 + Duration::hours(1)));
    env.set_time(Duration::minutes(30));
    assert!(matches!(
        env.svc.claim(&b, long.id).await.unwrap(),
        ClaimOutcome::AlreadyOwned { .. }
    ));

    // explicit renew and heartbeat renewal use it too
    let t = env.svc.renew(&a, long.id).await.unwrap();
    assert_eq!(
        t.lease_expires_at,
        Some(T0 + Duration::minutes(30) + Duration::hours(1))
    );
    env.set_time(Duration::minutes(40));
    assert_eq!(env.svc.renew_by_heartbeat(a.session.id).await.unwrap(), 1);
    assert_eq!(
        env.store
            .get_task(long.id)
            .await
            .unwrap()
            .unwrap()
            .lease_expires_at,
        Some(T0 + Duration::minutes(40) + Duration::hours(1))
    );

    // claim_next uses it; a task without its own TTL gets the server's 15 min
    let short = env
        .svc
        .create(Caller::Agent(&a), new(Some(Duration::minutes(5))))
        .await
        .unwrap();
    let t = env
        .svc
        .claim_next(&b, project_of(&b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.id, short.id);
    assert_eq!(
        t.lease_expires_at,
        Some(T0 + Duration::minutes(40) + Duration::minutes(5))
    );
    let plain = env.svc.create(Caller::Agent(&a), new(None)).await.unwrap();
    assert_eq!(plain.lease_ttl, None);
    let t = claimed(env.svc.claim(&b, plain.id).await.unwrap());
    assert_eq!(
        t.lease_expires_at,
        Some(T0 + Duration::minutes(40) + Duration::minutes(15))
    );
});

store_matrix!(claim_next_takes_over_expired_claims, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session("linux", "claude-code").await;
    let abandoned = env.task(&a, Eligibility::default()).await;
    let project = project_of(&a);

    // a takes the task, then crashes (no renewals).
    let t = env.svc.claim_next(&a, project).await.unwrap().unwrap();
    assert_eq!(t.id, abandoned.id);

    env.set_time(Duration::minutes(15));
    assert_eq!(
        env.svc.claim_next(&b, project).await.unwrap(),
        None,
        "still owned at exactly the expiry instant"
    );

    // A newer open task exists too; the older abandoned one is handed out first.
    let newer = env.task(&a, Eligibility::default()).await;
    env.set_time(Duration::minutes(15) + Duration::seconds(1));
    let t = env.svc.claim_next(&b, project).await.unwrap().unwrap();
    assert_eq!(t.id, abandoned.id);
    assert_eq!(t.owner_session_id, Some(b.session.id));
    assert_eq!(t.lease_epoch, 2);

    let last = env.svc.get(abandoned.id).await.unwrap().events;
    let last = last.last().unwrap();
    assert_eq!(last.kind, TaskEventKind::LeaseExpiredTakeover);
    let payload = last.payload.as_ref().unwrap();
    assert_eq!(payload["previous_agent_id"], a.agent.id.to_string());
    assert_eq!(payload["previous_session_id"], a.session.id.to_string());

    assert!(matches!(
        env.svc.renew(&a, abandoned.id).await,
        Err(TaskError::LeaseLost)
    ));
    let t = env.svc.claim_next(&b, project).await.unwrap().unwrap();
    assert_eq!(t.id, newer.id);
    assert_eq!(
        kinds(&env, &newer).await,
        vec![TaskEventKind::Created, TaskEventKind::Claimed]
    );
});

async fn child_in(env: &Env, parent: &Task, project: ProjectId) -> Result<Task, TaskError> {
    env.svc
        .create(
            Caller::Human,
            NewTask {
                project_id: project,
                parent_id: Some(parent.id),
                title: "child".into(),
                ..NewTask::default()
            },
        )
        .await
}

async fn child_of(env: &Env, parent: &Task) -> Task {
    child_in(env, parent, parent.project_id).await.unwrap()
}

fn done(summary: &str) -> Completion {
    Completion {
        summary: summary.into(),
        ..Completion::default()
    }
}

store_matrix!(
    child_belongs_to_one_project,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let other = env
            .session_in(Some("github.com/reyemtech/other"), "mac", "codex")
            .await;
        let third = env
            .session_in(Some("github.com/reyemtech/third"), "mac", "codex")
            .await;
        let parent = env
            .svc
            .create(
                Caller::Agent(&a),
                NewTask {
                    project_id: project_of(&a),
                    title: "span two repos".into(),
                    extra_projects: vec![project_of(&other)],
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();

        let err = env
            .svc
            .create(
                Caller::Human,
                NewTask {
                    project_id: project_of(&a),
                    parent_id: Some(parent.id),
                    title: "bad".into(),
                    extra_projects: vec![project_of(&other)],
                    ..NewTask::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, TaskError::ChildExtraProjects));
        assert_eq!(err.to_string(), "child tasks belong to one project");

        // Home project and spanned projects are fine; others are not.
        child_in(&env, &parent, project_of(&a)).await.unwrap();
        child_in(&env, &parent, project_of(&other)).await.unwrap();
        assert!(matches!(
            child_in(&env, &parent, project_of(&third)).await,
            Err(TaskError::ChildProjectMismatch)
        ));

        // The parent must exist.
        let ghost = Task {
            id: TaskId::new(),
            ..parent.clone()
        };
        assert!(matches!(
            child_in(&env, &ghost, project_of(&a)).await,
            Err(TaskError::NotFound)
        ));
        assert_eq!(env.svc.get(parent.id).await.unwrap().children.len(), 2);
    }
);

store_matrix!(
    child_events_bubble_to_parent,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let b = env.session("linux", "claude-code").await;
        let parent = env.task(&a, Eligibility::default()).await;
        let c1 = child_of(&env, &parent).await;
        let c2 = child_of(&env, &parent).await;
        let c3 = child_of(&env, &parent).await;

        claimed(env.svc.claim(&b, c1.id).await.unwrap());
        env.svc.block(&b, c1.id, "waiting on schema").await.unwrap();
        claimed(env.svc.claim(&b, c2.id).await.unwrap());
        env.svc.complete(&b, c2.id, done("shipped")).await.unwrap();
        env.svc.cancel(Caller::Human, c3.id).await.unwrap();

        let detail = env.svc.get(parent.id).await.unwrap();
        let bubbled: Vec<_> = detail
            .events
            .iter()
            .filter(|e| e.kind != TaskEventKind::Created)
            .map(|e| {
                (
                    e.kind,
                    e.note.clone(),
                    e.payload.as_ref().unwrap()["child_id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            bubbled,
            vec![
                (TaskEventKind::ChildClaimed, None, c1.id.to_string()),
                (
                    TaskEventKind::ChildBlocked,
                    Some("waiting on schema".into()),
                    c1.id.to_string()
                ),
                (TaskEventKind::ChildClaimed, None, c2.id.to_string()),
                (
                    TaskEventKind::ChildDone,
                    Some("shipped".into()),
                    c2.id.to_string()
                ),
                (TaskEventKind::ChildCancelled, None, c3.id.to_string()),
            ]
        );
        // The actor is whoever changed the child.
        assert_eq!(
            detail.events[1].actor,
            Actor::Agent(b.agent.id, b.session.id)
        );
        assert_eq!(detail.events[5].actor, Actor::Human);
        // Renewals and the child's own created event do not bubble.
        assert_eq!(kinds(&env, &parent).await.len(), 6);
    }
);

store_matrix!(parent_moves_to_review_when_children_finish, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session("linux", "claude-code").await;
    let parent = env.task(&a, Eligibility::default()).await;
    claimed(env.svc.claim(&a, parent.id).await.unwrap());
    let c1 = child_of(&env, &parent).await;
    let c2 = child_of(&env, &parent).await;

    claimed(env.svc.claim(&b, c1.id).await.unwrap());
    env.svc.complete(&b, c1.id, done("one")).await.unwrap();
    // One child still open: the parent keeps running.
    let mid = env.svc.get(parent.id).await.unwrap();
    assert_eq!(mid.task.status, TaskStatus::Claimed);
    assert!(
        !mid.events
            .iter()
            .any(|e| e.kind == TaskEventKind::ChildrenComplete)
    );

    env.svc.cancel(Caller::Human, c2.id).await.unwrap();
    let detail = env.svc.get(parent.id).await.unwrap();
    assert_eq!(detail.task.status, TaskStatus::Review);
    assert_eq!(detail.task.owner_agent_id, Some(a.agent.id));
    assert_eq!(detail.task.lease_expires_at, None);
    assert_eq!(
        detail.events.last().map(|e| e.kind),
        Some(TaskEventKind::ChildrenComplete)
    );
    assert_eq!(
        detail
            .events
            .iter()
            .filter(|e| e.kind == TaskEventKind::ChildrenComplete)
            .count(),
        1
    );
    // The coordinator can still close it.
    let closed = env
        .svc
        .complete(&a, parent.id, done("all merged"))
        .await
        .unwrap();
    assert_eq!(closed.status, TaskStatus::Done);
});

store_matrix!(coordinator_is_notified_in_parent_thread, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let coordinator = env.session("mac", "claude-code").await;
    let worker = env.session("linux", "codex").await;
    let parent = env.task(&coordinator, Eligibility::default()).await;
    claimed(env.svc.claim(&coordinator, parent.id).await.unwrap());
    let c1 = child_of(&env, &parent).await;
    let c2 = child_of(&env, &parent).await;
    let c3 = child_of(&env, &parent).await;

    // Claiming is not a notification; blocked, done and cancelled are.
    claimed(env.svc.claim(&worker, c1.id).await.unwrap());
    claimed(env.svc.claim(&worker, c2.id).await.unwrap());
    env.svc.block(&worker, c1.id, "needs creds").await.unwrap();
    env.svc
        .complete(&worker, c2.id, done("shipped it"))
        .await
        .unwrap();
    env.svc.to_review(&worker, c1.id, None).await.unwrap();
    env.svc.complete(&worker, c1.id, done("")).await.unwrap();
    env.svc.cancel(Caller::Human, c3.id).await.unwrap();

    let thread = env.store.task_thread(parent.id).await.unwrap();
    // The first message is the task itself; the rest are the automatic notices.
    let notes: Vec<(Option<&str>, &str, Actor, Importance)> = thread[1..]
        .iter()
        .map(|m| (m.subject.as_deref(), m.body.as_str(), m.from, m.importance))
        .collect();
    let w = Actor::Agent(worker.agent.id, worker.session.id);
    let title = "child";
    assert_eq!(
        notes,
        vec![
            (
                Some("child task blocked"),
                format!("Child task \"{title}\" ({}) is blocked: needs creds", c1.id).as_str(),
                w,
                Importance::High,
            ),
            (
                Some("child task done"),
                format!("Child task \"{title}\" ({}) is done: shipped it", c2.id).as_str(),
                w,
                Importance::Normal,
            ),
            (
                Some("child task done"),
                format!("Child task \"{title}\" ({}) is done.", c1.id).as_str(),
                w,
                Importance::Normal,
            ),
            (
                Some("child task cancelled"),
                format!("Child task \"{title}\" ({}) is cancelled.", c3.id).as_str(),
                Actor::Human,
                Importance::Normal,
            ),
            (
                Some("child tasks complete"),
                "All child tasks are done or cancelled; the parent task is review.",
                Actor::Human,
                Importance::High,
            ),
        ]
    );
    for m in &thread[1..] {
        assert_eq!(m.to, Address::Task(parent.id));
        assert_eq!(m.task_id, Some(parent.id));
        assert_eq!(m.project_id, Some(parent.project_id));
    }

    // They reach the coordinator's inbox (the parent's owner and creator).
    let inbox = env
        .store
        .inbox(
            Recipient::Agent {
                agent: coordinator.agent.id,
                session: coordinator.session.id,
            },
            &InboxFilter::default(),
            env.clock.now(),
        )
        .await
        .unwrap();
    let subjects: Vec<_> = inbox
        .iter()
        .filter(|i| i.message.task_id == Some(parent.id))
        .filter_map(|i| i.message.subject.clone())
        .collect();
    assert_eq!(subjects.len(), 5, "{subjects:?}");
    assert_eq!(subjects.last().unwrap(), "child tasks complete");
});

store_matrix!(
    children_complete_with_sub_microsecond_clock,
    |store: Arc<dyn Store>| async move {
        // Stores keep microseconds; the clock here has nanoseconds, like SystemClock.
        let clock = Arc::new(TestClock::new(datetime!(2026-01-01 0:00:00.123456789 UTC)));
        let bus = axle::events::EventBus::new(store.clone(), clock.clone());
        let svc = Arc::new(
            TaskService::new(
                store.clone(),
                clock.clone(),
                Duration::minutes(15),
                Duration::hours(2),
                Duration::seconds(90),
            )
            .with_events(bus.clone()),
        );
        let env = Env {
            store: store.clone(),
            clock,
            svc,
        };
        let coordinator = env.session("mac", "claude-code").await;
        let worker = env.session("linux", "codex").await;
        let parent = env.task(&coordinator, Eligibility::default()).await;
        claimed(env.svc.claim(&coordinator, parent.id).await.unwrap());
        let child = child_of(&env, &parent).await;
        claimed(env.svc.claim(&worker, child.id).await.unwrap());
        let mut rx = bus.subscribe();
        env.svc
            .complete(&worker, child.id, done("ok"))
            .await
            .unwrap();

        let thread = env.store.task_thread(parent.id).await.unwrap();
        let subjects: Vec<_> = thread[1..]
            .iter()
            .filter_map(|m| m.subject.as_deref())
            .collect();
        assert_eq!(subjects, ["child task done", "child tasks complete"]);

        let mut kinds = vec![];
        while let Ok(e) = rx.try_recv() {
            if let axle::events::Event::TaskChanged {
                id, kind, status, ..
            } = e
            {
                kinds.push((id, kind, status));
            }
        }
        assert!(
            kinds.contains(&(
                parent.id,
                TaskEventKind::ChildrenComplete,
                TaskStatus::Review
            )),
            "{kinds:?}"
        );
    }
);

store_matrix!(parent_open_stays_open, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let parent = env.task(&a, Eligibility::default()).await;
    let c1 = child_of(&env, &parent).await;
    claimed(env.svc.claim(&a, c1.id).await.unwrap());
    env.svc.complete(&a, c1.id, done("done")).await.unwrap();

    let detail = env.svc.get(parent.id).await.unwrap();
    assert_eq!(detail.task.status, TaskStatus::Open);
    assert_eq!(
        detail.events.last().map(|e| e.kind),
        Some(TaskEventKind::ChildrenComplete)
    );
});

store_matrix!(progress_counts, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session("linux", "claude-code").await;
    let other = env
        .session_in(Some("github.com/reyemtech/other"), "mac", "codex")
        .await;
    let parent = env
        .svc
        .create(
            Caller::Agent(&a),
            NewTask {
                project_id: project_of(&a),
                title: "span".into(),
                extra_projects: vec![project_of(&other)],
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    let c1 = child_of(&env, &parent).await;
    let c2 = child_of(&env, &parent).await;
    let c3 = child_in(&env, &parent, project_of(&other)).await.unwrap();
    // A grandchild is not counted.
    let grandchild = child_of(&env, &c1).await;
    claimed(env.svc.claim(&a, grandchild.id).await.unwrap());

    claimed(env.svc.claim(&b, c1.id).await.unwrap());
    env.svc.complete(&b, c1.id, done("x")).await.unwrap();
    env.svc.cancel(Caller::Human, c2.id).await.unwrap();
    claimed(env.svc.claim(&other, c3.id).await.unwrap());

    let progress = env.svc.get(parent.id).await.unwrap().progress;
    assert_eq!((progress.done, progress.total), (2, 3));
    assert_eq!(
        progress.owners_by_project,
        vec![(project_of(&other), vec![other.agent.id])]
    );
    assert_eq!(
        env.svc.get(c3.id).await.unwrap().progress,
        Progress::default()
    );
});

store_matrix!(
    last_children_closing_concurrently_review_the_parent,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let parent = env.task(&a, Eligibility::default()).await;
        claimed(env.svc.claim(&a, parent.id).await.unwrap());
        let mut work = vec![];
        for i in 0..4 {
            let worker = env.session(&format!("host-{i}"), "claude-code").await;
            let child = child_of(&env, &parent).await;
            claimed(env.svc.claim(&worker, child.id).await.unwrap());
            work.push((worker, child));
        }
        let handles: Vec<_> = work
            .into_iter()
            .map(|(worker, child)| {
                let svc = env.svc.clone();
                tokio::spawn(async move {
                    svc.complete(&worker, child.id, done("ok")).await.unwrap();
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
        let detail = env.svc.get(parent.id).await.unwrap();
        assert_eq!(detail.task.status, TaskStatus::Review);
        assert_eq!(
            detail
                .events
                .iter()
                .filter(|e| e.kind == TaskEventKind::ChildrenComplete)
                .count(),
            1
        );
    }
);

store_matrix!(edit_is_guarded_attributed_and_published, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store.clone());
    let bus = axle::events::EventBus::new(store.clone(), env.clock.clone());
    let svc = TaskService::new(
        store.clone(),
        env.clock.clone(),
        Duration::minutes(15),
        Duration::hours(2),
        Duration::seconds(90),
    )
    .with_events(bus.clone());
    let mut rx = bus.subscribe();
    let creator = env.session("creator", "claude-code").await;
    let task = env.task(&creator, Eligibility::default()).await;

    // The creator may edit; the replaced value is kept and a TaskChanged is published.
    let edited = svc
        .edit(&creator, task.id, Some("renamed".into()), None)
        .await
        .unwrap();
    assert_eq!(edited.title, "renamed");
    assert_eq!(edited.body, task.body);
    assert_eq!(edited.version, task.version + 1);
    let last = svc.get(task.id).await.unwrap().events.pop().unwrap();
    assert_eq!(last.kind, TaskEventKind::Edited);
    assert_eq!(
        last.payload,
        Some(serde_json::json!({"previous_title": "fix the flaky test"}))
    );
    match rx.recv().await.unwrap() {
        axle::events::Event::TaskChanged { id, kind, .. } => {
            assert_eq!((id, kind), (task.id, TaskEventKind::Edited));
        }
        other => panic!("expected TaskChanged, got {other:?}"),
    }

    // Another agent may not, until its session holds the task.
    let worker = env.session("worker", "codex").await;
    assert!(matches!(
        svc.edit(&worker, task.id, None, Some("x".into())).await,
        Err(TaskError::NotOwner)
    ));
    claimed(svc.claim(&worker, task.id).await.unwrap());
    svc.edit(&worker, task.id, None, Some("worker body".into()))
        .await
        .unwrap();

    // A second session of the owning agent does not hold the lease, so it may not edit...
    let worker_again = env.session("worker", "codex").await;
    assert!(matches!(
        svc.edit(&worker_again, task.id, None, Some("y".into()))
            .await,
        Err(TaskError::NotOwner)
    ));
    // ...and once it takes the task over (the first session gone stale), the first session
    // has lost it.
    env.set_time(Duration::minutes(2));
    claimed(svc.claim(&worker_again, task.id).await.unwrap());
    assert!(matches!(
        svc.edit(&worker, task.id, None, Some("z".into())).await,
        Err(TaskError::LeaseLost)
    ));

    // Closed tasks are never edited, not even by their creator.
    svc.complete(
        &worker_again,
        task.id,
        Completion {
            summary: "done".into(),
            ..Completion::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        svc.edit(&creator, task.id, Some("late".into()), None).await,
        Err(TaskError::Closed(TaskStatus::Done))
    ));
    let after = store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(
        (after.title.as_str(), after.body.as_str()),
        ("renamed", "worker body")
    );
});

store_matrix!(
    task_list_pages_by_cursor,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store.clone());
        let creator = env.session("creator", "claude-code").await;
        let mut ids = vec![];
        for _ in 0..3 {
            ids.push(env.task(&creator, Eligibility::default()).await.id);
        }
        let page = |after: Option<TaskId>| {
            let store = store.clone();
            async move {
                store
                    .list_tasks(&TaskFilter {
                        after,
                        limit: Some(2),
                        ..TaskFilter::default()
                    })
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|t| t.id)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(page(None).await, ids[..2]);
        assert_eq!(page(Some(ids[1])).await, ids[2..]);
        assert_eq!(page(Some(ids[2])).await, Vec::<TaskId>::new());
    }
);

store_matrix!(
    task_events_paged_newest_first,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let me = env.session("mac", "claude-code").await;
        let task = env.task(&me, Eligibility::default()).await;
        env.clock.advance(Duration::seconds(1));
        assert!(matches!(
            env.svc.claim(&me, task.id).await.unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        for _ in 0..3 {
            env.clock.advance(Duration::seconds(1));
            env.svc.renew(&me, task.id).await.unwrap();
        }
        let kinds = |es: &[axle::domain::TaskEvent]| es.iter().map(|e| e.kind).collect::<Vec<_>>();
        let first = env
            .store
            .task_events_page(task.id, None, None, 2)
            .await
            .unwrap();
        assert_eq!(
            kinds(&first),
            [TaskEventKind::Renewed, TaskEventKind::Renewed]
        );
        let rest = env
            .store
            .task_events_page(task.id, None, Some(first[1].id), 10)
            .await
            .unwrap();
        assert_eq!(
            kinds(&rest),
            [
                TaskEventKind::Renewed,
                TaskEventKind::Claimed,
                TaskEventKind::Created
            ]
        );
        let claims = env
            .store
            .task_events_page(task.id, Some(TaskEventKind::Claimed), None, 10)
            .await
            .unwrap();
        assert_eq!(kinds(&claims), [TaskEventKind::Claimed]);
        // The whole history, oldest first, is the same events reversed.
        let all = env.store.task_events(task.id).await.unwrap();
        let mut paged: Vec<_> = first.into_iter().chain(rest).collect();
        paged.reverse();
        assert_eq!(paged, all);
    }
);
