//! Local projects for folders without a git remote, and adopting the remote later
//! (docs/superpowers/specs/2026-11-28-local-projects-design.md).

#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{Actor, Address, Adoption, Importance, SessionId};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::messages::{InboxFilter, MessageService, NewMessage};
use axle::service::tasks::{Caller, NewTask, TaskService};
use axle::store::{Store, StoreError, TaskFilter};
use time::Duration;
use time::macros::datetime;

const FOLDER: &str = "0123456789abcdef";
const OTHER_FOLDER: &str = "fedcba9876543210";
const LOCAL: &str = "local/mac/axl-saas";
const GIT: &str = "github.com/acme/axle-saas";

struct Env {
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    msgs: MessageService,
    tasks: TaskService,
    token: axle::domain::TokenId,
}

impl Env {
    async fn new(store: Arc<dyn Store>) -> Self {
        let token = common::token(&*store).await;
        let clock = Arc::new(TestClock::new(datetime!(2026-11-28 9:00 UTC)));
        Self {
            msgs: MessageService::new(store.clone(), clock.clone()),
            tasks: TaskService::new(
                store.clone(),
                clock.clone(),
                Duration::minutes(15),
                Duration::hours(2),
                Duration::seconds(90),
            ),
            store,
            clock,
            token,
        }
    }

    async fn session(&self, key: &str, folder: Option<&str>, host: &str) -> Resolved {
        self.session_as(key, folder, host, SessionId::new()).await
    }

    async fn session_as(
        &self,
        key: &str,
        folder: Option<&str>,
        host: &str,
        session: SessionId,
    ) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(key.into()),
                folder: folder.map(str::to_string),
                host: host.into(),
                harness: "claude-code".into(),
                session,
                token: self.token,
            },
        )
        .await
        .unwrap()
    }

    async fn task(&self, by: &Resolved, title: &str) -> axle::domain::Task {
        self.tasks
            .create(
                Caller::Agent(by),
                NewTask {
                    project_id: by.agent.project_id.unwrap(),
                    title: title.into(),
                    body: "body".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    async fn broadcast(&self, from: &Resolved, body: &str) {
        self.clock.advance(Duration::seconds(1));
        self.msgs
            .send(
                &Actor::Agent(from.agent.id, from.session.id),
                NewMessage {
                    to: Address::Project(from.agent.project_id.unwrap()),
                    subject: Some("note".into()),
                    body: body.into(),
                    importance: Importance::Normal,
                    reply_to: None,
                },
            )
            .await
            .unwrap();
    }

    async fn task_titles(&self, project: axle::domain::ProjectId) -> Vec<String> {
        let mut t: Vec<String> = self
            .store
            .list_tasks(&TaskFilter {
                project: Some(project),
                ..TaskFilter::default()
            })
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        t.sort();
        t
    }
}

store_matrix!(
    local_project_is_found_by_folder,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let a = env.session(LOCAL, Some(FOLDER), "mac").await;
        let p = a.project.clone().unwrap();
        assert_eq!(p.key, LOCAL);
        assert_eq!(p.display_name, "axl-saas");
        assert_eq!(p.local_host.as_deref(), Some("mac"));
        assert_eq!(p.adopt_key, None);

        // Same folder again: same project and agent.
        let again = env.session(LOCAL, Some(FOLDER), "mac").await;
        assert_eq!(again.agent.id, a.agent.id);

        // A same-named folder at another path: a distinct project with a suffixed key.
        let other = env.session(LOCAL, Some(OTHER_FOLDER), "mac").await;
        let o = other.project.unwrap();
        assert_ne!(o.id, p.id);
        assert_eq!(o.key, "local/mac/axl-saas-fedcba");

        // The same folder hash on another host is another folder.
        let elsewhere = env
            .session("local/linux/axl-saas", Some(FOLDER), "linux")
            .await;
        assert_ne!(elsewhere.project.unwrap().id, p.id);

        // Without a folder hash (an old bridge or a pin) a local key is a plain key.
        let plain = env.session(LOCAL, None, "mac").await;
        assert_eq!(plain.project.unwrap().id, p.id);
        // A malformed folder value is ignored.
        let junk = env.session(LOCAL, Some("../../etc"), "mac").await;
        assert_eq!(junk.project.unwrap().id, p.id);
    }
);

store_matrix!(
    remote_is_noted_not_switched,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let before = env.session(LOCAL, Some(FOLDER), "mac").await;
        let local = before.project.clone().unwrap();

        // The folder gains a remote: the session stays on the local project, which records the key.
        let after = env.session(GIT, Some(FOLDER), "mac").await;
        let p = after.project.clone().unwrap();
        assert_eq!(p.id, local.id);
        assert_eq!(p.key, LOCAL);
        assert_eq!(p.adopt_key.as_deref(), Some(GIT));
        assert_eq!(after.agent.id, before.agent.id);
        assert!(env.store.get_project_by_key(GIT).await.unwrap().is_none());

        // Another folder with the same remote is not affected and joins the git project.
        let other = env.session(GIT, Some(OTHER_FOLDER), "mac").await;
        assert_eq!(other.project.unwrap().key, GIT);
        let still = env.store.get_project(local.id).await.unwrap().unwrap();
        assert_eq!(still.adopt_key.as_deref(), Some(GIT));

        // Nothing to adopt for a project without a recorded remote.
        let fresh = env
            .session("local/mac/x", Some(OTHER_FOLDER), "laptop")
            .await;
        assert!(matches!(
            IdentityService::adopt(&*env.store, &*env.clock, fresh.project.unwrap().id).await,
            Err(StoreError::NotFound(_))
        ));
    }
);

store_matrix!(adopt_renames_in_place, |store: Arc<dyn Store>| async move {
    let env = Env::new(store).await;
    let session = SessionId::new();
    let a = env.session_as(LOCAL, Some(FOLDER), "mac", session).await;
    let local = a.project.clone().unwrap();
    env.task(&a, "brainstorm").await;
    env.broadcast(&a, "idea").await;
    env.session(GIT, Some(FOLDER), "mac").await;

    let out = IdentityService::adopt(&*env.store, &*env.clock, local.id)
        .await
        .unwrap();
    let Adoption::Renamed(p) = out else {
        panic!("expected a rename, got {out:?}")
    };
    assert_eq!(p.id, local.id);
    assert_eq!(p.key, GIT);
    assert_eq!(
        p.display_name, "acme/axle-saas",
        "a default name follows the key"
    );
    assert_eq!(p.local_host, None);
    assert_eq!(p.adopt_key, None);
    assert_eq!(env.task_titles(p.id).await, vec!["brainstorm"]);

    // The bridge's next request uses the git key and lands on the moved project; the
    // running session (still sending the old local key) resolves there too.
    let next = env.session(GIT, Some(FOLDER), "mac").await;
    assert_eq!(next.project.unwrap().id, local.id);
    assert_eq!(next.agent.id, a.agent.id);
    let old = env.session_as(LOCAL, Some(FOLDER), "mac", session).await;
    assert_eq!(old.project.unwrap().id, local.id);
    assert_eq!(old.session.id, session);
    let old_plain = env.session(LOCAL, None, "mac").await;
    assert_eq!(old_plain.project.unwrap().id, local.id);

    // A second adoption has nothing to do.
    assert!(matches!(
        IdentityService::adopt(&*env.store, &*env.clock, local.id).await,
        Err(StoreError::NotFound(_))
    ));
    // The freed local key is given to a different folder of that name.
    let newcomer = env.session(LOCAL, Some(OTHER_FOLDER), "mac").await;
    let n = newcomer.project.unwrap();
    assert_ne!(n.id, local.id);
    assert_eq!(n.key, LOCAL);
});

store_matrix!(adopt_keeps_a_custom_display_name, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store).await;
    let a = env.session(LOCAL, Some(FOLDER), "mac").await;
    let id = a.project.unwrap().id;
    env.store.rename_project(id, "SaaS ideas").await.unwrap();
    env.session(GIT, Some(FOLDER), "mac").await;
    let Adoption::Renamed(p) = IdentityService::adopt(&*env.store, &*env.clock, id)
        .await
        .unwrap()
    else {
        panic!("expected a rename")
    };
    assert_eq!(p.display_name, "SaaS ideas");
});

store_matrix!(adopt_merges_into_an_existing_project, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store).await;
    // The git project already exists: another machine works there.
    let remote = env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
    let target = remote.project.clone().unwrap();
    env.task(&remote, "existing").await;
    // An agent with the same host and harness as the local one is already in the target
    // (a mac session pinned to the git key), so the local agent folds into it.
    let twin = env.session(GIT, None, "mac").await;
    assert_eq!(twin.project.as_ref().unwrap().id, target.id);

    let session = SessionId::new();
    let a = env.session_as(LOCAL, Some(FOLDER), "mac", session).await;
    let local = a.project.clone().unwrap();
    // And a local agent with no twin there moves as it is.
    let codex = IdentityService::resolve(
        &*env.store,
        &*env.clock,
        IdentityInput {
            project_key: Some(LOCAL.into()),
            folder: Some(FOLDER.into()),
            host: "mac".into(),
            harness: "codex".into(),
            session: SessionId::new(),
            token: common::token(&*env.store).await,
        },
    )
    .await
    .unwrap();
    assert_eq!(codex.project.as_ref().unwrap().id, local.id);
    env.task(&a, "local work").await;
    env.broadcast(&a, "local note").await;
    // A direct message to the local agent.
    env.msgs
        .send(
            &Actor::Agent(remote.agent.id, remote.session.id),
            NewMessage {
                to: Address::Agent(a.agent.id),
                subject: Some("hi".into()),
                body: "for the mac".into(),
                importance: Importance::Normal,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    env.session(GIT, Some(FOLDER), "mac").await;

    let out = IdentityService::adopt(&*env.store, &*env.clock, local.id)
        .await
        .unwrap();
    let Adoption::Merged(p) = out else {
        panic!("expected a merge, got {out:?}")
    };
    assert_eq!(p.id, target.id);
    assert_eq!(
        env.task_titles(target.id).await,
        vec!["existing", "local work"]
    );

    // Old ids and keys resolve to the target; the local project is gone from listings.
    assert_eq!(
        env.store.get_project(local.id).await.unwrap().unwrap().id,
        target.id
    );
    let keys: Vec<String> = env
        .store
        .list_projects()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.key)
        .collect();
    assert_eq!(keys, vec![GIT.to_string()]);
    let old = env.session_as(LOCAL, Some(FOLDER), "mac", session).await;
    assert_eq!(old.project.unwrap().id, target.id);
    assert_eq!(
        old.agent.id, twin.agent.id,
        "the local agent folded into its twin"
    );
    assert_eq!(old.session.id, session);

    // The moved agent kept its id and now lives in the target.
    let moved = env.store.get_agent(codex.agent.id).await.unwrap().unwrap();
    assert_eq!(moved.project_id, Some(target.id));

    // The twin now has the local agent's direct message.
    let inbox = env.msgs.inbox(&twin, InboxFilter::default()).await.unwrap();
    assert!(
        inbox.iter().any(|i| i.message.body == "for the mac"),
        "{inbox:?}"
    );
    let left = env
        .store
        .list_tasks(&TaskFilter {
            project: Some(local.id),
            ..TaskFilter::default()
        })
        .await
        .unwrap();
    assert!(left.is_empty(), "no task is left on the local project");
    // The project-wide note moved with the project.
    let notes = env
        .store
        .list_messages(Some(target.id), None, 50)
        .await
        .unwrap();
    assert!(notes.iter().any(|m| m.body == "local note"), "{notes:?}");
    assert!(
        env.store
            .list_messages(Some(local.id), None, 50)
            .await
            .unwrap()
            .is_empty()
    );

    // New sessions in the folder use the git key and land on the target.
    let next = env.session(GIT, Some(FOLDER), "mac").await;
    assert_eq!(next.project.unwrap().id, target.id);
    assert_eq!(next.agent.id, twin.agent.id);
});

impl Env {
    /// A session of the agent `(key, mac, harness)` from `folder`.
    async fn harness(&self, key: &str, folder: Option<&str>, harness: &str) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(key.into()),
                folder: folder.map(str::to_string),
                host: "mac".into(),
                harness: harness.into(),
                session: SessionId::new(),
                token: self.token,
            },
        )
        .await
        .unwrap()
    }

    async fn inbox_bodies(&self, r: &Resolved) -> Vec<String> {
        self.msgs
            .inbox(r, InboxFilter::default())
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.message.body)
            .collect()
    }

    async fn adopt_merged(&self, local: axle::domain::ProjectId) {
        let out = IdentityService::adopt(&*self.store, &*self.clock, local)
            .await
            .unwrap();
        assert!(matches!(out, Adoption::Merged(_)), "{out:?}");
    }

    async fn send_to(&self, from: &Resolved, to: Address, body: &str) -> axle::domain::Message {
        self.clock.advance(Duration::seconds(1));
        self.msgs
            .send(
                &Actor::Agent(from.agent.id, from.session.id),
                NewMessage {
                    to,
                    subject: None,
                    body: body.into(),
                    importance: Importance::Normal,
                    reply_to: None,
                },
            )
            .await
            .unwrap()
    }
}

store_matrix!(
    merge_keeps_unread_broadcasts_of_folded_agents,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        // Local project: a claude-code agent that has not read its project mail yet, and a
        // codex agent that broadcasts to it.
        let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
        let local = a.project.clone().unwrap().id;
        let b = env.harness(LOCAL, Some(FOLDER), "codex").await;
        env.broadcast(&b, "unread local note").await;

        // The git project already exists with its own history, and a mac/claude-code twin
        // created after the local note (so its cursor starts past it).
        let remote = env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
        env.broadcast(&remote, "old target note").await;
        let twin = env.harness(GIT, None, "claude-code").await;
        env.harness(GIT, Some(FOLDER), "claude-code").await;
        env.adopt_merged(local).await;

        // The folded agent's unread broadcast is still unread, now for the twin.
        let twin_mail = env.inbox_bodies(&twin).await;
        assert!(
            twin_mail.contains(&"unread local note".to_string()),
            "{twin_mail:?}"
        );
        // An agent that moves as it is does not inherit the target's old broadcasts.
        let b_mail = env.inbox_bodies(&b).await;
        assert!(
            !b_mail.contains(&"old target note".to_string()),
            "{b_mail:?}"
        );
    }
);

store_matrix!(folded_agents_redirect_to_the_survivor, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store).await;
    let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
    let local = a.project.clone().unwrap().id;
    let remote = env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
    let target = remote.project.clone().unwrap().id;
    let twin = env.harness(GIT, None, "claude-code").await;
    env.harness(GIT, Some(FOLDER), "claude-code").await;
    env.adopt_merged(local).await;

    // The old agent id resolves to the surviving agent and is no longer listed.
    let got = env.store.get_agent(a.agent.id).await.unwrap().unwrap();
    assert_eq!(got.id, twin.agent.id);
    let listed: Vec<_> = env
        .store
        .list_agents(100)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert!(!listed.contains(&a.agent.id), "{listed:?}");
    assert!(listed.contains(&twin.agent.id));
    let sessions = env.store.list_sessions(None).await.unwrap();
    assert!(sessions.iter().all(|(ag, _)| ag.id != a.agent.id));

    // Messages to the old agent id or the old project id reach the survivors.
    let sent = env
        .send_to(&remote, Address::Agent(a.agent.id), "to the old id")
        .await;
    assert_eq!(sent.to, Address::Agent(twin.agent.id));
    let sent = env
        .send_to(&remote, Address::Project(local), "to the old project")
        .await;
    assert_eq!(sent.to, Address::Project(target));
    let mail = env.inbox_bodies(&twin).await;
    assert!(mail.contains(&"to the old id".to_string()), "{mail:?}");
    assert!(mail.contains(&"to the old project".to_string()), "{mail:?}");

    // The old local key still names the project, so labels that use it resolve.
    let by_key = env.store.get_project_by_key(LOCAL).await.unwrap().unwrap();
    assert_eq!(by_key.id, target);

    // A task created against the old project id lands on the target.
    let task = env
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: local,
                title: "late".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(task.project_id, target);
});

store_matrix!(
    merge_keeps_a_receipt_pending_for_either_agent,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
        let local = a.project.clone().unwrap().id;
        let remote = env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
        let target = remote.project.clone().unwrap().id;
        let twin = env.harness(GIT, None, "claude-code").await;
        env.harness(GIT, Some(FOLDER), "claude-code").await;

        // A task in the target, created by the local agent and claimed by its twin: a post in
        // its thread is due to both.
        let task = env
            .tasks
            .create(
                Caller::Agent(&a),
                NewTask {
                    project_id: target,
                    title: "shared".into(),
                    body: "body".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();
        let claimed = env.tasks.claim(&twin, task.id).await.unwrap();
        assert!(
            matches!(claimed, axle::service::tasks::ClaimOutcome::Claimed(_)),
            "{claimed:?}"
        );
        let post = env
            .send_to(&remote, Address::Task(task.id), "thread post")
            .await;
        // The twin reads and acks it; the local agent has not.
        assert!(
            env.inbox_bodies(&twin)
                .await
                .contains(&"thread post".into())
        );
        env.msgs
            .ack(Caller::Agent(&twin), &[post.id])
            .await
            .unwrap();
        assert!(
            !env.inbox_bodies(&twin)
                .await
                .contains(&"thread post".into())
        );
        assert!(env.inbox_bodies(&a).await.contains(&"thread post".into()));

        env.adopt_merged(local).await;
        // Unread wins: the surviving agent still has it pending.
        let mail = env.inbox_bodies(&twin).await;
        assert!(mail.contains(&"thread post".to_string()), "{mail:?}");
    }
);

store_matrix!(
    merge_keeps_unread_mail_between_the_folded_pair,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
        let local = a.project.clone().unwrap().id;
        env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
        let twin = env.harness(GIT, None, "claude-code").await;
        env.harness(GIT, Some(FOLDER), "claude-code").await;

        // Unread direct mail both ways between the two agents that are about to fold.
        env.send_to(&a, Address::Agent(twin.agent.id), "local to twin")
            .await;
        env.send_to(&twin, Address::Agent(a.agent.id), "twin to local")
            .await;
        assert!(
            env.inbox_bodies(&twin)
                .await
                .contains(&"local to twin".into())
        );
        assert!(env.inbox_bodies(&a).await.contains(&"twin to local".into()));

        env.adopt_merged(local).await;
        // Both are still unread for the surviving agent, and still show their real sender.
        let items = env.msgs.inbox(&twin, InboxFilter::default()).await.unwrap();
        let bodies: Vec<&str> = items.iter().map(|i| i.message.body.as_str()).collect();
        assert!(bodies.contains(&"local to twin"), "{bodies:?}");
        assert!(bodies.contains(&"twin to local"), "{bodies:?}");
        let from_local = items
            .iter()
            .find(|i| i.message.body == "local to twin")
            .unwrap();
        assert_eq!(
            from_local.message.from,
            Actor::Agent(a.agent.id, a.session.id)
        );

        // Its own new mail is still never delivered back to it.
        let c = env.harness(GIT, None, "codex").await;
        env.send_to(
            &twin,
            Address::Project(c.project.clone().unwrap().id),
            "own note",
        )
        .await;
        let mine = env.inbox_bodies(&twin).await;
        assert!(!mine.contains(&"own note".to_string()), "{mine:?}");
    }
);

store_matrix!(
    merge_leaves_history_and_survivor_broadcasts_alone,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        // The twin exists first, so its cursor predates everything below.
        env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
        let twin = env.harness(GIT, None, "claude-code").await;
        let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
        let local = a.project.clone().unwrap().id;
        let task = env.task(&a, "local history").await;
        env.broadcast(&a, "broadcast by the folded agent").await;
        let before = env.store.task_events(task.id).await.unwrap();
        env.harness(GIT, Some(FOLDER), "claude-code").await;

        env.adopt_merged(local).await;

        // History is append-only: event actors keep the folded agent's id.
        let after = env.store.task_events(task.id).await.unwrap();
        assert_eq!(
            after.iter().map(|e| e.actor).collect::<Vec<_>>(),
            before.iter().map(|e| e.actor).collect::<Vec<_>>()
        );
        assert!(
            after
                .iter()
                .all(|e| matches!(e.actor, Actor::Agent(id, _) if id == a.agent.id))
        );
        // The folded agent's broadcast, never delivered to the survivor, is still due to it.
        let mail = env.inbox_bodies(&twin).await;
        assert!(
            mail.contains(&"broadcast by the folded agent".to_string()),
            "{mail:?}"
        );
        // And it is listed once.
        assert_eq!(
            mail.iter()
                .filter(|b| *b == "broadcast by the folded agent")
                .count(),
            1
        );
    }
);

store_matrix!(
    a_session_created_for_a_folded_agent_lands_on_the_survivor,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
        let local = a.project.clone().unwrap().id;
        env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
        let twin = env.harness(GIT, None, "claude-code").await;
        env.harness(GIT, Some(FOLDER), "claude-code").await;
        env.adopt_merged(local).await;

        // A registration that looked up its agent before the fold and inserts its session
        // after it: the session is stored against the survivor.
        let late = SessionId::new();
        let s = env
            .store
            .create_session(late, a.agent.id, env.token, env.clock.now())
            .await
            .unwrap();
        assert_eq!(s.agent_id, twin.agent.id);
        // And it resolves, now and on every later request, without a mismatch.
        let r = env.session_as(GIT, Some(FOLDER), "mac", late).await;
        assert_eq!(r.session.id, late);
        assert_eq!(r.agent.id, twin.agent.id);
        let loaded = IdentityService::load(&*env.store, late)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.agent.id, twin.agent.id);
    }
);

store_matrix!(
    writes_against_retired_ids_land_on_the_survivors,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        // `a` is resolved before the merge and used after it, as a request in flight would.
        let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
        let local = a.project.clone().unwrap().id;
        let remote = env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
        let target = remote.project.clone().unwrap().id;
        let twin = env.harness(GIT, None, "claude-code").await;
        env.harness(GIT, Some(FOLDER), "claude-code").await;
        env.adopt_merged(local).await;

        // A message stored with the retired project, recipient and sender ids.
        let m = axle::domain::Message {
            id: axle::domain::MessageId::new(),
            project_id: Some(local),
            from: Actor::Agent(a.agent.id, a.session.id),
            to: Address::Project(local),
            subject: None,
            body: "late broadcast".into(),
            importance: Importance::Normal,
            task_id: None,
            reply_to: None,
            created_at: env.clock.now(),
            kind: axle::domain::MessageKind::Message,
            options: Vec::new(),
        };
        env.store.insert_message(&m, None).await.unwrap();
        let stored = env.store.get_message(m.id).await.unwrap().unwrap();
        assert_eq!(stored.project_id, Some(target));
        assert_eq!(stored.to, Address::Project(target));
        assert_eq!(stored.from, Actor::Agent(twin.agent.id, a.session.id));
        let to_agent = axle::domain::Message {
            id: axle::domain::MessageId::new(),
            to: Address::Agent(a.agent.id),
            from: Actor::Agent(remote.agent.id, remote.session.id),
            body: "late direct".into(),
            ..m.clone()
        };
        env.store.insert_message(&to_agent, None).await.unwrap();
        let stored = env.store.get_message(to_agent.id).await.unwrap().unwrap();
        assert_eq!(stored.to, Address::Agent(twin.agent.id));
        let mail = env.inbox_bodies(&twin).await;
        assert!(mail.contains(&"late direct".to_string()), "{mail:?}");

        // A task created and claimed by the stale caller: its creator and owner are the
        // survivor, in the survivor's project.
        let task = env.task(&a, "late task").await;
        assert_eq!(task.project_id, target);
        let stored = env.store.get_task(task.id).await.unwrap().unwrap();
        assert!(matches!(stored.created_by, Actor::Agent(id, _) if id == twin.agent.id));
        let claimed = env.tasks.claim(&a, task.id).await.unwrap();
        assert!(
            matches!(claimed, axle::service::tasks::ClaimOutcome::Claimed(_)),
            "{claimed:?}"
        );
        let stored = env.store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(stored.owner_agent_id, Some(twin.agent.id));
        // The claim's history event is the survivor's too.
        let events = env.store.task_events(task.id).await.unwrap();
        assert!(
            events
                .iter()
                .all(|e| matches!(e.actor, Actor::Agent(id, _) if id == twin.agent.id))
        );
    }
);

store_matrix!(
    a_folded_creators_acked_copy_does_not_hide_the_owners_pending_one,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
        let local = a.project.clone().unwrap().id;
        let remote = env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
        let target = remote.project.clone().unwrap().id;
        let twin = env.harness(GIT, None, "claude-code").await;
        env.harness(GIT, Some(FOLDER), "claude-code").await;

        // A task in the target created by the local agent and held by its twin.
        let task = env
            .tasks
            .create(
                Caller::Agent(&a),
                NewTask {
                    project_id: target,
                    title: "shared".into(),
                    body: "body".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();
        env.tasks.claim(&twin, task.id).await.unwrap();
        let post = env
            .send_to(&remote, Address::Task(task.id), "thread post")
            .await;
        // The creator reads and acks it; the owner has not even fetched its inbox yet.
        assert!(env.inbox_bodies(&a).await.contains(&"thread post".into()));
        env.msgs.ack(Caller::Agent(&a), &[post.id]).await.unwrap();

        env.adopt_merged(local).await;
        let mail = env.inbox_bodies(&twin).await;
        assert!(mail.contains(&"thread post".to_string()), "{mail:?}");
    }
);

store_matrix!(
    a_folded_creators_ack_stays_acked_for_the_survivor,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store).await;
        let a = env.harness(LOCAL, Some(FOLDER), "claude-code").await;
        let local = a.project.clone().unwrap().id;
        let remote = env.session(GIT, Some(OTHER_FOLDER), "laptop").await;
        let target = remote.project.clone().unwrap().id;
        let twin = env.harness(GIT, None, "claude-code").await;
        env.harness(GIT, Some(FOLDER), "claude-code").await;

        // A task created by the local agent that its twin never takes part in.
        let task = env
            .tasks
            .create(
                Caller::Agent(&a),
                NewTask {
                    project_id: target,
                    title: "mine".into(),
                    body: "body".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();
        let post = env
            .send_to(&remote, Address::Task(task.id), "thread post")
            .await;
        assert!(env.inbox_bodies(&a).await.contains(&"thread post".into()));
        env.msgs.ack(Caller::Agent(&a), &[post.id]).await.unwrap();

        env.adopt_merged(local).await;
        // The survivor is now the creator, but the post was already handled: it is neither
        // counted as unread nor delivered again.
        let unread = env
            .store
            .unread_count(
                axle::store::Recipient::Agent {
                    agent: twin.agent.id,
                    session: twin.session.id,
                },
                None,
            )
            .await
            .unwrap();
        let mail = env.inbox_bodies(&twin).await;
        assert!(!mail.contains(&"thread post".to_string()), "{mail:?}");
        assert_eq!(unread, mail.len() as u64, "{mail:?}");
    }
);
