#[macro_use]
mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{Billing, SessionId};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::usage::{Repriced, UsageService, parse_since};
use axle::store::sqlite::SqliteStore;
use axle::store::{NewUsage, Priced, Pricer, Store, UsageBatch, UsageCounts};
use axle::usage::prices::PriceTable;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-28 9:00 UTC);
const BUILTIN_REF: &str = "builtin:2026-11-28#claude-opus-5-5";

struct NoPrice;

impl Pricer for NoPrice {
    fn price(&self, _: &str, _: Option<&str>, _: &UsageCounts) -> Option<Priced> {
        None
    }
}

struct Old;

impl Pricer for Old {
    fn price(&self, _: &str, _: Option<&str>, _: &UsageCounts) -> Option<Priced> {
        Some(Priced {
            cost_micros: 1,
            price_ref: "old#x".into(),
        })
    }
}

fn rec(key: &str, at: OffsetDateTime) -> NewUsage {
    NewUsage {
        key: key.into(),
        model: "claude-opus-5-5".into(),
        counts: UsageCounts {
            input: 1000,
            output: 100,
            ..UsageCounts::default()
        },
        speed: None,
        billing: Billing::Subscription,
        occurred_at: at,
    }
}

async fn session(store: &Arc<dyn Store>, clock: &TestClock) -> Resolved {
    IdentityService::resolve(
        &**store,
        clock,
        IdentityInput {
            project_key: Some("github.com/reyemtech/axle".to_string()),
            folder: None,
            host: "mac".into(),
            harness: "claude-code".into(),
            session: SessionId::new(),
            token: common::token(&**store).await,
        },
    )
    .await
    .unwrap()
}

async fn put(
    store: &Arc<dyn Store>,
    clock: &TestClock,
    r: &Resolved,
    rec: NewUsage,
    pricer: &dyn Pricer,
) {
    store
        .insert_usage(
            &UsageBatch {
                session: r.session.id,
                agent: &r.agent,
                token: r.session.token_id,
                parser: "claude-code/1",
                harness_session_id: None,
                records: &[rec],
                now: clock.now(),
            },
            pricer,
        )
        .await
        .unwrap();
}

async fn price_ref(store: &Arc<dyn Store>, key: &str) -> Option<String> {
    store
        .get_usage("claude-code", key)
        .await
        .unwrap()
        .unwrap()
        .price_ref
}

store_matrix!(
    reprice_all_and_unpriced_only,
    |store: Arc<dyn Store>| async move {
        let clock = Arc::new(TestClock::new(T0));
        let r = session(&store, &clock).await;
        let at = |m: i64| T0 - Duration::minutes(m);
        put(&store, &clock, &r, rec("a", at(180)), &NoPrice).await;
        put(&store, &clock, &r, rec("b", at(60)), &NoPrice).await;
        put(&store, &clock, &r, rec("c", at(120)), &Old).await;
        let svc = UsageService::new(
            store.clone(),
            clock.clone(),
            Arc::new(PriceTable::builtin()),
        );

        let done = svc.reprice(None, true).await.unwrap();
        assert_eq!(
            done,
            Repriced {
                updated: 2,
                unpriced: 0
            }
        );
        assert_eq!(price_ref(&store, "c").await.as_deref(), Some("old#x"));
        assert_eq!(price_ref(&store, "a").await.as_deref(), Some(BUILTIN_REF));

        let done = svc.reprice(None, false).await.unwrap();
        assert_eq!(done.updated, 3);
        assert_eq!(price_ref(&store, "c").await.as_deref(), Some(BUILTIN_REF));

        // Only records at or after `since`.
        put(&store, &clock, &r, rec("d", at(30)), &Old).await;
        put(&store, &clock, &r, rec("e", at(150)), &Old).await;
        let done = svc.reprice(Some(at(90)), false).await.unwrap();
        assert_eq!(done.updated, 2); // b and d
        assert_eq!(price_ref(&store, "d").await.as_deref(), Some(BUILTIN_REF));
        assert_eq!(price_ref(&store, "e").await.as_deref(), Some("old#x"));
    }
);

store_matrix!(reprice_counts_models_without_a_price, |store: Arc<
    dyn Store,
>| async move {
    let clock = Arc::new(TestClock::new(T0));
    let r = session(&store, &clock).await;
    let mut odd = rec("x", T0);
    odd.model = "no-such-model".into();
    put(&store, &clock, &r, odd, &Old).await;
    let svc = UsageService::new(
        store.clone(),
        clock.clone(),
        Arc::new(PriceTable::builtin()),
    );
    let done = svc.reprice(None, false).await.unwrap();
    assert_eq!(
        done,
        Repriced {
            updated: 1,
            unpriced: 1
        }
    );
    assert_eq!(price_ref(&store, "x").await, None);
});

#[test]
fn parse_since_forms() {
    assert_eq!(
        parse_since("2026-11-01").unwrap(),
        datetime!(2026-11-01 0:00 UTC)
    );
    assert_eq!(
        parse_since("2026-11-01T10:30:00+01:00").unwrap(),
        datetime!(2026-11-01 9:30 UTC)
    );
    for bad in ["yesterday", "2026-13-01", "2026-11-1", "2026-02-30", ""] {
        assert!(parse_since(bad).is_err(), "{bad:?} should be rejected");
    }
}

#[tokio::test]
async fn cli_reprices_a_sqlite_file() {
    let dir: PathBuf = std::env::temp_dir().join(format!("axl-reprice-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}", dir.join("a.db").display());
    {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::connect(&url).await.unwrap());
        let clock = TestClock::new(T0);
        let r = session(&store, &clock).await;
        put(&store, &clock, &r, rec("a", T0), &NoPrice).await;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_axl"))
        .args(["usage", "reprice", "--unpriced"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DATABASE_URL", &url)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("axl: repriced 1 records (0 without a price)"),
        "{stderr}"
    );
}
