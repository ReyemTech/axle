#[macro_use]
mod common;

use std::sync::Arc;

use coord::clock::{Clock, TestClock};
use coord::domain::{AgentId, ProjectId, SessionId, TaskEventKind, TaskId, TaskStatus, TokenId};
use coord::events::{Event, EventBus};
use coord::service::auth::{AuthError, AuthService};
use coord::store::{Store, StoreError};
use time::Duration;
use time::macros::datetime;

store_matrix!(token_lifecycle, |store: Arc<dyn Store>| async move {
    let clock = Arc::new(TestClock::new(datetime!(2026-11-27 9:00 UTC)));
    let auth = AuthService::new(store.clone(), clock.clone());

    let (id, plaintext) = auth.create("mac-claude").await.unwrap();
    assert!(plaintext.starts_with("coord_"));
    let (other, other_plain) = auth.create("ci").await.unwrap();
    assert_ne!(plaintext, other_plain);

    let listed = auth.list().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].label, "mac-claude");
    assert_eq!(listed[0].created_at, datetime!(2026-11-27 9:00 UTC));
    assert_eq!(listed[0].last_used_at, None);
    assert_eq!(listed[0].revoked_at, None);

    let used_at = datetime!(2026-11-27 9:30 UTC);
    assert_eq!(auth.verify(&plaintext, used_at).await.unwrap(), Some(id));
    assert_eq!(
        auth.verify(&other_plain, used_at).await.unwrap(),
        Some(other)
    );
    assert_eq!(auth.verify("coord_wrong", used_at).await.unwrap(), None);
    assert_eq!(auth.verify("", used_at).await.unwrap(), None);
    let no_prefix = plaintext.trim_start_matches("coord_");
    assert_eq!(auth.verify(no_prefix, used_at).await.unwrap(), None);
    assert_eq!(auth.list().await.unwrap()[0].last_used_at, Some(used_at));

    clock.advance(Duration::hours(1));
    auth.revoke(id).await.unwrap();
    auth.revoke(id).await.unwrap(); // idempotent
    assert_eq!(auth.verify(&plaintext, clock.now()).await.unwrap(), None);
    assert_eq!(
        auth.verify(&other_plain, clock.now()).await.unwrap(),
        Some(other)
    );
    assert_eq!(
        auth.list().await.unwrap()[0].revoked_at,
        Some(datetime!(2026-11-27 10:00 UTC))
    );

    assert!(matches!(
        auth.revoke(TokenId::new()).await,
        Err(AuthError::Store(StoreError::NotFound("token")))
    ));
    assert!(matches!(
        auth.create("  ").await,
        Err(AuthError::EmptyLabel)
    ));
});

store_matrix!(
    sessions_reference_real_tokens,
    |store: Arc<dyn Store>| async move {
        let now = datetime!(2026-11-27 9:00 UTC);
        let agent = store.upsert_agent(None, "mac", "codex", now).await.unwrap();
        assert!(
            store
                .create_session(SessionId::new(), agent.id, TokenId::new(), now)
                .await
                .is_err(),
            "a session's token must exist"
        );
        let token = common::token(&*store).await;
        store
            .create_session(SessionId::new(), agent.id, token, now)
            .await
            .unwrap();
    }
);

store_matrix!(ping_and_outbox, |store: Arc<dyn Store>| async move {
    let clock = Arc::new(TestClock::new(datetime!(2026-11-27 9:00 UTC)));
    store.ping().await.unwrap();

    let project = store
        .upsert_project("h/o/r", "o/r", clock.now())
        .await
        .unwrap();
    let bus = EventBus::new(store.clone(), clock.clone());
    let mut rx = bus.subscribe();
    let event = Event::TaskChanged {
        id: TaskId::new(),
        project_id: project.id,
        status: TaskStatus::Open,
        kind: TaskEventKind::Created,
    };
    bus.publish(event.clone()).await.unwrap();
    assert_eq!(rx.recv().await.unwrap(), event);

    // Publishing with no subscribers is not an error.
    drop(rx);
    bus.publish(Event::MessageCreated {
        id: coord::domain::MessageId::new(),
        to: coord::domain::Address::Agent(AgentId::new()),
        project_id: None,
    })
    .await
    .unwrap();

    // An outbox row needs a real project.
    assert!(
        bus.publish(Event::TaskChanged {
            id: TaskId::new(),
            project_id: ProjectId::new(),
            status: TaskStatus::Open,
            kind: TaskEventKind::Created,
        })
        .await
        .is_err()
    );
});
