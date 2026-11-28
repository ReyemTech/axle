#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{Billing, Eligibility, SessionId, Task, TaskId};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{Caller, ClaimOutcome, Completion, NewTask, TaskService};
use axle::store::{NewUsage, Priced, Pricer, Store, UsageBatch, UsageCounts, UsageWritten};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-27 9:00 UTC);
const PROJECT: &str = "github.com/reyemtech/axle";

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

    /// A new session of the agent `(PROJECT, host, harness)`.
    async fn session(&self, host: &str, harness: &str) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(PROJECT.to_string()),
                host: host.into(),
                harness: harness.into(),
                session: SessionId::new(),
                token: common::token(&*self.store).await,
            },
        )
        .await
        .unwrap()
    }

    async fn task(&self, by: &Resolved) -> Task {
        self.svc
            .create(
                Caller::Agent(by),
                NewTask {
                    project_id: by.project.as_ref().expect("has a project").id,
                    title: "fix the flaky test".into(),
                    eligibility: Eligibility::default(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    async fn claim(&self, by: &Resolved, task: &Task) {
        match self.svc.claim(by, task.id).await.unwrap() {
            ClaimOutcome::Claimed(_) => {}
            other => panic!("expected Claimed, got {other:?}"),
        }
    }

    fn set_time(&self, offset: Duration) {
        let delta = T0 + offset - self.clock.now();
        self.clock.advance(delta);
    }

    async fn allocations(&self, key: &str) -> Vec<(Option<TaskId>, i64)> {
        self.store
            .get_usage("claude-code", key)
            .await
            .unwrap()
            .expect("usage stored")
            .allocations
    }
}

struct NoPrice;

impl Pricer for NoPrice {
    fn price(&self, _: &str, _: Option<&str>, _: &UsageCounts) -> Option<Priced> {
        None
    }
}

struct PerToken;

impl Pricer for PerToken {
    fn price(&self, _: &str, _: Option<&str>, c: &UsageCounts) -> Option<Priced> {
        Some(Priced {
            cost_micros: c.input + c.output + c.cache_write_5m + c.cache_write_1h + c.cache_read,
            price_ref: "test#per-token".into(),
        })
    }
}

fn rec(key: &str, at: OffsetDateTime, input: i64) -> NewUsage {
    NewUsage {
        key: key.into(),
        model: "claude-opus-5-5".into(),
        counts: UsageCounts {
            input,
            output: 10,
            ..UsageCounts::default()
        },
        speed: None,
        billing: Billing::Subscription,
        occurred_at: at,
    }
}

async fn report(
    env: &Env,
    r: &Resolved,
    records: &[NewUsage],
    pricer: &dyn Pricer,
) -> UsageWritten {
    let batch = UsageBatch {
        session: r.session.id,
        agent: &r.agent,
        token: r.session.token_id,
        parser: "claude-code/1",
        harness_session_id: Some("hs-1"),
        records,
        now: env.clock.now(),
    };
    env.store.insert_usage(&batch, pricer).await.unwrap()
}

/// `tasks` as allocation rows of `parts` each, in the store's order (task id text).
fn split(tasks: &[&Task], parts: i64) -> Vec<(Option<TaskId>, i64)> {
    let mut ids: Vec<TaskId> = tasks.iter().map(|t| t.id).collect();
    ids.sort_by_key(|id| id.to_string());
    ids.into_iter().map(|id| (Some(id), parts)).collect()
}

store_matrix!(
    usage_insert_and_read_back,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let w = report(&env, &a, &[rec("req_1", T0, 12)], &PerToken).await;
        assert_eq!(
            w,
            UsageWritten {
                inserted: 1,
                duplicate: 0
            }
        );
        let u = env
            .store
            .get_usage("claude-code", "req_1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (u.session, u.agent, u.project),
            (a.session.id, a.agent.id, a.agent.project_id)
        );
        assert_eq!(u.harness, "claude-code");
        assert_eq!(u.model, "claude-opus-5-5");
        assert_eq!(
            u.counts,
            UsageCounts {
                input: 12,
                output: 10,
                ..UsageCounts::default()
            }
        );
        assert_eq!(u.speed, None);
        assert_eq!(u.billing, Billing::Subscription);
        assert_eq!(u.cost_micros, Some(22));
        assert_eq!(u.price_ref.as_deref(), Some("test#per-token"));
        assert_eq!(u.parser, "claude-code/1");
        assert_eq!(u.harness_session_id.as_deref(), Some("hs-1"));
        assert_eq!((u.occurred_at, u.received_at), (T0, T0));
        assert_eq!(u.allocations, vec![(None, 1)]);
        assert!(
            env.store
                .get_usage("codex", "req_1")
                .await
                .unwrap()
                .is_none(),
            "keyed by harness"
        );

        // No price is stored as NULL.
        report(&env, &a, &[rec("req_2", T0, 12)], &NoPrice).await;
        let u = env
            .store
            .get_usage("claude-code", "req_2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((u.cost_micros, u.price_ref), (None, None));
    }
);

store_matrix!(
    usage_duplicate_is_noop,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        report(&env, &a, &[rec("req_1", T0, 12)], &PerToken).await;
        let before = env
            .store
            .get_usage("claude-code", "req_1")
            .await
            .unwrap()
            .unwrap();

        env.set_time(Duration::minutes(3));
        let w = report(&env, &a, &[rec("req_1", T0, 12)], &PerToken).await;
        assert_eq!(
            w,
            UsageWritten {
                inserted: 0,
                duplicate: 1
            }
        );
        let after = env
            .store
            .get_usage("claude-code", "req_1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after, before);
    }
);

store_matrix!(
    usage_merges_upward_and_reprices,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let b = env.session("linux", "claude-code").await;
        report(&env, &a, &[rec("req_1", T0, 12)], &PerToken).await;

        env.set_time(Duration::minutes(1));
        let mut second = rec("req_1", T0 + Duration::minutes(1), 5);
        second.counts.output = 900;
        second.counts.cache_read = 7;
        second.model = "claude-other".into();
        second.billing = Billing::Api;
        let w = report(&env, &b, &[second], &PerToken).await;
        assert_eq!(
            w,
            UsageWritten {
                inserted: 0,
                duplicate: 1
            }
        );

        let u = env
            .store
            .get_usage("claude-code", "req_1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            u.counts,
            UsageCounts {
                input: 12,
                output: 900,
                cache_read: 7,
                ..UsageCounts::default()
            }
        );
        assert_eq!((u.session, u.agent), (a.session.id, a.agent.id));
        assert_eq!(u.model, "claude-opus-5-5");
        assert_eq!(u.billing, Billing::Subscription);
        assert_eq!((u.occurred_at, u.received_at), (T0, T0));
        assert_eq!(u.cost_micros, Some(12 + 900 + 7));
        assert_eq!(u.allocations, vec![(None, 1)]);
    }
);

store_matrix!(usage_allocation_zero_one_three_tasks, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let t1 = env.task(&a).await;
    let t2 = env.task(&a).await;
    let t3 = env.task(&a).await;

    report(&env, &a, &[rec("r0", T0, 1)], &NoPrice).await;
    assert_eq!(env.allocations("r0").await, vec![(None, 1)]);

    env.set_time(Duration::minutes(1));
    env.claim(&a, &t1).await;
    env.set_time(Duration::minutes(2));
    report(
        &env,
        &a,
        &[rec("r1", T0 + Duration::minutes(2), 1)],
        &NoPrice,
    )
    .await;
    assert_eq!(env.allocations("r1").await, split(&[&t1], 1));

    env.set_time(Duration::minutes(3));
    env.claim(&a, &t2).await;
    env.claim(&a, &t3).await;
    env.set_time(Duration::minutes(4));
    report(
        &env,
        &a,
        &[rec("r3", T0 + Duration::minutes(4), 1)],
        &NoPrice,
    )
    .await;
    assert_eq!(env.allocations("r3").await, split(&[&t1, &t2, &t3], 3));

    // Another session's usage is never allocated to a's tasks.
    let b = env.session("linux", "claude-code").await;
    report(
        &env,
        &b,
        &[rec("rb", T0 + Duration::minutes(4), 1)],
        &NoPrice,
    )
    .await;
    assert_eq!(env.allocations("rb").await, vec![(None, 1)]);
});

store_matrix!(
    usage_holdings_at_occurred_at,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let t1 = env.task(&a).await;
        let t2 = env.task(&a).await;
        let t3 = env.task(&a).await;
        let at = |m: i64| T0 + Duration::minutes(m);

        env.set_time(Duration::minutes(1));
        env.claim(&a, &t1).await;
        env.set_time(Duration::minutes(5));
        env.svc
            .complete(&a, t1.id, Completion::default())
            .await
            .unwrap();
        env.set_time(Duration::minutes(6));
        report(&env, &a, &[rec("finishing", at(4), 1)], &NoPrice).await;
        assert_eq!(
            env.allocations("finishing").await,
            split(&[&t1], 1),
            "the turn that finishes a task"
        );
        report(&env, &a, &[rec("after_done", at(6), 1)], &NoPrice).await;
        assert_eq!(env.allocations("after_done").await, vec![(None, 1)]);

        env.set_time(Duration::minutes(7));
        env.claim(&a, &t2).await;
        env.set_time(Duration::minutes(8));
        env.svc.release(&a, t2.id, None).await.unwrap();
        let mid = at(7) + Duration::seconds(30);
        report(&env, &a, &[rec("while_held", mid, 1)], &NoPrice).await;
        assert_eq!(env.allocations("while_held").await, split(&[&t2], 1));
        report(&env, &a, &[rec("at_release", at(8), 1)], &NoPrice).await;
        assert_eq!(env.allocations("at_release").await, vec![(None, 1)]);

        env.set_time(Duration::minutes(9));
        env.claim(&a, &t3).await;
        env.set_time(Duration::minutes(10));
        env.svc.block(&a, t3.id, "waiting on CI").await.unwrap();
        env.set_time(Duration::minutes(11));
        report(&env, &a, &[rec("blocked", at(11), 1)], &NoPrice).await;
        assert_eq!(
            env.allocations("blocked").await,
            split(&[&t3], 1),
            "a blocked task is still held"
        );
    }
);

store_matrix!(
    usage_holdings_end_on_takeover,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let b = env.session("linux", "claude-code").await;
        let t = env.task(&a).await;
        let at = |m: i64| T0 + Duration::minutes(m);

        env.claim(&a, &t).await;
        env.set_time(Duration::minutes(20));
        env.claim(&b, &t).await;

        report(&env, &a, &[rec("held", at(10), 1)], &NoPrice).await;
        assert_eq!(env.allocations("held").await, split(&[&t], 1));
        report(&env, &a, &[rec("taken", at(21), 1)], &NoPrice).await;
        assert_eq!(env.allocations("taken").await, vec![(None, 1)]);
        report(&env, &a, &[rec("expired", at(16), 1)], &NoPrice).await;
        assert_eq!(
            env.allocations("expired").await,
            split(&[&t], 1),
            "an expired lease still counts until someone takes the task"
        );
    }
);
