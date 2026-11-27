#[macro_use]
mod common;

use std::sync::Arc;

use coord::clock::{Clock, TestClock};
use coord::domain::{Actor, Address, Importance, MessageId, ProjectId, SessionId};
use coord::service::identity::{IdentityInput, IdentityService, Resolved};
use coord::service::messages::{InboxFilter, MessageError, MessageService, NewMessage};
use coord::service::tasks::{Caller, ClaimOutcome, NewTask, TaskError, TaskService};
use coord::store::Store;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-27 9:00 UTC);
const IAC: &str = "github.com/reyemtech/iac";
const NIMBUS: &str = "github.com/reyemtech/nimbus";

struct Env {
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    msgs: MessageService,
    tasks: TaskService,
}

impl Env {
    fn new(store: Arc<dyn Store>) -> Self {
        let clock = Arc::new(TestClock::new(T0));
        Self {
            msgs: MessageService::new(store.clone(), clock.clone()),
            tasks: TaskService::new(
                store.clone(),
                clock.clone(),
                Duration::minutes(15),
                Duration::hours(2),
            ),
            store,
            clock,
        }
    }

    /// A new session of the agent `(project, host, claude-code)`.
    async fn session(&self, project: &str, host: &str) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(project.into()),
                host: host.into(),
                harness: "claude-code".into(),
                session: SessionId::new(),
                token: common::token(&*self.store).await,
            },
        )
        .await
        .unwrap()
    }

    async fn send(&self, from: &Resolved, to: Address, body: &str) -> MessageId {
        self.clock.advance(Duration::seconds(1));
        self.msgs
            .send(&actor(from), msg(to, body))
            .await
            .unwrap()
            .id
    }

    async fn inbox_ids(&self, r: &Resolved) -> Vec<MessageId> {
        self.msgs
            .inbox(r, InboxFilter::default())
            .await
            .unwrap()
            .iter()
            .map(|i| i.message.id)
            .collect()
    }
}

fn actor(r: &Resolved) -> Actor {
    Actor::Agent(r.agent.id, r.session.id)
}

fn project_of(r: &Resolved) -> ProjectId {
    r.project.as_ref().expect("has a project").id
}

fn msg(to: Address, body: &str) -> NewMessage {
    NewMessage {
        to,
        subject: Some("hello".into()),
        body: body.into(),
        importance: Importance::Normal,
        reply_to: None,
    }
}

store_matrix!(agent_message_acked_once_for_all_sessions, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let sender = env.session(IAC, "mac").await;
    let x1 = env.session(NIMBUS, "mbp").await;
    let x2 = env.session(NIMBUS, "mbp").await;
    assert_eq!(x1.agent.id, x2.agent.id);
    assert_ne!(x1.session.id, x2.session.id);

    let m = env.send(&sender, Address::Agent(x1.agent.id), "ping").await;
    assert_eq!(env.inbox_ids(&x1).await, vec![m]);
    assert_eq!(env.inbox_ids(&x2).await, vec![m]);
    assert_eq!(env.msgs.unread_count(&x2).await.unwrap(), 1);

    assert_eq!(env.msgs.ack(Caller::Agent(&x1), &[m]).await.unwrap(), 1);
    assert!(env.inbox_ids(&x2).await.is_empty());
    assert!(env.inbox_ids(&x1).await.is_empty());
    assert_eq!(env.msgs.unread_count(&x2).await.unwrap(), 0);
    // Acking again is a no-op.
    assert_eq!(env.msgs.ack(Caller::Agent(&x2), &[m]).await.unwrap(), 0);
});

store_matrix!(session_message_only_that_session, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let sender = env.session(IAC, "mac").await;
    let x1 = env.session(NIMBUS, "mbp").await;
    let x2 = env.session(NIMBUS, "mbp").await;

    let m = env
        .send(&sender, Address::Session(x1.session.id), "just you")
        .await;
    assert_eq!(env.inbox_ids(&x1).await, vec![m]);
    assert!(env.inbox_ids(&x2).await.is_empty());
    // Another session of the same agent cannot ack it away.
    assert_eq!(env.msgs.ack(Caller::Agent(&x2), &[m]).await.unwrap(), 0);
    assert_eq!(env.inbox_ids(&x1).await, vec![m]);
    assert_eq!(env.msgs.ack(Caller::Agent(&x1), &[m]).await.unwrap(), 1);
    assert!(env.inbox_ids(&x1).await.is_empty());
});

store_matrix!(
    project_broadcast_cursor,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let sender = env.session(NIMBUS, "mac").await;
        let early = env.session(NIMBUS, "early").await;
        let nimbus = project_of(&sender);

        let m1 = env.send(&sender, Address::Project(nimbus), "first").await;
        env.clock.advance(Duration::seconds(1));
        let late = env.session(NIMBUS, "late").await;
        let m2 = env.send(&sender, Address::Project(nimbus), "second").await;

        assert_eq!(env.inbox_ids(&early).await, vec![m1, m2]);
        assert_eq!(env.inbox_ids(&late).await, vec![m2]);
        // The sender never gets its own broadcast.
        assert!(env.inbox_ids(&sender).await.is_empty());
        assert_eq!(env.msgs.unread_count(&late).await.unwrap(), 1);

        // Acks are per agent and per message: acking m2 leaves m1 unread for `early`.
        env.msgs.ack(Caller::Agent(&early), &[m2]).await.unwrap();
        assert_eq!(env.inbox_ids(&early).await, vec![m1]);
        assert_eq!(env.inbox_ids(&late).await, vec![m2]);
        // ... and the older history stays browsable for the late agent.
        let found = env.msgs.search("first", Some(nimbus), 10).await.unwrap();
        assert_eq!(found.iter().map(|m| m.id).collect::<Vec<_>>(), vec![m1]);
    }
);

store_matrix!(unacked_redelivered, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let sender = env.session(IAC, "mac").await;
    let x = env.session(NIMBUS, "mbp").await;
    let m = env.send(&sender, Address::Agent(x.agent.id), "again").await;

    let first = env.msgs.inbox(&x, InboxFilter::default()).await.unwrap();
    env.clock.advance(Duration::minutes(1));
    let second = env.msgs.inbox(&x, InboxFilter::default()).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].message.id, m);
    // Delivery is stamped once, at the first read.
    assert_eq!(first[0].delivered_at, second[0].delivered_at);
    assert_eq!(second[0].read_at, None);

    env.msgs.mark_read(Caller::Agent(&x), &[m]).await.unwrap();
    let third = env.msgs.inbox(&x, InboxFilter::default()).await.unwrap();
    assert_eq!(third[0].read_at, Some(env.clock.now()));
});

store_matrix!(body_cap_bytes, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let sender = env.session(IAC, "mac").await;
    let x = env.session(NIMBUS, "mbp").await;

    // 16,383 four-byte emoji + 4 ASCII bytes = 65,536 bytes.
    let exact = format!("{}abcd", "🦀".repeat(16_383));
    assert_eq!(exact.len(), 65_536);
    let sent = env
        .msgs
        .send(&actor(&sender), msg(Address::Agent(x.agent.id), &exact))
        .await
        .unwrap();
    assert_eq!(sent.body, exact);
    let got = env.msgs.inbox(&x, InboxFilter::default()).await.unwrap();
    assert_eq!(got[0].message.body, exact);

    let over = format!("{exact}e");
    let err = env
        .msgs
        .send(&actor(&sender), msg(Address::Agent(x.agent.id), &over))
        .await
        .unwrap_err();
    assert!(matches!(err, MessageError::BodyTooLarge { limit: 65_536 }));
    assert_eq!(err.to_string(), "body exceeds 65536 bytes");
});

store_matrix!(cross_project_send, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let iac = env.session(IAC, "mac").await;
    let nimbus = env.session(NIMBUS, "mbp").await;

    let m = env
        .send(&iac, Address::Project(project_of(&nimbus)), "rotate creds")
        .await;
    let items = env
        .msgs
        .inbox(&nimbus, InboxFilter::default())
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    let got = &items[0].message;
    assert_eq!(got.id, m);
    assert_eq!(got.from, actor(&iac));
    assert_eq!(got.project_id, Some(project_of(&nimbus)));
    assert_eq!(got.to, Address::Project(project_of(&nimbus)));
    // Not a broadcast to the sender's own project.
    assert!(env.inbox_ids(&iac).await.is_empty());
});

store_matrix!(
    search_finds_subject_and_body,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let iac = env.session(IAC, "mac").await;
        let nimbus = env.session(NIMBUS, "mbp").await;

        env.clock.advance(Duration::seconds(1));
        let by_subject = env
            .msgs
            .send(
                &actor(&iac),
                NewMessage {
                    subject: Some("Postgres failover drill".into()),
                    ..msg(Address::Agent(nimbus.agent.id), "see runbook")
                },
            )
            .await
            .unwrap()
            .id;
        let by_body = env
            .send(
                &iac,
                Address::Project(project_of(&nimbus)),
                "the failover finished cleanly",
            )
            .await;
        env.send(&iac, Address::Agent(nimbus.agent.id), "unrelated")
            .await;

        let ids = |v: Vec<coord::domain::Message>| v.iter().map(|m| m.id).collect::<Vec<_>>();
        // Newest first; case-insensitive.
        assert_eq!(
            ids(env.msgs.search("FAILOVER", None, 10).await.unwrap()),
            vec![by_body, by_subject]
        );
        assert_eq!(
            ids(env.msgs.search("failover drill", None, 10).await.unwrap()),
            vec![by_subject]
        );
        assert_eq!(
            ids(env.msgs.search("failover", None, 1).await.unwrap()),
            vec![by_body]
        );
        assert!(
            env.msgs
                .search("failover", Some(project_of(&iac)), 10)
                .await
                .unwrap()
                .is_empty()
        );
        // FTS syntax characters are treated as text, not query operators.
        assert!(
            env.msgs
                .search("\"unbalanced AND (", None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(env.msgs.search("   ", None, 10).await.unwrap().is_empty());
    }
);

store_matrix!(human_inbox, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let x = env.session(IAC, "mac").await;
    let m = env.send(&x, Address::Human, "need a decision").await;

    let items = env.msgs.human_inbox(InboxFilter::default()).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].message.id, m);
    assert_eq!(items[0].message.to, Address::Human);
    assert_eq!(items[0].message.project_id, Some(project_of(&x)));
    // Agents never see human-addressed mail.
    assert!(env.inbox_ids(&x).await.is_empty());

    env.msgs.ack(Caller::Human, &[m]).await.unwrap();
    assert!(
        env.msgs
            .human_inbox(InboxFilter::default())
            .await
            .unwrap()
            .is_empty()
    );
});

store_matrix!(task_thread_delivery, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let creator = env.session(IAC, "mac").await;
    let worker = env.session(IAC, "cac-1").await;
    let bystander = env.session(IAC, "mbp").await;

    let task = env
        .tasks
        .create(
            Caller::Agent(&creator),
            NewTask {
                project_id: project_of(&creator),
                title: "rotate creds".into(),
                body: "rotate the db creds".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    let thread = env.msgs.thread(task.id).await.unwrap();
    assert_eq!(thread.len(), 1);
    let first = &thread[0];
    assert_eq!(first.body, "rotate the db creds");
    assert_eq!(first.subject.as_deref(), Some("rotate creds"));
    assert_eq!(first.task_id, Some(task.id));
    assert_eq!(first.to, Address::Task(task.id));
    assert_eq!(first.from, actor(&creator));
    // Not delivered back to its author, nor to anyone before the task has an owner.
    assert!(env.inbox_ids(&creator).await.is_empty());
    assert!(env.inbox_ids(&worker).await.is_empty());

    assert!(matches!(
        env.tasks.claim(&worker, task.id).await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    // The owner now receives the thread, starting with the task's first message.
    assert_eq!(env.inbox_ids(&worker).await, vec![first.id]);
    env.msgs
        .ack(Caller::Agent(&worker), &[first.id])
        .await
        .unwrap();

    let reply = env.send(&worker, Address::Task(task.id), "on it").await;
    assert_eq!(env.inbox_ids(&creator).await, vec![reply]);
    assert!(env.inbox_ids(&worker).await.is_empty());
    assert!(env.inbox_ids(&bystander).await.is_empty());
    let filtered = env
        .msgs
        .inbox(
            &creator,
            InboxFilter {
                task: Some(task.id),
                ..InboxFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);

    let thread = env.msgs.thread(task.id).await.unwrap();
    assert_eq!(
        thread.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![first.id, reply]
    );
});

store_matrix!(
    human_task_thread_and_promotion,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let x = env.session(IAC, "mac").await;
        let source = env.send(&x, Address::Human, "the build is red").await;

        let task = env
            .tasks
            .create_from_message(
                Caller::Human,
                source,
                NewTask {
                    project_id: project_of(&x),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();
        // Empty title / body default from the promoted message.
        assert_eq!(task.title, "hello");
        assert_eq!(task.body, "the build is red");
        let thread = env.msgs.thread(task.id).await.unwrap();
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].reply_to, Some(source));
        assert_eq!(thread[0].from, Actor::Human);

        // The human created the task, so agent posts in its thread reach the human inbox.
        let post = env.send(&x, Address::Task(task.id), "investigating").await;
        let human: Vec<_> = env
            .msgs
            .human_inbox(InboxFilter::default())
            .await
            .unwrap()
            .iter()
            .map(|i| i.message.id)
            .collect();
        assert_eq!(human, vec![source, post]);

        let missing = env
            .tasks
            .create_from_message(
                Caller::Human,
                MessageId::new(),
                NewTask {
                    project_id: project_of(&x),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            missing,
            TaskError::Message(MessageError::NotFound("message"))
        ));
    }
);

store_matrix!(inbox_filters_and_unknown_recipients, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let sender = env.session(IAC, "mac").await;
    let x = env.session(NIMBUS, "mbp").await;
    env.send(&sender, Address::Agent(x.agent.id), "normal")
        .await;
    let urgent = env
        .msgs
        .send(
            &actor(&sender),
            NewMessage {
                importance: Importance::Urgent,
                ..msg(Address::Agent(x.agent.id), "fire")
            },
        )
        .await
        .unwrap()
        .id;
    let only_urgent = env
        .msgs
        .inbox(
            &x,
            InboxFilter {
                importance: Some(Importance::Urgent),
                ..InboxFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(only_urgent.len(), 1);
    assert_eq!(only_urgent[0].message.id, urgent);
    let limited = env
        .msgs
        .inbox(
            &x,
            InboxFilter {
                limit: Some(1),
                ..InboxFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(limited.len(), 1);
    assert_ne!(limited[0].message.id, urgent);

    let err = env
        .msgs
        .send(
            &actor(&sender),
            msg(Address::Agent(coord::domain::AgentId::new()), "x"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessageError::NotFound("agent")));
    let err = env
        .msgs
        .send(
            &actor(&sender),
            NewMessage {
                reply_to: Some(MessageId::new()),
                ..msg(Address::Human, "x")
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessageError::NotFound("message")));
});

store_matrix!(
    concurrent_sessions_ack_once,
    |store: Arc<dyn Store>| async move {
        let env = Arc::new(Env::new(store));
        let sender = env.session(IAC, "mac").await;
        let mut sessions = Vec::new();
        for _ in 0..8 {
            sessions.push(env.session(NIMBUS, "mbp").await);
        }
        let direct = env
            .send(&sender, Address::Agent(sessions[0].agent.id), "direct")
            .await;
        let broadcast = env
            .send(&sender, Address::Project(project_of(&sessions[0])), "all")
            .await;

        // Every session reads and acks at once: each message is acked exactly once for the agent.
        let handles: Vec<_> = sessions
            .iter()
            .cloned()
            .map(|r| {
                let env = env.clone();
                tokio::spawn(async move {
                    let ids: Vec<MessageId> = env
                        .msgs
                        .inbox(&r, InboxFilter::default())
                        .await
                        .unwrap()
                        .iter()
                        .map(|i| i.message.id)
                        .collect();
                    env.msgs.ack(Caller::Agent(&r), &ids).await.unwrap()
                })
            })
            .collect();
        let mut acked = 0;
        for h in handles {
            acked += h.await.unwrap();
        }
        assert_eq!(
            acked, 2,
            "direct {direct} and broadcast {broadcast} acked once each"
        );
        for r in &sessions {
            assert!(env.inbox_ids(r).await.is_empty());
        }
    }
);
