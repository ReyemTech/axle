//! Related links between tasks (docs/superpowers/specs/2026-11-28-related-links-design.md),
//! on both backends.

#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::TestClock;
use axle::domain::{LinkKind, MessageKind, SessionId, Task, TaskEventKind, TaskId, TaskStatus};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{
    Caller, ClaimOutcome, Completion, MAX_LINKS, NewTask, TaskError, TaskService,
};
use axle::store::Store;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-28 9:00 UTC);
const PROJECT: &str = "github.com/acme/platform-infra";

struct Env {
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    svc: TaskService,
}

impl Env {
    fn new(store: Arc<dyn Store>) -> Self {
        let clock = Arc::new(TestClock::new(T0));
        let svc = TaskService::new(
            store.clone(),
            clock.clone(),
            Duration::minutes(15),
            Duration::hours(2),
            Duration::seconds(90),
        );
        Self { store, clock, svc }
    }

    async fn session(&self, host: &str) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(PROJECT.to_string()),
                host: host.into(),
                harness: "claude-code".into(),
                session: SessionId::new(),
                token: common::token(&*self.store).await,
            },
        )
        .await
        .unwrap()
    }

    async fn task(&self, by: Caller<'_>, title: &str) -> Task {
        let project = match by {
            Caller::Agent(r) => r.project.as_ref().unwrap().id,
            Caller::Human => self.project().await,
        };
        self.clock.advance(Duration::seconds(1));
        self.svc
            .create(
                by,
                NewTask {
                    project_id: project,
                    title: title.into(),
                    body: "details".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    async fn project(&self) -> axle::domain::ProjectId {
        self.store
            .get_project_by_key(PROJECT)
            .await
            .unwrap()
            .expect("a session created it")
            .id
    }

    async fn relate(
        &self,
        by: Caller<'_>,
        id: TaskId,
        add: &[(TaskId, LinkKind)],
        remove: &[TaskId],
    ) -> Result<Task, TaskError> {
        self.clock.advance(Duration::seconds(1));
        self.svc.relate(by, id, add, remove).await
    }

    async fn events(&self, t: &Task, kind: TaskEventKind) -> usize {
        self.store
            .task_events(t.id)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == kind)
            .count()
    }

    /// `(other task, label)` for each link of `t`.
    async fn links(&self, t: &Task) -> Vec<(TaskId, &'static str)> {
        self.svc
            .related(t.id)
            .await
            .unwrap()
            .iter()
            .map(|r| (r.other.id, r.label()))
            .collect()
    }
}

store_matrix!(
    links_read_from_both_sides,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let creator = env.session("mac").await;
        let a = env.task(Caller::Agent(&creator), "a").await;
        let b = env.task(Caller::Human, "b").await;
        let c = env.task(Caller::Human, "c").await;

        let t = env
            .relate(
                Caller::Agent(&creator),
                a.id,
                &[(b.id, LinkKind::Relates), (c.id, LinkKind::FollowUp)],
                &[],
            )
            .await
            .unwrap();
        assert!(t.version > a.version);
        assert_eq!(
            env.links(&a).await,
            vec![(b.id, "related to"), (c.id, "follow-up of")]
        );
        assert_eq!(env.links(&b).await, vec![(a.id, "related to")]);
        assert_eq!(env.links(&c).await, vec![(a.id, "follow-up")]);
        let related = env
            .store
            .task_events(a.id)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == TaskEventKind::Related)
            .unwrap();
        let payload = related.payload.unwrap();
        assert_eq!(payload["added"][1]["kind"], "follow-up");
        assert_eq!(payload["added"][1]["id"], c.id.to_string());

        // The same link again changes nothing; a new kind replaces the pair's link.
        env.relate(
            Caller::Agent(&creator),
            a.id,
            &[(b.id, LinkKind::Relates)],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(env.events(&a, TaskEventKind::Related).await, 1);
        env.relate(Caller::Human, b.id, &[(a.id, LinkKind::FollowUp)], &[])
            .await
            .unwrap();
        assert_eq!(env.links(&b).await, vec![(a.id, "follow-up of")]);
        assert_eq!(
            env.links(&a).await,
            vec![(c.id, "follow-up of"), (b.id, "follow-up")]
        );

        // Removal works from either side, whichever way the link points.
        env.relate(Caller::Agent(&creator), a.id, &[], &[b.id, c.id])
            .await
            .unwrap();
        assert!(env.links(&a).await.is_empty());
        assert!(env.links(&b).await.is_empty());
        assert_eq!(env.events(&a, TaskEventKind::Related).await, 2);
        // Removing a link that is not there is a no-op.
        env.relate(Caller::Human, a.id, &[], &[b.id]).await.unwrap();
        assert_eq!(env.events(&a, TaskEventKind::Related).await, 2);
    }
);

store_matrix!(bad_links_are_refused, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    env.session("mac").await;
    let a = env.task(Caller::Human, "a").await;
    assert!(matches!(
        env.relate(Caller::Human, a.id, &[(a.id, LinkKind::Relates)], &[])
            .await,
        Err(TaskError::SelfLink)
    ));
    let ghost = TaskId::new();
    assert!(matches!(
        env.relate(Caller::Human, a.id, &[(ghost, LinkKind::Relates)], &[])
            .await,
        Err(TaskError::UnknownLinkTarget(t)) if t == ghost
    ));
    assert!(matches!(
        env.relate(Caller::Human, ghost, &[(a.id, LinkKind::Relates)], &[])
            .await,
        Err(TaskError::NotFound)
    ));

    // At most MAX_LINKS per task, counting links that point at it.
    let mut others = Vec::new();
    for i in 0..MAX_LINKS {
        others.push(env.task(Caller::Human, &format!("other {i}")).await);
    }
    let add: Vec<_> = others.iter().map(|t| (t.id, LinkKind::Relates)).collect();
    env.relate(Caller::Human, a.id, &add, &[]).await.unwrap();
    let extra = env.task(Caller::Human, "one too many").await;
    assert!(matches!(
        env.relate(Caller::Human, extra.id, &[(a.id, LinkKind::Relates)], &[])
            .await,
        Err(TaskError::TooManyLinks(t)) if t == a.id
    ));
    assert!(matches!(
        env.relate(Caller::Human, a.id, &[(extra.id, LinkKind::Relates)], &[])
            .await,
        Err(TaskError::TooManyLinks(t)) if t == a.id
    ));
    assert!(env.links(&extra).await.is_empty(), "nothing was written");
    assert_eq!(env.links(&a).await.len(), MAX_LINKS as usize);
    // Swapping one out in the same call fits.
    env.relate(
        Caller::Human,
        a.id,
        &[(extra.id, LinkKind::Relates)],
        &[others[0].id],
    )
    .await
    .unwrap();
    assert_eq!(env.links(&a).await.len(), MAX_LINKS as usize);
});

store_matrix!(only_the_creator_owner_or_human_links, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let creator = env.session("mac").await;
    let owner = env.session("linux").await;
    let stranger = env.session("pi").await;
    let a = env.task(Caller::Agent(&creator), "a").await;
    let b = env.task(Caller::Human, "b").await;
    let c = env.task(Caller::Human, "c").await;
    assert!(matches!(
        env.relate(
            Caller::Agent(&owner),
            a.id,
            &[(b.id, LinkKind::Relates)],
            &[]
        )
        .await,
        Err(TaskError::NotOwner)
    ));
    match env.svc.claim(&owner, a.id).await.unwrap() {
        ClaimOutcome::Claimed(_) => {}
        other => panic!("expected Claimed, got {other:?}"),
    }
    env.relate(
        Caller::Agent(&owner),
        a.id,
        &[(b.id, LinkKind::Relates)],
        &[],
    )
    .await
    .unwrap();
    env.relate(
        Caller::Agent(&creator),
        a.id,
        &[(c.id, LinkKind::Relates)],
        &[],
    )
    .await
    .unwrap();
    assert!(matches!(
        env.relate(Caller::Agent(&stranger), a.id, &[], &[b.id])
            .await,
        Err(TaskError::NotOwner)
    ));
    // Linking *to* a task needs no rights over it.
    env.relate(Caller::Human, b.id, &[(c.id, LinkKind::Relates)], &[])
        .await
        .unwrap();
    assert_eq!(env.links(&a).await.len(), 2);
});

store_matrix!(a_duplicate_is_cancelled_with_a_note, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let creator = env.session("mac").await;
    let original = env.task(Caller::Human, "the original").await;
    let dup = env.task(Caller::Agent(&creator), "the same again").await;

    let t = env
        .relate(
            Caller::Agent(&creator),
            dup.id,
            &[(original.id, LinkKind::Duplicate)],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(t.status, TaskStatus::Cancelled);
    assert_eq!(env.links(&dup).await, vec![(original.id, "duplicate of")]);
    assert_eq!(env.links(&original).await, vec![(dup.id, "duplicated by")]);
    let cancelled = env
        .store
        .task_events(dup.id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == TaskEventKind::Cancelled)
        .unwrap();
    assert!(cancelled.note.unwrap().contains(&original.id.to_string()));
    let thread = env.store.task_thread(dup.id).await.unwrap();
    let note = thread.last().unwrap();
    assert_eq!(note.kind, MessageKind::System);
    assert!(
        note.body.contains("Duplicate of \"the original\""),
        "{}",
        note.body
    );
    assert_eq!(
        env.store
            .get_task(original.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Open,
        "the target is untouched"
    );

    // Two duplicate targets at once are refused.
    let other = env.task(Caller::Human, "other").await;
    let x = env.task(Caller::Human, "x").await;
    assert!(matches!(
        env.relate(
            Caller::Human,
            x.id,
            &[
                (original.id, LinkKind::Duplicate),
                (other.id, LinkKind::Duplicate)
            ],
            &[],
        )
        .await,
        Err(TaskError::DuplicateTwice)
    ));

    // A done task cannot be marked a duplicate (other links are fine).
    let worker = env.session("linux").await;
    let done = env.task(Caller::Agent(&creator), "finished").await;
    env.svc.claim(&worker, done.id).await.unwrap();
    env.svc
        .complete(
            &worker,
            done.id,
            Completion {
                summary: "done".into(),
                ..Completion::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        env.relate(
            Caller::Agent(&creator),
            done.id,
            &[(original.id, LinkKind::Duplicate)],
            &[],
        )
        .await,
        Err(TaskError::DuplicateOfDone)
    ));
    assert!(env.links(&done).await.is_empty());
    let t = env
        .relate(
            Caller::Agent(&creator),
            done.id,
            &[(original.id, LinkKind::Relates)],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(t.status, TaskStatus::Done);
});

store_matrix!(a_top_level_task_moves_under_a_parent, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let creator = env.session("mac").await;
    let lead = env.session("linux").await;
    let parent = env.task(Caller::Human, "parent").await;
    let loose = env.task(Caller::Agent(&creator), "loose").await;
    env.svc.claim(&lead, parent.id).await.unwrap();

    let moved = env
        .svc
        .move_under(Caller::Agent(&creator), loose.id, parent.id)
        .await
        .unwrap();
    assert_eq!(moved.parent_id, Some(parent.id));
    assert!(moved.version > loose.version);
    let event = env
        .store
        .task_events(loose.id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == TaskEventKind::MovedUnder)
        .unwrap();
    assert_eq!(event.note.as_deref(), Some("moved under \"parent\""));
    assert_eq!(event.payload.unwrap()["parent"], parent.id.to_string());
    // The parent counts it at once, and consolidates when it closes.
    let detail = env.svc.get(parent.id).await.unwrap();
    assert_eq!((detail.progress.done, detail.progress.total), (0, 1));
    env.svc.claim(&creator, loose.id).await.unwrap();
    env.svc
        .complete(
            &creator,
            loose.id,
            Completion {
                summary: "done".into(),
                ..Completion::default()
            },
        )
        .await
        .unwrap();
    let p = env.store.get_task(parent.id).await.unwrap().unwrap();
    assert_eq!(
        p.status,
        TaskStatus::Review,
        "last child closed: parent to review"
    );

    // Refusals.
    let other = env.task(Caller::Human, "other").await;
    assert!(matches!(
        env.svc.move_under(Caller::Human, other.id, other.id).await,
        Err(TaskError::OwnParent)
    ));
    assert!(matches!(
        env.svc
            .move_under(Caller::Human, other.id, TaskId::new())
            .await,
        Err(TaskError::UnknownParent)
    ));
    assert!(matches!(
        env.svc.move_under(Caller::Human, loose.id, other.id).await,
        Err(TaskError::AlreadyAChild)
    ));
    // `parent` is now an ancestor of `loose`: moving it under `loose` would be a cycle
    // (refused for a done parent first, so use an open grandchild).
    let grandparent = env.task(Caller::Human, "top").await;
    let child = env
        .svc
        .create(
            Caller::Human,
            NewTask {
                project_id: grandparent.project_id,
                parent_id: Some(grandparent.id),
                title: "child".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        env.svc
            .move_under(Caller::Human, grandparent.id, child.id)
            .await,
        Err(TaskError::ParentIsDescendant)
    ));
    let cancelled = env.task(Caller::Human, "gone").await;
    env.svc.cancel(Caller::Human, cancelled.id).await.unwrap();
    assert!(matches!(
        env.svc
            .move_under(Caller::Human, other.id, cancelled.id)
            .await,
        Err(TaskError::ParentClosed(TaskStatus::Cancelled))
    ));
    assert!(matches!(
        env.svc.move_under(Caller::Human, other.id, loose.id).await,
        Err(TaskError::ParentClosed(TaskStatus::Done))
    ));
    // Only its creator, its owner or the human may move it.
    let mine = env.task(Caller::Agent(&creator), "mine").await;
    assert!(matches!(
        env.svc
            .move_under(Caller::Agent(&lead), mine.id, other.id)
            .await,
        Err(TaskError::NotOwner)
    ));
    env.svc.claim(&lead, mine.id).await.unwrap();
    env.svc
        .move_under(Caller::Agent(&lead), mine.id, other.id)
        .await
        .unwrap();
    // A task in another project, unless the parent spans it.
    let elsewhere = IdentityService::resolve(
        &*env.store,
        &*env.clock,
        IdentityInput {
            project_key: Some("github.com/acme/web-app".into()),
            host: "mac".into(),
            harness: "claude-code".into(),
            session: SessionId::new(),
            token: common::token(&*env.store).await,
        },
    )
    .await
    .unwrap();
    let foreign = env.task(Caller::Agent(&elsewhere), "web part").await;
    assert!(matches!(
        env.svc
            .move_under(Caller::Human, foreign.id, other.id)
            .await,
        Err(TaskError::ChildProjectMismatch)
    ));
    env.svc
        .add_projects(Caller::Human, other.id, &[foreign.project_id])
        .await
        .unwrap();
    env.svc
        .move_under(Caller::Human, foreign.id, other.id)
        .await
        .unwrap();
    // A task spanning projects cannot become a child.
    let wide = env.task(Caller::Human, "wide").await;
    env.svc
        .add_projects(Caller::Human, wide.id, &[foreign.project_id])
        .await
        .unwrap();
    assert!(matches!(
        env.svc.move_under(Caller::Human, wide.id, other.id).await,
        Err(TaskError::ChildExtraProjects)
    ));
});
