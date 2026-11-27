//! Enrollment store contract: one-time codes and the device flow, on every backend.

#[macro_use]
mod common;

use std::sync::Arc;

use axle::domain::{EnrollmentId, EnrollmentKind, EnrollmentStatus, TokenId};
use axle::store::{NewEnrollment, NewToken, PollOutcome, Store};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-27 9:00 UTC);

fn code_row<'a>(secret: &'a str, label: &'a str, now: OffsetDateTime) -> NewEnrollment<'a> {
    NewEnrollment {
        id: EnrollmentId::new(),
        kind: EnrollmentKind::Code,
        secret_hash: secret,
        user_code_hash: None,
        label,
        host: "",
        requester_ip: None,
        now,
        expires_at: now + Duration::minutes(15),
    }
}

fn device_row<'a>(secret: &'a str, user: &'a str, now: OffsetDateTime) -> NewEnrollment<'a> {
    NewEnrollment {
        id: EnrollmentId::new(),
        kind: EnrollmentKind::Device,
        secret_hash: secret,
        user_code_hash: Some(user),
        label: "",
        host: "laptop",
        requester_ip: Some("203.0.113.9"),
        now,
        expires_at: now + Duration::minutes(10),
    }
}

fn tok(hash: &str) -> NewToken<'_> {
    NewToken {
        id: TokenId::new(),
        hash,
    }
}

store_matrix!(code_lifecycle, |store: Arc<dyn Store>| async move {
    store
        .insert_enrollment(&code_row("s1", "work laptop", T0))
        .await
        .unwrap();
    let t = tok("h1");
    let tid = t.id;
    let e = store
        .consume_code("s1", "laptop", T0 + Duration::minutes(14), &t)
        .await
        .unwrap()
        .expect("consumed");
    assert_eq!(e.status, EnrollmentStatus::Consumed);
    assert_eq!(e.token_id, Some(tid));
    assert_eq!(e.host, "laptop");
    let tokens = store.list_tokens().await.unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].id, tid);
    assert_eq!(tokens[0].label, "work laptop (laptop)");

    let again = store
        .consume_code("s1", "laptop", T0 + Duration::minutes(14), &tok("h2"))
        .await
        .unwrap();
    assert!(again.is_none());
    assert_eq!(store.list_tokens().await.unwrap().len(), 1);
});

store_matrix!(
    code_expires_after_15_minutes,
    |store: Arc<dyn Store>| async move {
        store
            .insert_enrollment(&code_row("s1", "work laptop", T0))
            .await
            .unwrap();
        let got = store
            .consume_code("s1", "laptop", T0 + Duration::minutes(15), &tok("h1"))
            .await
            .unwrap();
        assert!(got.is_none());
        assert!(store.list_tokens().await.unwrap().is_empty());
    }
);

store_matrix!(
    code_unknown_secret_is_none,
    |store: Arc<dyn Store>| async move {
        let got = store
            .consume_code("nope", "laptop", T0, &tok("h1"))
            .await
            .unwrap();
        assert!(got.is_none());
        assert!(store.list_tokens().await.unwrap().is_empty());
    }
);

store_matrix!(
    code_consumed_once_under_race,
    |store: Arc<dyn Store>| async move {
        store
            .insert_enrollment(&code_row("s1", "ci box", T0))
            .await
            .unwrap();
        let mut tasks = Vec::new();
        for i in 0..16 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let hash = format!("hash-{i}");
                store
                    .consume_code("s1", "host", T0 + Duration::minutes(1), &tok(&hash))
                    .await
                    .unwrap()
            }));
        }
        let mut winners = 0;
        for t in tasks {
            if t.await.unwrap().is_some() {
                winners += 1;
            }
        }
        assert_eq!(winners, 1);
        assert_eq!(store.list_tokens().await.unwrap().len(), 1);
    }
);

store_matrix!(
    device_approve_then_poll,
    |store: Arc<dyn Store>| async move {
        let row = device_row("dev", "user", T0);
        let id = row.id;
        store.insert_enrollment(&row).await.unwrap();
        let at = T0 + Duration::minutes(1);

        let t = tok("h1");
        assert_eq!(
            store.poll_device("dev", at, &t).await.unwrap(),
            PollOutcome::Pending
        );
        let found = store
            .device_by_user_code("user", at)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, id);
        assert_eq!(found.requester_ip.as_deref(), Some("203.0.113.9"));

        let approved = store.decide_device(id, true, at).await.unwrap().unwrap();
        assert_eq!(approved.status, EnrollmentStatus::Approved);
        assert_eq!(approved.approved_at, Some(at));
        // Approved rows are no longer offered for a decision.
        assert!(
            store
                .device_by_user_code("user", at)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.decide_device(id, true, at).await.unwrap().is_none());

        let tid = t.id;
        match store.poll_device("dev", at, &t).await.unwrap() {
            PollOutcome::Consumed(e) => {
                assert_eq!(e.status, EnrollmentStatus::Consumed);
                assert_eq!(e.token_id, Some(tid));
            }
            other => panic!("expected Consumed, got {other:?}"),
        }
        let tokens = store.list_tokens().await.unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].label, "laptop (device login)");

        assert_eq!(
            store.poll_device("dev", at, &tok("h2")).await.unwrap(),
            PollOutcome::Expired
        );
        assert_eq!(store.list_tokens().await.unwrap().len(), 1);
    }
);

store_matrix!(device_denied, |store: Arc<dyn Store>| async move {
    let row = device_row("dev", "user", T0);
    let id = row.id;
    store.insert_enrollment(&row).await.unwrap();
    let at = T0 + Duration::minutes(1);
    let denied = store.decide_device(id, false, at).await.unwrap().unwrap();
    assert_eq!(denied.status, EnrollmentStatus::Denied);
    assert_eq!(
        store.poll_device("dev", at, &tok("h1")).await.unwrap(),
        PollOutcome::Denied
    );
    assert!(store.list_tokens().await.unwrap().is_empty());
});

store_matrix!(
    device_expires_after_10_minutes,
    |store: Arc<dyn Store>| async move {
        let row = device_row("dev", "user", T0);
        let id = row.id;
        store.insert_enrollment(&row).await.unwrap();
        store
            .decide_device(id, true, T0 + Duration::minutes(5))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .poll_device("dev", T0 + Duration::minutes(10), &tok("h1"))
                .await
                .unwrap(),
            PollOutcome::Expired
        );
        assert!(store.list_tokens().await.unwrap().is_empty());
        // An unexpired-looking decision is refused too once the time has passed.
        assert!(
            store
                .device_by_user_code("user", T0 + Duration::minutes(10))
                .await
                .unwrap()
                .is_none()
        );
    }
);

store_matrix!(
    device_unknown_secret_is_expired,
    |store: Arc<dyn Store>| async move {
        assert_eq!(
            store.poll_device("nope", T0, &tok("h1")).await.unwrap(),
            PollOutcome::Expired
        );
    }
);

store_matrix!(
    device_consumed_once_under_race,
    |store: Arc<dyn Store>| async move {
        let row = device_row("dev", "user", T0);
        let id = row.id;
        store.insert_enrollment(&row).await.unwrap();
        let at = T0 + Duration::minutes(1);
        store.decide_device(id, true, at).await.unwrap().unwrap();
        let mut tasks = Vec::new();
        for i in 0..16 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let hash = format!("hash-{i}");
                store.poll_device("dev", at, &tok(&hash)).await.unwrap()
            }));
        }
        let mut consumed = 0;
        for t in tasks {
            match t.await.unwrap() {
                PollOutcome::Consumed(_) => consumed += 1,
                PollOutcome::Expired => {}
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        assert_eq!(consumed, 1);
        assert_eq!(store.list_tokens().await.unwrap().len(), 1);
    }
);

store_matrix!(
    cancel_and_list_pending,
    |store: Arc<dyn Store>| async move {
        let a = code_row("a", "first", T0);
        let b = code_row("b", "second", T0 + Duration::minutes(1));
        let (a_id, b_id) = (a.id, b.id);
        store.insert_enrollment(&a).await.unwrap();
        store.insert_enrollment(&b).await.unwrap();
        let now = T0 + Duration::minutes(2);

        let listed = store.list_pending_enrollments(now).await.unwrap();
        assert_eq!(
            listed.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![b_id, a_id],
            "newest first"
        );

        assert!(store.cancel_enrollment(a_id, now).await.unwrap());
        assert!(!store.cancel_enrollment(a_id, now).await.unwrap());
        let listed = store.list_pending_enrollments(now).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, b_id);
        assert_eq!(listed[0].label, "second");
        // A cancelled code can no longer be redeemed.
        assert!(
            store
                .consume_code("a", "h", now, &tok("h1"))
                .await
                .unwrap()
                .is_none()
        );
    }
);

store_matrix!(cleanup_marks_and_deletes_old_rows, |store: Arc<
    dyn Store,
>| async move {
    store
        .insert_enrollment(&code_row("old", "old", T0))
        .await
        .unwrap();
    // Inserting after the first row's expiry (9:15) retires it.
    let later = T0 + Duration::minutes(20);
    store
        .insert_enrollment(&code_row("new", "new", later))
        .await
        .unwrap();
    let pending = store.list_pending_enrollments(later).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].label, "new");
    assert!(
        store
            .consume_code("old", "h", later, &tok("h1"))
            .await
            .unwrap()
            .is_none()
    );

    // Two days on, the old row is deleted: its unique secret hash is free again.
    let much_later = T0 + Duration::days(2);
    store
        .insert_enrollment(&code_row("third", "third", much_later))
        .await
        .unwrap();
    store
        .insert_enrollment(&code_row("old", "reused", much_later))
        .await
        .expect("old row was deleted, so its secret hash is reusable");
});
