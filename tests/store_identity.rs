#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::TestClock;
use axle::domain::{SessionId, TokenId};
use axle::service::identity::{IdentityError, IdentityInput, IdentityService};
use axle::store::{Store, open_store};
use time::macros::datetime;

fn input(token: TokenId, project_key: Option<&str>, session: SessionId) -> IdentityInput {
    IdentityInput {
        project_key: project_key.map(str::to_string),
        host: "mac".into(),
        harness: "claude-code".into(),
        session,
        token,
    }
}

store_matrix!(first_contact_creates_project_agent_session, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
    let token = common::token(&*store).await;
    let key = "github.com/reyemtech/iac";

    let first =
        IdentityService::resolve(&*store, &clock, input(token, Some(key), SessionId::new()))
            .await
            .unwrap();
    let project = first.project.clone().expect("project created");
    assert_eq!(project.key, key);
    assert_eq!(project.display_name, "reyemtech/iac");
    assert_eq!(first.agent.project_id, Some(project.id));
    assert_eq!(first.agent.host, "mac");
    assert_eq!(first.agent.harness, "claude-code");
    assert_eq!(first.session.agent_id, first.agent.id);
    assert_eq!(first.session.started_at, datetime!(2026-11-27 9:00 UTC));

    let second =
        IdentityService::resolve(&*store, &clock, input(token, Some(key), SessionId::new()))
            .await
            .unwrap();
    assert_eq!(second.project.unwrap().id, project.id);
    assert_eq!(second.agent.id, first.agent.id);
    assert_ne!(second.session.id, first.session.id);

    let stored = store.get_session(first.session.id).await.unwrap();
    assert_eq!(stored, Some(first.session.clone()));
    assert_eq!(
        store.list_sessions(Some(project.id)).await.unwrap().len(),
        2
    );
});

store_matrix!(no_project_is_allowed, |store: Arc<dyn Store>| async move {
    let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
    let token = common::token(&*store).await;

    let first = IdentityService::resolve(&*store, &clock, input(token, None, SessionId::new()))
        .await
        .unwrap();
    assert!(first.project.is_none());
    assert_eq!(first.agent.project_id, None);

    let second = IdentityService::resolve(&*store, &clock, input(token, None, SessionId::new()))
        .await
        .unwrap();
    assert_eq!(
        second.agent.id, first.agent.id,
        "NULL project still dedupes"
    );
    assert!(store.list_projects().await.unwrap().is_empty());
});

store_matrix!(rename_project_changes_display_only, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
    let token = common::token(&*store).await;
    let key = "github.com/reyemtech/iac";
    let resolved =
        IdentityService::resolve(&*store, &clock, input(token, Some(key), SessionId::new()))
            .await
            .unwrap();
    let project = resolved.project.unwrap();

    store.rename_project(project.id, "IaC").await.unwrap();

    let renamed = store.get_project_by_key(key).await.unwrap().unwrap();
    assert_eq!(renamed.id, project.id);
    assert_eq!(renamed.key, key);
    assert_eq!(renamed.display_name, "IaC");

    // A later first-contact upsert must not reset the human-edited name.
    IdentityService::resolve(&*store, &clock, input(token, Some(key), SessionId::new()))
        .await
        .unwrap();
    let after = store.get_project_by_key(key).await.unwrap().unwrap();
    assert_eq!(after.display_name, "IaC");
});

store_matrix!(resolve_same_session_is_idempotent, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
    let token = common::token(&*store).await;
    let session = SessionId::new();
    let a = IdentityService::resolve(&*store, &clock, input(token, Some("x/y/z"), session))
        .await
        .unwrap();
    clock.advance(time::Duration::minutes(1));
    let b = IdentityService::resolve(&*store, &clock, input(token, Some("x/y/z"), session))
        .await
        .unwrap();
    assert_eq!(a.session, b.session);
});

store_matrix!(heartbeat_and_tool_call_touch_session, |store: Arc<
    dyn Store,
>| async move {
    let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
    let token = common::token(&*store).await;
    let r = IdentityService::resolve(&*store, &clock, input(token, None, SessionId::new()))
        .await
        .unwrap();

    store
        .touch_heartbeat(r.session.id, datetime!(2026-11-27 9:00:30.123456 UTC))
        .await
        .unwrap();
    let s = store.get_session(r.session.id).await.unwrap().unwrap();
    assert_eq!(
        s.last_heartbeat_at,
        datetime!(2026-11-27 9:00:30.123456 UTC)
    );
    assert_eq!(s.last_tool_call_at, datetime!(2026-11-27 9:00 UTC));

    store
        .touch_tool_call(r.session.id, datetime!(2026-11-27 9:05 UTC))
        .await
        .unwrap();
    let s = store.get_session(r.session.id).await.unwrap().unwrap();
    assert_eq!(s.last_tool_call_at, datetime!(2026-11-27 9:05 UTC));
    assert_eq!(s.last_heartbeat_at, datetime!(2026-11-27 9:05 UTC));

    assert!(
        store
            .touch_heartbeat(SessionId::new(), clock_now())
            .await
            .is_err()
    );
});

fn clock_now() -> time::OffsetDateTime {
    datetime!(2026-11-27 10:00 UTC)
}

store_matrix!(
    list_sessions_filters_by_project,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
        let token = common::token(&*store).await;
        let a = IdentityService::resolve(
            &*store,
            &clock,
            input(token, Some("h/o/a"), SessionId::new()),
        )
        .await
        .unwrap();
        IdentityService::resolve(
            &*store,
            &clock,
            input(token, Some("h/o/b"), SessionId::new()),
        )
        .await
        .unwrap();
        IdentityService::resolve(&*store, &clock, input(token, None, SessionId::new()))
            .await
            .unwrap();

        let only_a = store
            .list_sessions(a.project.as_ref().map(|p| p.id))
            .await
            .unwrap();
        assert_eq!(only_a, vec![(a.agent.clone(), a.session.clone())]);
        assert_eq!(store.list_sessions(None).await.unwrap().len(), 3);

        let keys: Vec<_> = store
            .list_projects()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.key)
            .collect();
        assert_eq!(keys, vec!["h/o/a", "h/o/b"]);
    }
);

store_matrix!(
    session_id_reused_by_other_agent_is_rejected,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
        let token = common::token(&*store).await;
        let session = SessionId::new();
        IdentityService::resolve(&*store, &clock, input(token, Some("h/o/a"), session))
            .await
            .unwrap();
        let err = IdentityService::resolve(&*store, &clock, input(token, Some("h/o/b"), session))
            .await
            .unwrap_err();
        assert!(
            matches!(err, IdentityError::SessionAgentMismatch { .. }),
            "{err}"
        );
    }
);

#[tokio::test]
async fn open_store_creates_and_reopens_sqlite_file() {
    let path = std::env::temp_dir().join(format!("axle-test-{}.db", uuid::Uuid::now_v7()));
    let backend = axle::config::Backend::from_url(&format!("sqlite://{}", path.display())).unwrap();
    let now = datetime!(2026-11-27 9:00 UTC);

    let store = open_store(&backend).await.unwrap();
    store.upsert_project("h/o/r", "o/r", now).await.unwrap();
    drop(store);

    let reopened = open_store(&backend).await.unwrap();
    assert!(
        reopened
            .get_project_by_key("h/o/r")
            .await
            .unwrap()
            .is_some()
    );
    drop(reopened);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

store_matrix!(
    session_presented_with_other_token_is_rejected,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
        let token = common::token(&*store).await;
        let other = common::token(&*store).await;
        let session = SessionId::new();
        IdentityService::resolve(&*store, &clock, input(token, Some("h/o/a"), session))
            .await
            .unwrap();
        let err = IdentityService::resolve(&*store, &clock, input(other, Some("h/o/a"), session))
            .await
            .unwrap_err();
        assert!(
            matches!(err, IdentityError::SessionTokenMismatch { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("token"), "{err}");
    }
);

store_matrix!(
    load_returns_resolved_session,
    |store: Arc<dyn Store>| async move {
        let clock = TestClock::new(datetime!(2026-11-27 9:00 UTC));
        let token = common::token(&*store).await;
        let r = IdentityService::resolve(
            &*store,
            &clock,
            input(token, Some("h/o/a"), SessionId::new()),
        )
        .await
        .unwrap();
        assert_eq!(
            IdentityService::load(&*store, r.session.id).await.unwrap(),
            Some(r)
        );
        assert_eq!(
            IdentityService::load(&*store, SessionId::new())
                .await
                .unwrap(),
            None
        );
    }
);
