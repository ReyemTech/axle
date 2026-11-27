//! The plan gate and append-only participating projects
//! (docs/superpowers/specs/2026-11-27-plan-gate-and-projects-design.md), on both backends.

#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{
    Actor, Address, Eligibility, Importance, MessageId, MessageKind, PlanState, ProjectId,
    SessionId, Task, TaskEventKind, TaskStatus,
};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{Caller, ClaimOutcome, Completion, NewTask, TaskError, TaskService};
use axle::store::{InboxFilter, Recipient, Store};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-27 9:00 UTC);
const PROJECT: &str = "github.com/acme/platform-infra";
const OTHER: &str = "github.com/acme/web-app";

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

    async fn session_in(&self, project: &str, host: &str) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(project.to_string()),
                host: host.into(),
                harness: "claude-code".into(),
                session: SessionId::new(),
                token: common::token(&*self.store).await,
            },
        )
        .await
        .unwrap()
    }

    async fn session(&self, host: &str) -> Resolved {
        self.session_in(PROJECT, host).await
    }

    /// A top-level task with the plan gate, created by the human.
    async fn gated(&self, project: ProjectId) -> Task {
        self.svc
            .create(
                Caller::Human,
                NewTask {
                    project_id: project,
                    title: "split infra and web_app".into(),
                    body: "do the thing".into(),
                    requires_plan: true,
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    async fn claimed(&self, by: &Resolved, task: &Task) -> Task {
        match self.svc.claim(by, task.id).await.unwrap() {
            ClaimOutcome::Claimed(t) => t,
            other => panic!("expected Claimed, got {other:?}"),
        }
    }

    async fn task(&self, task: &Task) -> Task {
        self.store.get_task(task.id).await.unwrap().unwrap()
    }

    async fn kinds(&self, task: &Task) -> Vec<TaskEventKind> {
        self.store
            .task_events(task.id)
            .await
            .unwrap()
            .iter()
            .map(|e| e.kind)
            .collect()
    }

    async fn inbox_of(&self, r: &Resolved) -> Vec<axle::domain::Message> {
        self.store
            .inbox(
                Recipient::Agent {
                    agent: r.agent.id,
                    session: r.session.id,
                },
                &InboxFilter::default(),
                self.clock.now(),
            )
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.message)
            .collect()
    }
}

fn project_of(r: &Resolved) -> ProjectId {
    r.project.as_ref().expect("has a project").id
}

fn not_approved(e: TaskError) -> PlanState {
    match e {
        TaskError::PlanNotApproved(state) => state,
        other => panic!("expected PlanNotApproved, got {other:?}"),
    }
}

store_matrix!(claiming_a_gated_task_starts_planning, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let lead = env.session("mac").await;
    let gated = env.gated(project_of(&lead)).await;
    assert!(gated.requires_plan);
    assert_eq!(gated.plan_state, PlanState::None);

    let t = env.claimed(&lead, &gated).await;
    assert_eq!(t.status, TaskStatus::Planning);
    assert_eq!(t.plan_state, PlanState::Drafting);
    assert_eq!(t.lease_expires_at, Some(T0 + Duration::minutes(15)));
    assert_eq!(t.owner_session_id, Some(lead.session.id));

    // A task without the gate is claimed as before.
    let plain = env
        .svc
        .create(
            Caller::Human,
            NewTask {
                project_id: project_of(&lead),
                title: "plain".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    let p = env.claimed(&lead, &plain).await;
    assert_eq!(p.status, TaskStatus::Claimed);
    assert_eq!(p.plan_state, PlanState::None);
});

store_matrix!(planning_refuses_work_but_allows_talk, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let lead = env.session("mac").await;
    let gated = env.gated(project_of(&lead)).await;
    env.claimed(&lead, &gated).await;

    let done = Completion {
        summary: "did it".into(),
        ..Completion::default()
    };
    assert_eq!(
        not_approved(env.svc.complete(&lead, gated.id, done).await.unwrap_err()),
        PlanState::Drafting
    );
    not_approved(env.svc.to_review(&lead, gated.id, None).await.unwrap_err());
    not_approved(env.svc.block(&lead, gated.id, "stuck").await.unwrap_err());
    let child = env
        .svc
        .create(
            Caller::Agent(&lead),
            NewTask {
                project_id: project_of(&lead),
                parent_id: Some(gated.id),
                title: "child".into(),
                ..NewTask::default()
            },
        )
        .await;
    not_approved(child.unwrap_err());
    assert_eq!(env.task(&gated).await.status, TaskStatus::Planning);

    // Renewing, editing and posting still work.
    env.clock.advance(Duration::minutes(5));
    let renewed = env.svc.renew(&lead, gated.id).await.unwrap();
    assert_eq!(renewed.lease_expires_at, Some(T0 + Duration::minutes(20)));
    assert_eq!(
        env.svc.renew_by_heartbeat(lead.session.id).await.unwrap(),
        1,
        "heartbeats renew a drafting plan's lease"
    );
    env.svc
        .edit(&lead, gated.id, None, Some("clarified".into()))
        .await
        .unwrap();
});

store_matrix!(proposing_a_plan_submits_it_to_the_human, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let lead = env.session("mac").await;
    let other = env.session("linux").await;
    let gated = env.gated(project_of(&lead)).await;

    // Not while open, and only the owner.
    assert!(matches!(
        env.svc.propose_plan(&lead, gated.id, "plan".into()).await,
        Err(TaskError::NotPlanning(TaskStatus::Open))
    ));
    env.claimed(&lead, &gated).await;
    assert!(matches!(
        env.svc.propose_plan(&other, gated.id, "plan".into()).await,
        Err(TaskError::NotOwner)
    ));
    assert!(matches!(
        env.svc.propose_plan(&lead, gated.id, "  ".into()).await,
        Err(TaskError::EmptyPlan)
    ));

    let (t, plan) = env
        .svc
        .propose_plan(&lead, gated.id, "1. infra: x\n2. web_app: y".into())
        .await
        .unwrap();
    assert_eq!(t.status, TaskStatus::Planning);
    assert_eq!(t.plan_state, PlanState::Submitted);
    assert_eq!(t.lease_expires_at, None, "a submitted plan has no lease");
    assert_eq!(plan.kind, MessageKind::Plan);
    assert_eq!(plan.to, Address::Human);
    assert_eq!(plan.importance, Importance::High);
    assert_eq!(plan.task_id, Some(gated.id));

    let thread = env.store.task_thread(gated.id).await.unwrap();
    assert_eq!(thread.last().unwrap().id, plan.id);
    assert_eq!(thread.last().unwrap().kind, MessageKind::Plan);
    let human = env
        .store
        .inbox(Recipient::Human, &InboxFilter::default(), env.clock.now())
        .await
        .unwrap();
    assert!(human.iter().any(|i| i.message.id == plan.id));
    assert!(
        env.kinds(&gated)
            .await
            .contains(&TaskEventKind::PlanSubmitted)
    );

    // A second plan supersedes the first; both stay in the thread.
    let (_, second) = env
        .svc
        .propose_plan(&lead, gated.id, "revised".into())
        .await
        .unwrap();
    let plans: Vec<_> = env
        .store
        .task_thread(gated.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.kind == MessageKind::Plan)
        .collect();
    assert_eq!(plans.len(), 2);
    assert_eq!(plans.last().unwrap().id, second.id);

    // A claimed task is not planning.
    let plain = env
        .svc
        .create(
            Caller::Human,
            NewTask {
                project_id: project_of(&lead),
                title: "plain".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    env.claimed(&lead, &plain).await;
    assert!(matches!(
        env.svc.propose_plan(&lead, plain.id, "plan".into()).await,
        Err(TaskError::NotPlanning(TaskStatus::Claimed))
    ));
});

store_matrix!(a_submitted_plan_lease_does_not_expire, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let lead = env.session("mac").await;
    let other = env.session("linux").await;
    let gated = env.gated(project_of(&lead)).await;
    env.claimed(&lead, &gated).await;
    env.svc
        .propose_plan(&lead, gated.id, "plan".into())
        .await
        .unwrap();

    env.clock.advance(Duration::hours(6));
    assert!(matches!(
        env.svc.claim(&other, gated.id).await.unwrap(),
        ClaimOutcome::AlreadyOwned { until: None, .. }
    ));
    assert!(
        env.svc
            .claim_next(&other, project_of(&other))
            .await
            .unwrap()
            .is_none()
    );
    // Neither a heartbeat nor an explicit renewal starts a lease on it.
    env.store
        .touch_tool_call(lead.session.id, env.clock.now())
        .await
        .unwrap();
    assert_eq!(
        env.svc.renew_by_heartbeat(lead.session.id).await.unwrap(),
        0
    );
    let renewed = env.svc.renew(&lead, gated.id).await.unwrap();
    assert_eq!(renewed.lease_expires_at, None);
    assert_eq!(renewed.plan_state, PlanState::Submitted);
    let t = env.task(&gated).await;
    assert_eq!(t.owner_session_id, Some(lead.session.id));
    assert_eq!(t.status, TaskStatus::Planning);
});

store_matrix!(
    approving_a_plan_starts_the_work,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let lead = env.session("mac").await;
        let gated = env.gated(project_of(&lead)).await;
        env.claimed(&lead, &gated).await;
        assert!(matches!(
            env.svc.approve_plan(gated.id, MessageId::new()).await,
            Err(TaskError::NoPlanToReview(PlanState::Drafting))
        ));
        let (_, plan) = env
            .svc
            .propose_plan(&lead, gated.id, "plan".into())
            .await
            .unwrap();

        env.clock.advance(Duration::minutes(30));
        let t = env.svc.approve_plan(gated.id, plan.id).await.unwrap();
        assert_eq!(t.status, TaskStatus::Claimed);
        assert_eq!(t.plan_state, PlanState::Approved);
        assert_eq!(
            t.lease_expires_at,
            Some(T0 + Duration::minutes(45)),
            "approval starts a fresh lease"
        );
        assert!(
            env.kinds(&gated)
                .await
                .contains(&TaskEventKind::PlanApproved)
        );

        let mail = env.inbox_of(&lead).await;
        let approval = mail
            .iter()
            .find(|m| m.to == Address::Session(lead.session.id))
            .expect("the owner session is told");
        assert_eq!(approval.kind, MessageKind::System);
        assert_eq!(approval.from, Actor::Human);
        assert_eq!(approval.task_id, Some(gated.id));

        // Approving twice is refused; the work can now finish.
        assert!(matches!(
            env.svc.approve_plan(gated.id, plan.id).await,
            Err(TaskError::InvalidTransition { .. })
        ));
        env.svc
            .complete(
                &lead,
                gated.id,
                Completion {
                    summary: "done".into(),
                    ..Completion::default()
                },
            )
            .await
            .unwrap();
    }
);

store_matrix!(requesting_changes_keeps_planning, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let lead = env.session("mac").await;
    let other = env.session("linux").await;
    let gated = env.gated(project_of(&lead)).await;
    env.claimed(&lead, &gated).await;
    let (_, plan) = env
        .svc
        .propose_plan(&lead, gated.id, "plan".into())
        .await
        .unwrap();
    assert!(matches!(
        env.svc.request_plan_changes(gated.id, plan.id, " ").await,
        Err(TaskError::NoteRequired)
    ));

    let t = env
        .svc
        .request_plan_changes(gated.id, plan.id, "split the web_app part")
        .await
        .unwrap();
    assert_eq!(t.status, TaskStatus::Planning);
    assert_eq!(t.plan_state, PlanState::ChangesRequested);
    assert_eq!(t.lease_expires_at, Some(T0 + Duration::minutes(15)));
    let mail = env.inbox_of(&lead).await;
    assert!(
        mail.iter().any(
            |m| m.body == "split the web_app part" && m.to == Address::Session(lead.session.id)
        )
    );
    not_approved(
        env.svc
            .complete(&lead, gated.id, Completion::default())
            .await
            .unwrap_err(),
    );

    // The normal lease applies again: once it lapses another agent may take it over, and
    // it stays in planning with the request still standing.
    env.clock.advance(Duration::minutes(16));
    let taken = env.claimed(&other, &gated).await;
    assert_eq!(taken.status, TaskStatus::Planning);
    assert_eq!(taken.plan_state, PlanState::ChangesRequested);
    assert_eq!(taken.owner_session_id, Some(other.session.id));
    env.svc
        .propose_plan(&other, gated.id, "revised".into())
        .await
        .unwrap();
});

store_matrix!(release_resets_the_plan_but_keeps_the_gate, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let lead = env.session("mac").await;
    let gated = env.gated(project_of(&lead)).await;
    env.claimed(&lead, &gated).await;
    env.svc
        .propose_plan(&lead, gated.id, "plan".into())
        .await
        .unwrap();

    let t = env.svc.release(&lead, gated.id, None).await.unwrap();
    assert_eq!(t.status, TaskStatus::Open);
    assert_eq!(t.plan_state, PlanState::None);
    assert!(t.requires_plan);
    assert_eq!(t.owner_session_id, None);
    let again = env.claimed(&lead, &gated).await;
    assert_eq!(again.status, TaskStatus::Planning);
    assert_eq!(again.plan_state, PlanState::Drafting);
    assert_eq!(
        env.store
            .task_thread(gated.id)
            .await
            .unwrap()
            .iter()
            .filter(|m| m.kind == MessageKind::Plan)
            .count(),
        1,
        "old plans stay in the thread"
    );

    // The human may force-release or cancel a planning task.
    let t = env.svc.force_release(gated.id).await.unwrap();
    assert_eq!(
        (t.status, t.plan_state),
        (TaskStatus::Open, PlanState::None)
    );
    env.claimed(&lead, &gated).await;
    let t = env.svc.cancel(Caller::Human, gated.id).await.unwrap();
    assert_eq!(t.status, TaskStatus::Cancelled);
});

store_matrix!(
    projects_are_appended_by_creator_owner_or_human,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let creator = env.session("mac").await;
        let owner = env.session("linux").await;
        let stranger = env.session("pi").await;
        let web_app = env.session_in(OTHER, "mac").await;
        let third = env.session_in("github.com/reyemtech/third", "mac").await;
        let home = project_of(&creator);
        let task = env
            .svc
            .create(
                Caller::Agent(&creator),
                NewTask {
                    project_id: home,
                    title: "cross-project".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();
        env.claimed(&owner, &task).await;

        assert!(matches!(
            env.svc
                .add_projects(Caller::Agent(&stranger), task.id, &[project_of(&web_app)])
                .await,
            Err(TaskError::NotOwner)
        ));
        assert!(matches!(
            env.svc
                .add_projects(Caller::Human, task.id, &[ProjectId::new()])
                .await,
            Err(TaskError::UnknownProject)
        ));

        let t = env
            .svc
            .add_projects(Caller::Agent(&creator), task.id, &[project_of(&web_app)])
            .await
            .unwrap();
        assert!(t.version > task.version);
        assert_eq!(
            env.store.task_projects(task.id).await.unwrap(),
            vec![project_of(&web_app)]
        );
        let note = env.store.task_thread(task.id).await.unwrap();
        let note = note.last().unwrap();
        assert_eq!(note.kind, MessageKind::System);
        assert!(note.body.contains("web-app"), "{}", note.body);
        let added = env
            .store
            .task_events(task.id)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == TaskEventKind::ProjectsAdded)
            .unwrap();
        assert_eq!(
            added.payload.unwrap()["added_projects"][0],
            project_of(&web_app).to_string()
        );

        // Already listed (or the home project): a no-op with no new event.
        let events = env.kinds(&task).await.len();
        env.svc
            .add_projects(
                Caller::Agent(&owner),
                task.id,
                &[project_of(&web_app), home],
            )
            .await
            .unwrap();
        assert_eq!(env.kinds(&task).await.len(), events);

        // The owner and the human may add too.
        env.svc
            .add_projects(Caller::Agent(&owner), task.id, &[project_of(&third)])
            .await
            .unwrap();
        assert_eq!(env.store.task_projects(task.id).await.unwrap().len(), 2);

        // A child in the added project is now valid; a child cannot take projects.
        let child = env
            .svc
            .create(
                Caller::Agent(&owner),
                NewTask {
                    project_id: project_of(&web_app),
                    parent_id: Some(task.id),
                    title: "web_app part".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            env.svc
                .add_projects(Caller::Human, child.id, &[project_of(&third)])
                .await,
            Err(TaskError::ChildExtraProjects)
        ));
        let _ = Eligibility::default();
    }
);

store_matrix!(
    a_decision_names_the_plan_it_saw,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let lead = env.session("mac").await;
        let gated = env.gated(project_of(&lead)).await;
        env.claimed(&lead, &gated).await;
        let (_, a) = env
            .svc
            .propose_plan(&lead, gated.id, "plan A".into())
            .await
            .unwrap();
        let (_, b) = env
            .svc
            .propose_plan(&lead, gated.id, "plan B".into())
            .await
            .unwrap();

        // Deciding on a plan that is no longer the latest is refused and changes nothing.
        assert!(matches!(
            env.svc.approve_plan(gated.id, a.id).await,
            Err(TaskError::PlanChanged)
        ));
        assert!(matches!(
            env.svc.request_plan_changes(gated.id, a.id, "nope").await,
            Err(TaskError::PlanChanged)
        ));
        let t = env.task(&gated).await;
        assert_eq!(
            (t.status, t.plan_state),
            (TaskStatus::Planning, PlanState::Submitted)
        );

        let t = env.svc.approve_plan(gated.id, b.id).await.unwrap();
        assert_eq!(t.status, TaskStatus::Claimed);
    }
);

store_matrix!(
    superseded_and_decided_plans_leave_the_human_inbox,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let lead = env.session("mac").await;
        let gated = env.gated(project_of(&lead)).await;
        env.claimed(&lead, &gated).await;
        let human = |env: &Env| {
            let store = env.store.clone();
            let now = env.clock.now();
            async move {
                store
                    .inbox(Recipient::Human, &InboxFilter::default(), now)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|i| i.message.id)
                    .collect::<Vec<_>>()
            }
        };
        let (_, a) = env
            .svc
            .propose_plan(&lead, gated.id, "plan A".into())
            .await
            .unwrap();
        assert_eq!(human(&env).await, vec![a.id]);
        let (_, b) = env
            .svc
            .propose_plan(&lead, gated.id, "plan B".into())
            .await
            .unwrap();
        assert_eq!(
            human(&env).await,
            vec![b.id],
            "the superseded plan is archived"
        );
        env.svc
            .request_plan_changes(gated.id, b.id, "smaller")
            .await
            .unwrap();
        assert!(human(&env).await.is_empty(), "the decided plan is archived");
        let (_, c) = env
            .svc
            .propose_plan(&lead, gated.id, "plan C".into())
            .await
            .unwrap();
        env.svc.approve_plan(gated.id, c.id).await.unwrap();
        assert!(human(&env).await.is_empty());
    }
);

store_matrix!(human_attention_is_a_read_only_peek, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let lead = env.session("mac").await;
    let gated = env.gated(project_of(&lead)).await;
    env.claimed(&lead, &gated).await;
    let (_, plan) = env
        .svc
        .propose_plan(&lead, gated.id, "plan".into())
        .await
        .unwrap();
    let messages =
        axle::service::messages::MessageService::new(env.store.clone(), env.clock.clone());
    let send = |to: Address, importance: Importance, body: &'static str| {
        let messages = &messages;
        let from = Actor::Agent(lead.agent.id, lead.session.id);
        async move {
            messages
                .send(
                    &from,
                    axle::service::messages::NewMessage {
                        to,
                        subject: None,
                        body: body.into(),
                        importance,
                        reply_to: None,
                    },
                )
                .await
                .unwrap()
        }
    };
    let urgent = send(Address::Human, Importance::Urgent, "urgent").await;
    send(Address::Human, Importance::Normal, "normal").await;
    // A high post in the thread of a task the human created is due to the human too.
    let thread = send(Address::Task(gated.id), Importance::High, "thread").await;

    let peek = env.store.human_attention(100).await.unwrap();
    let ids: Vec<_> = peek.iter().map(|m| m.id).collect();
    assert_eq!(
        ids,
        vec![urgent.id, thread.id],
        "plans and normal mail are left out"
    );
    assert!(!ids.contains(&plan.id));
    let receipts = env.store.message_receipts(&[urgent.id]).await.unwrap();
    assert_eq!(receipts[0].delivered, 0, "peeking delivers nothing");
});

store_matrix!(a_human_reply_archives_the_original, |store: Arc<
    dyn Store,
>| async move {
    use axle::service::messages::{MessageError, MessageService, NewMessage};
    let env = Env::new(store);
    let lead = env.session("mac").await;
    let other = env.session("linux").await;
    let messages = MessageService::new(env.store.clone(), env.clock.clone());
    let original = messages
        .send(
            &Actor::Agent(lead.agent.id, lead.session.id),
            NewMessage {
                to: Address::Human,
                subject: None,
                body: "help".into(),
                importance: Importance::Urgent,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    let reply = |to: Address| NewMessage {
        to,
        subject: None,
        body: "on it".into(),
        importance: Importance::Normal,
        reply_to: Some(original.id),
    };
    assert!(matches!(
        messages
            .send_as_human(reply(Address::Agent(other.agent.id)))
            .await,
        Err(MessageError::MismatchedReply)
    ));
    assert_eq!(env.store.human_attention(10).await.unwrap().len(), 1);
    messages
        .send_as_human(reply(Address::Agent(lead.agent.id)))
        .await
        .unwrap();
    assert!(env.store.human_attention(10).await.unwrap().is_empty());
    assert!(env.inbox_of(&lead).await.iter().any(|m| m.body == "on it"));
});

/// PostgreSQL, READ COMMITTED: an approval that queues behind a resubmission's row lock must
/// see the resubmitted plan once the lock is released, not the plan its snapshot started with.
#[cfg(feature = "pg-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_queued_behind_a_resubmission_is_refused_on_postgres() {
    let (store, schema) = common::pg_store().await;
    let raw = schema.pool().await;
    let outcome = common::catch_body(async move {
        let env = Env::new(store);
        let lead = env.session("mac").await;
        let gated = env.gated(project_of(&lead)).await;
        env.claimed(&lead, &gated).await;
        let (_, a) = env
            .svc
            .propose_plan(&lead, gated.id, "plan A".into())
            .await
            .unwrap();

        // The resubmission, held open: it has locked the task row and written plan B.
        let mut resubmit = raw.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *resubmit)
            .await
            .unwrap();
        sqlx::query("UPDATE tasks SET version = version + 1 WHERE id = $1")
            .bind(gated.id.as_uuid())
            .execute(&mut *resubmit)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, project_id, to_kind, task_id, body, created_at, kind)
             VALUES ($1, $2, 'human', $3, 'plan B', now(), 'plan')",
        )
        .bind(MessageId::new().as_uuid())
        .bind(gated.project_id.as_uuid())
        .bind(gated.id.as_uuid())
        .execute(&mut *resubmit)
        .await
        .unwrap();

        // The human approves plan A, the one they saw; it queues on the row lock.
        let svc = env.svc.clone();
        let id = gated.id;
        let approval = tokio::spawn(async move { svc.approve_plan(id, a.id).await });
        let mut waited = 0;
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
            )
            .bind(blocker)
            .fetch_one(&raw)
            .await
            .unwrap();
            if blocked > 0 {
                break;
            }
            waited += 1;
            assert!(
                waited < 500,
                "the approval never queued behind the resubmission"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        resubmit.commit().await.unwrap();

        assert!(matches!(
            approval.await.unwrap(),
            Err(TaskError::PlanChanged)
        ));
        let t = env.task(&gated).await;
        assert_eq!(
            (t.status, t.plan_state),
            (TaskStatus::Planning, PlanState::Submitted)
        );
    })
    .await;
    schema.drop().await;
    common::resume(outcome);
}
