#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{Actor, Address, Importance, MessageId, ProjectId, SessionId};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::messages::{InboxFilter, MessageError, MessageService, NewMessage};
use axle::service::tasks::{Caller, ClaimOutcome, NewTask, TaskError, TaskService};
use axle::store::Store;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-27 9:00 UTC);
const INFRA: &str = "github.com/acme/platform-infra";
const WEB_APP: &str = "github.com/acme/web-app";

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
                Duration::seconds(90),
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
    let sender = env.session(INFRA, "mac").await;
    let x1 = env.session(WEB_APP, "laptop-01").await;
    let x2 = env.session(WEB_APP, "laptop-01").await;
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
    let sender = env.session(INFRA, "mac").await;
    let x1 = env.session(WEB_APP, "laptop-01").await;
    let x2 = env.session(WEB_APP, "laptop-01").await;

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
        let sender = env.session(WEB_APP, "mac").await;
        let early = env.session(WEB_APP, "early").await;
        let web_app = project_of(&sender);

        let m1 = env.send(&sender, Address::Project(web_app), "first").await;
        env.clock.advance(Duration::seconds(1));
        let late = env.session(WEB_APP, "late").await;
        let m2 = env.send(&sender, Address::Project(web_app), "second").await;

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
        let found = env.msgs.search("first", Some(web_app), 10).await.unwrap();
        assert_eq!(found.iter().map(|m| m.id).collect::<Vec<_>>(), vec![m1]);
    }
);

store_matrix!(unacked_redelivered, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let sender = env.session(INFRA, "mac").await;
    let x = env.session(WEB_APP, "laptop-01").await;
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
    let sender = env.session(INFRA, "mac").await;
    let x = env.session(WEB_APP, "laptop-01").await;

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
    let infra = env.session(INFRA, "mac").await;
    let web_app = env.session(WEB_APP, "laptop-01").await;

    let m = env
        .send(
            &infra,
            Address::Project(project_of(&web_app)),
            "rotate creds",
        )
        .await;
    let items = env
        .msgs
        .inbox(&web_app, InboxFilter::default())
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    let got = &items[0].message;
    assert_eq!(got.id, m);
    assert_eq!(got.from, actor(&infra));
    assert_eq!(got.project_id, Some(project_of(&web_app)));
    assert_eq!(got.to, Address::Project(project_of(&web_app)));
    // Not a broadcast to the sender's own project.
    assert!(env.inbox_ids(&infra).await.is_empty());
});

store_matrix!(
    search_finds_subject_and_body,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let infra = env.session(INFRA, "mac").await;
        let web_app = env.session(WEB_APP, "laptop-01").await;

        env.clock.advance(Duration::seconds(1));
        let by_subject = env
            .msgs
            .send(
                &actor(&infra),
                NewMessage {
                    subject: Some("Postgres failover drill".into()),
                    ..msg(Address::Agent(web_app.agent.id), "see runbook")
                },
            )
            .await
            .unwrap()
            .id;
        let by_body = env
            .send(
                &infra,
                Address::Project(project_of(&web_app)),
                "the failover finished cleanly",
            )
            .await;
        env.send(&infra, Address::Agent(web_app.agent.id), "unrelated")
            .await;

        let ids = |v: Vec<axle::domain::Message>| v.iter().map(|m| m.id).collect::<Vec<_>>();
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
                .search("failover", Some(project_of(&infra)), 10)
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
    let x = env.session(INFRA, "mac").await;
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

store_matrix!(
    unread_counts_by_importance,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let x = env.session(INFRA, "mac").await;
        let y = env.session(INFRA, "build-01").await;
        for (to, importance) in [
            (Address::Human, Importance::Normal),
            (Address::Human, Importance::Urgent),
            (Address::Human, Importance::Urgent),
            (Address::Agent(y.agent.id), Importance::Urgent),
            (Address::Project(project_of(&y)), Importance::Urgent),
        ] {
            env.msgs
                .send(
                    &actor(&x),
                    NewMessage {
                        importance,
                        ..msg(to, "ping")
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(env.msgs.human_unread_count(None).await.unwrap(), 3);
        let urgent = Some(Importance::Urgent);
        assert_eq!(env.msgs.human_unread_count(urgent).await.unwrap(), 2);
        assert_eq!(
            env.msgs
                .human_unread_count(Some(Importance::High))
                .await
                .unwrap(),
            0
        );
        // The filter covers agent rows and not-yet-materialised project mail too.
        assert_eq!(
            env.store
                .unread_count(
                    axle::store::Recipient::Agent {
                        agent: y.agent.id,
                        session: y.session.id
                    },
                    urgent
                )
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            env.store
                .unread_count(
                    axle::store::Recipient::Agent {
                        agent: y.agent.id,
                        session: y.session.id
                    },
                    Some(Importance::Normal)
                )
                .await
                .unwrap(),
            0
        );
        // Writes nothing: the human inbox still returns all three.
        assert_eq!(
            env.msgs
                .human_inbox(InboxFilter::default())
                .await
                .unwrap()
                .len(),
            3
        );
    }
);

store_matrix!(task_thread_delivery, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let creator = env.session(INFRA, "mac").await;
    let worker = env.session(INFRA, "build-01").await;
    let bystander = env.session(INFRA, "laptop-01").await;

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
        let x = env.session(INFRA, "mac").await;
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
    let sender = env.session(INFRA, "mac").await;
    let x = env.session(WEB_APP, "laptop-01").await;
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
            msg(Address::Agent(axle::domain::AgentId::new()), "x"),
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
        let sender = env.session(INFRA, "mac").await;
        let mut sessions = Vec::new();
        for _ in 0..8 {
            sessions.push(env.session(WEB_APP, "laptop-01").await);
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

store_matrix!(
    feed_pages_and_receipts,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session(INFRA, "mac").await;
        let b = env.session(WEB_APP, "laptop-01").await;
        let web_app = project_of(&b);
        let m1 = env.send(&a, Address::Agent(b.agent.id), "one").await;
        let m2 = env.send(&a, Address::Project(web_app), "two").await;
        let m3 = env.send(&b, Address::Human, "three").await;
        let ids = |ms: Vec<axle::domain::Message>| ms.iter().map(|m| m.id).collect::<Vec<_>>();

        // Newest first, paged by a message cursor.
        let page = env.store.list_messages(None, None, 2).await.unwrap();
        assert_eq!(ids(page), vec![m3, m2]);
        let next = env.store.list_messages(None, Some(m2), 2).await.unwrap();
        assert_eq!(ids(next), vec![m1]);
        assert!(
            env.store
                .list_messages(None, Some(m1), 2)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(env
                .store
                .list_messages(Some(web_app), None, 10)
                .await
                .unwrap()),
            vec![m3, m2, m1]
        );
        assert!(
            env.store
                .list_messages(Some(project_of(&a)), None, 10)
                .await
                .unwrap()
                .is_empty()
        );

        let state = |r: &axle::store::MessageReceipt| r.state();
        let receipts = env.store.message_receipts(&[m1, m2, m3]).await.unwrap();
        assert_eq!(
            receipts.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![m1, m2, m3]
        );
        assert_eq!(
            receipts.iter().map(state).collect::<Vec<_>>(),
            ["sent", "sent", "sent"]
        );
        assert_eq!(receipts[0].recipients, 1);
        assert_eq!(receipts[1].recipients, 0, "project rows appear when read");

        // b fetches its inbox (m1 and m2 delivered), reads m1, then replies to it.
        assert_eq!(env.inbox_ids(&b).await, vec![m1, m2]);
        env.msgs.mark_read(Caller::Agent(&b), &[m1]).await.unwrap();
        let receipts = env.store.message_receipts(&[m1, m2]).await.unwrap();
        assert_eq!(
            receipts.iter().map(state).collect::<Vec<_>>(),
            ["read", "delivered"]
        );
        env.clock.advance(Duration::seconds(1));
        env.msgs
            .send(
                &actor(&b),
                NewMessage {
                    reply_to: Some(m1),
                    ..msg(Address::Agent(a.agent.id), "re: one")
                },
            )
            .await
            .unwrap();
        // The human inbox delivers m3.
        env.msgs.human_inbox(InboxFilter::default()).await.unwrap();
        let receipts = env.store.message_receipts(&[m1, m3]).await.unwrap();
        assert_eq!(
            receipts.iter().map(state).collect::<Vec<_>>(),
            ["replied", "delivered"]
        );
        assert!(env.store.message_receipts(&[]).await.unwrap().is_empty());
    }
);

store_matrix!(
    task_thread_paged_newest_first,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session(INFRA, "mac").await;
        let task = env
            .tasks
            .create(
                Caller::Agent(&a),
                NewTask {
                    project_id: project_of(&a),
                    title: "t".into(),
                    body: "first".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap();
        let mut sent = Vec::new();
        for i in 0..4 {
            sent.push(env.send(&a, Address::Task(task.id), &format!("m{i}")).await);
        }
        let bodies =
            |ms: &[axle::domain::Message]| ms.iter().map(|m| m.body.clone()).collect::<Vec<_>>();
        let page = env.store.task_thread_page(task.id, None, 2).await.unwrap();
        assert_eq!(bodies(&page), ["m3", "m2"]);
        let rest = env
            .store
            .task_thread_page(task.id, Some(page[1].id), 10)
            .await
            .unwrap();
        assert_eq!(bodies(&rest), ["m1", "m0", "first"]);
        // Only this task's thread.
        env.send(&a, Address::Project(project_of(&a)), "elsewhere")
            .await;
        assert_eq!(
            env.store.task_thread_page(task.id, None, 1).await.unwrap()[0].body,
            "m3"
        );
    }
);

store_matrix!(
    list_agents_newest_first,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a1 = env.session(INFRA, "mac").await;
        let _again = env.session(INFRA, "mac").await; // same agent, second session
        env.clock.advance(Duration::seconds(1));
        let b = env.session(WEB_APP, "laptop-01").await;
        let agents = env.store.list_agents(10).await.unwrap();
        assert_eq!(
            agents.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![b.agent.id, a1.agent.id],
            "one row per agent, however many sessions"
        );
        assert_eq!(env.store.list_agents(1).await.unwrap().len(), 1);
    }
);

store_matrix!(
    quick_reply_options_round_trip,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let agent = env.session(INFRA, "mac").await;
        let asked = env
            .msgs
            .send_with_options(
                &actor(&agent),
                NewMessage {
                    importance: Importance::Urgent,
                    ..msg(Address::Human, "Ship it?")
                },
                &["  Yes, ship  ".into(), "No".into(), "Wait for CI".into()],
            )
            .await
            .unwrap();
        let want = vec![
            "Yes, ship".to_string(),
            "No".to_string(),
            "Wait for CI".to_string(),
        ];
        assert_eq!(asked.options, want, "trimmed, in order");

        // Every read path returns them.
        let got = env.store.get_message(asked.id).await.unwrap().unwrap();
        assert_eq!(got.options, want);
        let inbox = env.msgs.human_inbox(InboxFilter::default()).await.unwrap();
        assert_eq!(inbox[0].message.options, want);
        let attention = env.store.human_attention(10).await.unwrap();
        assert_eq!(attention[0].options, want);
        let found = env.msgs.search("ship", None, 10).await.unwrap();
        assert_eq!(found[0].options, want);
        let feed = env.store.list_messages(None, None, 10).await.unwrap();
        assert_eq!(feed[0].options, want);

        // Plain mail has none; the human's reply is an ordinary message.
        let plain = env.send(&agent, Address::Human, "fyi").await;
        let plain = env.store.get_message(plain).await.unwrap().unwrap();
        assert!(plain.options.is_empty());
        let reply = env
            .msgs
            .send_as_human(NewMessage {
                reply_to: Some(asked.id),
                ..msg(Address::Agent(agent.agent.id), "Yes, ship")
            })
            .await
            .unwrap();
        assert!(reply.options.is_empty());

        // Options are only for the human, and are checked before anything is written.
        let other = env.session(WEB_APP, "mac").await;
        let err = env
            .msgs
            .send_with_options(
                &actor(&agent),
                msg(Address::Agent(other.agent.id), "?"),
                &["a".into(), "b".into()],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MessageError::OptionsNeedHuman), "{err:?}");
        assert!(env.inbox_ids(&other).await.is_empty());
    }
);

store_matrix!(
    questions_need_the_human_whatever_their_importance,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let agent = env.session(INFRA, "mac").await;
        let mut asked = Vec::new();
        for body in ["Merge now?", "Tag a release?"] {
            env.clock.advance(Duration::seconds(1));
            let m = env
                .msgs
                .send_with_options(
                    &actor(&agent),
                    msg(Address::Human, body),
                    &["Yes".into(), "No".into()],
                )
                .await
                .unwrap();
            asked.push(m.id);
        }
        // Plain normal mail still waits in the inbox only.
        env.send(&agent, Address::Human, "fyi").await;
        let attention = |v: Vec<axle::domain::Message>| v.into_iter().map(|m| m.id).collect();
        let got: Vec<MessageId> = attention(env.store.human_attention(10).await.unwrap());
        assert_eq!(got, asked, "a normal-importance question needs the human");

        // Answered or acked, it leaves.
        env.msgs
            .send_as_human(NewMessage {
                reply_to: Some(asked[0]),
                ..msg(Address::Agent(agent.agent.id), "Yes")
            })
            .await
            .unwrap();
        let got: Vec<MessageId> = attention(env.store.human_attention(10).await.unwrap());
        assert_eq!(got, vec![asked[1]]);
        env.msgs.ack(Caller::Human, &[asked[1]]).await.unwrap();
        assert!(env.store.human_attention(10).await.unwrap().is_empty());
    }
);
