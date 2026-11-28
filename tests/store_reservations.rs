//! Per-session turn state, guarded wake reservations and the wake queue.

#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::TestClock;
use axle::domain::{
    Actor, Address, Eligibility, Importance, Message, MessageId, MessageKind, PlanState, ProjectId,
    SessionId, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus, TurnState, WakeChannel,
};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::store::{ClaimRequest, Claimant, LeaseUpdate, Reservation, Store, TransitionRequest};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-28 9:00 UTC);
const SETUP: OffsetDateTime = datetime!(2026-11-28 8:50 UTC);
const PROJECT: &str = "github.com/reyemtech/axle";

/// A session of the agent `(PROJECT, host, claude-code)`, created at `SETUP`.
async fn session_on(store: &Arc<dyn Store>, host: &str) -> Resolved {
    IdentityService::resolve(
        &**store,
        &TestClock::new(SETUP),
        IdentityInput {
            project_key: Some(PROJECT.into()),
            host: host.into(),
            harness: "claude-code".into(),
            session: SessionId::new(),
            token: common::token(&**store).await,
        },
    )
    .await
    .unwrap()
}

async fn session(store: &Arc<dyn Store>) -> Resolved {
    session_on(store, "host").await
}

/// Makes `s` wakeable, idle since `idle_at` and heartbeating at `beat_at`.
async fn idle(
    store: &Arc<dyn Store>,
    s: SessionId,
    idle_at: OffsetDateTime,
    beat_at: OffsetDateTime,
) {
    store
        .report_turn(
            s,
            Some(TurnState::Idle),
            Some(WakeChannel::ClaudeSocket),
            idle_at,
        )
        .await
        .unwrap();
    store.touch_heartbeat(s, beat_at).await.unwrap();
}

/// A reservation of `session` at `now` with the spec's windows (live 90 s, idle 5 s,
/// reserved for 2 minutes).
fn reservation(session: SessionId, task: Option<TaskId>, now: OffsetDateTime) -> Reservation {
    Reservation {
        session,
        task,
        now,
        until: now + Duration::minutes(2),
        live_since: now - Duration::seconds(90),
        idle_before: now - Duration::seconds(5),
        new_job: true,
    }
}

fn task_in(project: ProjectId, title: &str, sanctioned: bool) -> Task {
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
        created_at: SETUP,
        updated_at: SETUP,
        done_at: None,
        acked_at: None,
        version: 0,
        requires_plan: false,
        plan_state: PlanState::None,
        sanctioned,
        request_depth: 0,
        origin_task_id: None,
        request_chain: Vec::new(),
        request_dedupe_key: None,
        block: None,
    }
}

async fn insert(store: &Arc<dyn Store>, project: ProjectId, title: &str, sanctioned: bool) -> Task {
    let task = task_in(project, title, sanctioned);
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
        options: Vec::new(),
    };
    assert!(
        store
            .insert_task(&task, &[], None, &event, &first)
            .await
            .unwrap()
    );
    task
}

/// Records a `queued` event on an open task at `at`.
async fn queue(store: &Arc<dyn Store>, task: TaskId, at: OffsetDateTime) {
    let req = TransitionRequest {
        task,
        from: vec![TaskStatus::Open],
        to: None,
        owner: None,
        lease: LeaseUpdate::Keep,
        clear_owner: false,
        now: at,
        token: None,
        event: TaskEvent {
            id: uuid::Uuid::now_v7(),
            task_id: task,
            at,
            actor: Actor::Human,
            kind: TaskEventKind::Queued,
            note: None,
            payload: None,
        },
        plan_from: None,
        plan_to: None,
        message: None,
        latest_plan: None,
        archive_plans: false,
        version: None,
        sanctioned: None,
        block: None,
        unblock: None,
    };
    assert!(store.transition_task(&req).await.unwrap().is_some());
}

/// `who` claims `task` at `now` with a 15-minute lease.
async fn claim(store: &Arc<dyn Store>, who: &Resolved, task: TaskId, now: OffsetDateTime) {
    let req = ClaimRequest {
        claimant: Claimant {
            agent: who.agent.id,
            session: who.session.id,
            token: who.session.token_id,
            project: who.project.as_ref().map(|p| p.id),
            harness: "claude-code".into(),
            host: who.agent.host.clone(),
        },
        now,
        live_since: now - Duration::seconds(90),
        default_ttl: Duration::minutes(15),
        event_id: uuid::Uuid::now_v7(),
    };
    let attempt = store.claim_task(task, &req).await.unwrap();
    assert!(attempt.claimed.is_some(), "claim of {task} applied");
}

store_matrix!(racing_reservers_reserve_a_session_once, |store: Arc<
    dyn Store,
>| async move {
    let s = session(&store).await;
    let project = s.project.as_ref().unwrap().id;
    idle(&store, s.session.id, T0 - Duration::minutes(1), T0).await;
    let mut tasks = Vec::new();
    for i in 0..16 {
        tasks.push(insert(&store, project, &format!("t{i}"), true).await.id);
    }

    let mut racers = Vec::new();
    for task in tasks {
        let store = store.clone();
        let session = s.session.id;
        racers.push(tokio::spawn(async move {
            store
                .reserve_session(&reservation(session, Some(task), T0))
                .await
                .unwrap()
        }));
    }
    let mut won = 0;
    for r in racers {
        if r.await.unwrap() {
            won += 1;
        }
    }
    assert_eq!(won, 1);

    let got = store.get_session(s.session.id).await.unwrap().unwrap();
    assert!(got.wake_task_id.is_some());
    assert_eq!(got.wake_until, Some(T0 + Duration::minutes(2)));
    assert_eq!(got.last_woken_at, Some(T0));
});

store_matrix!(two_sessions_of_one_agent_reserve_apart, |store: Arc<
    dyn Store,
>| async move {
    let s1 = session(&store).await;
    let s2 = session(&store).await;
    assert_eq!(s1.agent.id, s2.agent.id);
    let project = s1.project.as_ref().unwrap().id;
    let t1 = insert(&store, project, "t1", true).await;
    let t2 = insert(&store, project, "t2", true).await;
    for s in [&s1, &s2] {
        idle(&store, s.session.id, T0 - Duration::minutes(1), T0).await;
    }

    assert!(
        store
            .reserve_session(&reservation(s1.session.id, Some(t1.id), T0))
            .await
            .unwrap()
    );
    assert!(
        store
            .reserve_session(&reservation(s2.session.id, Some(t2.id), T0))
            .await
            .unwrap()
    );
    let got1 = store.get_session(s1.session.id).await.unwrap().unwrap();
    let got2 = store.get_session(s2.session.id).await.unwrap().unwrap();
    assert_eq!(got1.wake_task_id, Some(t1.id));
    assert_eq!(got2.wake_task_id, Some(t2.id));
});

store_matrix!(
    stale_or_unknown_sessions_are_never_reserved,
    |store: Arc<dyn Store>| async move {
        let idle_long = T0 - Duration::minutes(1);

        let stale = session(&store).await;
        idle(
            &store,
            stale.session.id,
            idle_long,
            T0 - Duration::seconds(91),
        )
        .await;

        let unknown = session(&store).await;
        store
            .report_turn(
                unknown.session.id,
                None,
                Some(WakeChannel::ClaudeSocket),
                idle_long,
            )
            .await
            .unwrap();
        store.touch_heartbeat(unknown.session.id, T0).await.unwrap();

        let just_idle = session(&store).await;
        idle(&store, just_idle.session.id, T0 - Duration::seconds(4), T0).await;

        let unreachable = session(&store).await;
        store
            .report_turn(
                unreachable.session.id,
                Some(TurnState::Idle),
                None,
                idle_long,
            )
            .await
            .unwrap();
        store
            .touch_heartbeat(unreachable.session.id, T0)
            .await
            .unwrap();

        let busy = session(&store).await;
        idle(&store, busy.session.id, idle_long, T0).await;
        store
            .report_turn(
                busy.session.id,
                Some(TurnState::Busy),
                None,
                T0 - Duration::seconds(30),
            )
            .await
            .unwrap();

        for s in [&stale, &unknown, &just_idle, &unreachable, &busy] {
            assert!(
                !store
                    .reserve_session(&reservation(s.session.id, None, T0))
                    .await
                    .unwrap()
            );
            let got = store.get_session(s.session.id).await.unwrap().unwrap();
            assert_eq!(got.wake_until, None);
            assert_eq!(got.last_woken_at, None);
        }

        let free = session(&store).await;
        idle(
            &store,
            free.session.id,
            T0 - Duration::seconds(5),
            T0 - Duration::seconds(89),
        )
        .await;
        assert!(
            store
                .reserve_session(&reservation(free.session.id, None, T0))
                .await
                .unwrap()
        );
    }
);

store_matrix!(lease_blocks_new_job_not_own_task, |store: Arc<
    dyn Store,
>| async move {
    let s = session(&store).await;
    let project = s.project.as_ref().unwrap().id;
    let held = insert(&store, project, "held", true).await;
    let other = insert(&store, project, "other", true).await;
    claim(&store, &s, held.id, T0 - Duration::minutes(2)).await;
    idle(&store, s.session.id, T0 - Duration::minutes(1), T0).await;

    assert!(
        !store
            .reserve_session(&reservation(s.session.id, Some(other.id), T0))
            .await
            .unwrap()
    );
    let own = Reservation {
        new_job: false,
        ..reservation(s.session.id, Some(held.id), T0)
    };
    assert!(store.reserve_session(&own).await.unwrap());
});

store_matrix!(
    reservation_expires_and_frees,
    |store: Arc<dyn Store>| async move {
        let s = session(&store).await;
        let project = s.project.as_ref().unwrap().id;
        let t = insert(&store, project, "t", true).await;
        idle(&store, s.session.id, T0 - Duration::minutes(1), T0).await;
        assert!(
            store
                .reserve_session(&reservation(s.session.id, Some(t.id), T0))
                .await
                .unwrap()
        );

        let later = datetime!(2026-11-28 9:01:59 UTC);
        store.touch_heartbeat(s.session.id, later).await.unwrap();
        assert!(
            !store
                .reserve_session(&reservation(s.session.id, Some(t.id), later))
                .await
                .unwrap(),
            "still reserved"
        );
        assert!(store.expire_reservations(later).await.unwrap().is_empty());

        let at = datetime!(2026-11-28 9:02 UTC);
        assert_eq!(
            store.expire_reservations(at).await.unwrap(),
            vec![(s.session.id, Some(t.id))]
        );
        let got = store.get_session(s.session.id).await.unwrap().unwrap();
        assert_eq!(got.wake_task_id, None);
        assert_eq!(got.wake_until, None);
        assert_eq!(got.last_woken_at, Some(T0));
        assert!(store.expire_reservations(at).await.unwrap().is_empty());

        assert!(
            store
                .reserve_session(&reservation(s.session.id, Some(t.id), at))
                .await
                .unwrap()
        );
    }
);

store_matrix!(busy_report_clears_taskless_reservation, |store: Arc<
    dyn Store,
>| async move {
    let s = session(&store).await;
    let id = s.session.id;
    idle(&store, id, T0 - Duration::minutes(1), T0).await;
    assert!(
        store
            .reserve_session(&reservation(id, None, T0))
            .await
            .unwrap()
    );

    // An idle report changes nothing (same state, so `turn_state_at` stays).
    assert_eq!(
        store
            .report_turn(id, Some(TurnState::Idle), None, T0 + Duration::seconds(5))
            .await
            .unwrap(),
        None
    );
    let got = store.get_session(id).await.unwrap().unwrap();
    assert_eq!(got.turn_state_at, Some(T0 - Duration::minutes(1)));
    assert!(got.wake_until.is_some());

    let busy_at = T0 + Duration::seconds(10);
    assert_eq!(
        store
            .report_turn(id, Some(TurnState::Busy), None, busy_at)
            .await
            .unwrap(),
        Some(id)
    );
    let got = store.get_session(id).await.unwrap().unwrap();
    assert_eq!(got.turn_state, TurnState::Busy);
    assert_eq!(got.turn_state_at, Some(busy_at));
    assert_eq!(got.wake_channel, WakeChannel::ClaudeSocket);
    assert_eq!(got.wake_until, None);
    assert_eq!(got.wake_task_id, None);
    assert_eq!(got.last_woken_at, Some(T0));

    // A reservation for a task survives a busy report: the claim clears it.
    let project = s.project.as_ref().unwrap().id;
    let t = insert(&store, project, "t", true).await;
    let later = T0 + Duration::minutes(5);
    idle(&store, id, later - Duration::minutes(1), later).await;
    assert!(
        store
            .reserve_session(&reservation(id, Some(t.id), later))
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .report_turn(
                id,
                Some(TurnState::Busy),
                None,
                later + Duration::seconds(1)
            )
            .await
            .unwrap(),
        None
    );
    let got = store.get_session(id).await.unwrap().unwrap();
    assert_eq!(got.wake_task_id, Some(t.id));
    assert_eq!(store.clear_task_reservations(t.id).await.unwrap(), vec![id]);
    let got = store.get_session(id).await.unwrap().unwrap();
    assert_eq!(got.wake_task_id, None);
    assert_eq!(got.wake_until, None);

    assert!(matches!(
        store
            .report_turn(SessionId::new(), Some(TurnState::Busy), None, later)
            .await,
        Err(axle::store::StoreError::NotFound("session"))
    ));
});

store_matrix!(
    queue_is_fifo_by_queued_event,
    |store: Arc<dyn Store>| async move {
        let s = session(&store).await;
        let project = s.project.as_ref().unwrap().id;
        let t1 = insert(&store, project, "t1", true).await;
        let t2 = insert(&store, project, "t2", true).await;
        let unsanctioned = insert(&store, project, "not sanctioned", false).await;
        queue(&store, t2.id, T0).await;
        queue(&store, t1.id, T0 + Duration::minutes(1)).await;
        queue(&store, unsanctioned.id, T0 - Duration::minutes(5)).await;
        // Claimed at 8:50 with a 15-minute lease: back in the queue at 9:05.
        let lapsed = insert(&store, project, "lapsed", true).await;
        claim(&store, &s, lapsed.id, SETUP).await;
        let held = insert(&store, project, "held", true).await;
        claim(&store, &s, held.id, T0 + Duration::minutes(9)).await;

        let now = T0 + Duration::minutes(10);
        let ids = |tasks: Vec<Task>| tasks.into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(
            ids(store.list_queued(now).await.unwrap()),
            vec![t2.id, t1.id, lapsed.id]
        );
        assert_eq!(
            ids(store.list_queued(T0 + Duration::minutes(2)).await.unwrap()),
            vec![t2.id, t1.id],
            "lease not yet expired"
        );

        let free = session_on(&store, "other-host").await;
        idle(&store, free.session.id, now - Duration::minutes(1), now).await;
        assert!(
            store
                .reserve_session(&reservation(free.session.id, Some(t2.id), now))
                .await
                .unwrap()
        );
        assert_eq!(
            ids(store.list_queued(now).await.unwrap()),
            vec![t1.id, lapsed.id]
        );
        // Once the reservation lapses the task is queued again.
        assert_eq!(
            ids(store.list_queued(now + Duration::minutes(2)).await.unwrap())[0],
            t2.id
        );
        assert_eq!(
            store.clear_task_reservations(t2.id).await.unwrap(),
            vec![free.session.id]
        );
        assert_eq!(
            ids(store.list_queued(now).await.unwrap()),
            vec![t2.id, t1.id, lapsed.id]
        );
    }
);

store_matrix!(wake_candidates_least_recently_woken_first, |store: Arc<
    dyn Store,
>| async move {
    let project_task = |s: &Resolved| s.project.as_ref().unwrap().id;

    // Woken at 8:55 (reserved until 8:57), idle the longest.
    let woken = session_on(&store, "woken").await;
    let at = datetime!(2026-11-28 8:55 UTC);
    idle(&store, woken.session.id, SETUP, at).await;
    assert!(
        store
            .reserve_session(&reservation(woken.session.id, None, at))
            .await
            .unwrap()
    );
    // Never woken: idle since 8:58 and since 8:56.
    let recent = session_on(&store, "recent").await;
    idle(
        &store,
        recent.session.id,
        datetime!(2026-11-28 8:58 UTC),
        T0,
    )
    .await;
    let longer = session_on(&store, "longer").await;
    idle(
        &store,
        longer.session.id,
        datetime!(2026-11-28 8:56 UTC),
        T0,
    )
    .await;
    store.touch_heartbeat(woken.session.id, T0).await.unwrap();

    // Not candidates: reserved now, busy, stale, unreachable.
    let reserved = session_on(&store, "reserved").await;
    idle(&store, reserved.session.id, SETUP, T0).await;
    let t = insert(&store, project_task(&reserved), "t", true).await;
    assert!(
        store
            .reserve_session(&reservation(reserved.session.id, Some(t.id), T0))
            .await
            .unwrap()
    );
    let busy = session_on(&store, "busy").await;
    idle(&store, busy.session.id, SETUP, T0).await;
    store
        .report_turn(
            busy.session.id,
            Some(TurnState::Busy),
            None,
            SETUP + Duration::minutes(1),
        )
        .await
        .unwrap();
    let stale = session_on(&store, "stale").await;
    idle(&store, stale.session.id, SETUP, T0 - Duration::seconds(91)).await;
    let unreachable = session_on(&store, "unreachable").await;
    store
        .report_turn(unreachable.session.id, Some(TurnState::Idle), None, SETUP)
        .await
        .unwrap();
    store
        .touch_heartbeat(unreachable.session.id, T0)
        .await
        .unwrap();

    let got = store
        .wake_candidates(T0 - Duration::seconds(90), T0 - Duration::seconds(5), T0)
        .await
        .unwrap();
    let order: Vec<_> = got.iter().map(|(_, s)| s.id).collect();
    assert_eq!(
        order,
        vec![longer.session.id, recent.session.id, woken.session.id]
    );
    let (agent, _) = &got[0];
    assert_eq!(agent.id, longer.agent.id);
    assert_eq!(agent.host, "longer");
});
