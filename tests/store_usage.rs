#[macro_use]
mod common;

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::domain::{Billing, Eligibility, SessionId, Task, TaskId, TokenId};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{Caller, ClaimOutcome, Completion, NewTask, TaskService};
use axle::store::{
    NewUsage, Priced, Pricer, Store, TaskScope, UsageBatch, UsageCounts, UsageFilter, UsageGroup,
    UsageRow, UsageWritten,
};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-11-27 9:00 UTC);
const PROJECT: &str = "github.com/reyemtech/axle";
const OTHER: &str = "github.com/reyemtech/other";

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
        self.session_in(PROJECT, host, harness).await
    }

    async fn session_in(&self, project: &str, host: &str, harness: &str) -> Resolved {
        let token = common::token(&*self.store).await;
        self.session_with(project, host, harness, token).await
    }

    /// A new session of the agent `(project, host, harness)` under an existing `token`.
    async fn session_with(
        &self,
        project: &str,
        host: &str,
        harness: &str,
        token: TokenId,
    ) -> Resolved {
        IdentityService::resolve(
            &*self.store,
            &*self.clock,
            IdentityInput {
                project_key: Some(project.to_string()),
                folder: None,
                host: host.into(),
                harness: harness.into(),
                session: SessionId::new(),
                token,
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

    async fn subtask(&self, by: &Resolved, parent: &Task) -> Task {
        self.svc
            .create(
                Caller::Agent(by),
                NewTask {
                    project_id: parent.project_id,
                    parent_id: Some(parent.id),
                    title: "a part of it".into(),
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
        let b = env
            .session_with(PROJECT, "linux", "claude-code", a.session.token_id)
            .await;
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

store_matrix!(usage_duplicate_from_another_token_is_noop, |store: Arc<
    dyn Store,
>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let other = env.session("linux", "claude-code").await;
    assert_ne!(a.session.token_id, other.session.token_id);
    report(&env, &a, &[rec("req_1", T0, 12)], &PerToken).await;

    env.set_time(Duration::minutes(1));
    let mut bigger = rec("req_1", T0 + Duration::minutes(1), 500);
    bigger.counts.output = 900;
    let w = report(&env, &other, &[bigger], &PerToken).await;
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
            output: 10,
            ..UsageCounts::default()
        },
        "another token's counts are not merged"
    );
    assert_eq!(u.session, a.session.id);
    assert_eq!(u.cost_micros, Some(12 + 10));
});

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

store_matrix!(
    usage_takeover_opens_holding,
    |store: Arc<dyn Store>| async move {
        let env = Env::new(store);
        let a = env.session("mac", "claude-code").await;
        let b = env.session("linux", "claude-code").await;
        let t = env.task(&a).await;
        let at = |m: i64| T0 + Duration::minutes(m);

        env.claim(&a, &t).await;
        env.set_time(Duration::minutes(20));
        // Another agent takes the expired lease: a `lease_expired_takeover`, not `claimed`.
        env.claim(&b, &t).await;

        report(&env, &b, &[rec("b-after", at(21), 1)], &NoPrice).await;
        assert_eq!(
            env.allocations("b-after").await,
            split(&[&t], 1),
            "a lease takeover opens the new owner's holding"
        );
        report(&env, &a, &[rec("a-after", at(21), 1)], &NoPrice).await;
        assert_eq!(env.allocations("a-after").await, vec![(None, 1)]);
        report(&env, &a, &[rec("a-before", at(10), 1)], &NoPrice).await;
        assert_eq!(env.allocations("a-before").await, split(&[&t], 1));
        report(&env, &b, &[rec("b-before", at(10), 1)], &NoPrice).await;
        assert_eq!(env.allocations("b-before").await, vec![(None, 1)]);
    }
);

/// Two agents (mac/claude-code in PROJECT, linux/codex in OTHER). The first holds a child
/// task and a loose task together at minute 2, so that record is split in two.
struct Seeded {
    env: Env,
    a: Resolved,
    b: Resolved,
    parent: Task,
    child: Task,
    loose: Task,
}

const DAY: Duration = Duration::days(1);

async fn seeded(store: Arc<dyn Store>) -> Seeded {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let b = env.session_in(OTHER, "linux", "codex").await;
    let parent = env.task(&a).await;
    let child = env.subtask(&a, &parent).await;
    let loose = env.task(&a).await;
    let at = |d: Duration| T0 + d;

    // r1: no task.
    report(&env, &a, &[rec("r1", at(Duration::ZERO), 100)], &PerToken).await;
    env.set_time(Duration::minutes(1));
    env.claim(&a, &child).await;
    env.claim(&a, &loose).await;
    // r2: held child and loose, so two shares.
    env.set_time(Duration::minutes(2));
    report(
        &env,
        &a,
        &[rec("r2", at(Duration::minutes(2)), 200)],
        &PerToken,
    )
    .await;
    env.set_time(Duration::minutes(10));
    for t in [&child, &loose] {
        env.svc
            .complete(&a, t.id, Completion::default())
            .await
            .unwrap();
    }
    // r4: the other agent, API billed, the codex harness.
    let mut r4 = rec("r4", at(Duration::minutes(30)), 400);
    r4.model = "gpt-5".into();
    r4.billing = Billing::Api;
    report(&env, &b, &[r4], &PerToken).await;
    // r5: the next day, not priced, another model.
    env.set_time(DAY);
    let mut r5 = rec("r5", at(DAY), 800);
    r5.model = "claude-other".into();
    report(&env, &a, &[r5], &NoPrice).await;
    Seeded {
        env,
        a,
        b,
        parent,
        child,
        loose,
    }
}

async fn one(s: &Seeded, filter: UsageFilter) -> UsageRow {
    let rows = s
        .env
        .store
        .usage_summary(&filter, UsageGroup::Total)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "Total is always one row");
    rows.into_iter().next().unwrap()
}

fn row(rows: &[UsageRow], key: &str) -> UsageRow {
    rows.iter()
        .find(|r| r.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no row {key}: {rows:?}"))
        .clone()
}

store_matrix!(
    usage_every_dimension_adds_up,
    |store: Arc<dyn Store>| async move {
        let s = seeded(store).await;
        let all = UsageFilter::default();
        let by = |g| s.env.store.usage_summary(&all, g);
        let total = one(&s, all.clone()).await;
        assert_eq!(total.key, None);
        assert_eq!((total.requests, total.sessions), (4, 2));
        assert_eq!((total.unpriced, total.estimated), (1, 3));
        assert!((total.input - 1500.0).abs() < 1e-6);
        assert!((total.output - 40.0).abs() < 1e-6);
        // 110 + 210 + 410; the fourth has no price.
        assert!((total.cost_micros - 730.0).abs() < 1e-6);
        assert!((total.estimated_cost_micros - 320.0).abs() < 1e-6);

        for g in [
            UsageGroup::Project,
            UsageGroup::Task,
            UsageGroup::Agent,
            UsageGroup::Harness,
            UsageGroup::Host,
            UsageGroup::Model,
            UsageGroup::Day,
            UsageGroup::Hour,
        ] {
            let rows = by(g).await.unwrap();
            let sum = |f: fn(&UsageRow) -> f64| rows.iter().map(f).sum::<f64>();
            assert!((sum(|r| r.input) - total.input).abs() < 1e-6, "{g:?} input");
            assert!(
                (sum(|r| r.output) - total.output).abs() < 1e-6,
                "{g:?} output"
            );
            assert!(
                (sum(|r| r.cost_micros) - total.cost_micros).abs() < 1e-6,
                "{g:?} cost"
            );
            assert!(
                (sum(|r| r.estimated_cost_micros) - total.estimated_cost_micros).abs() < 1e-6,
                "{g:?} estimated cost"
            );
        }

        let days = by(UsageGroup::Day).await.unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(row(&days, "2026-11-27").requests, 3);
        assert_eq!(row(&days, "2026-11-28").requests, 1);
        let hours = by(UsageGroup::Hour).await.unwrap();
        assert_eq!(hours.len(), 2);
        assert_eq!(row(&hours, "2026-11-27T09").requests, 3);
        assert_eq!(row(&hours, "2026-11-28T09").requests, 1);

        let tasks = by(UsageGroup::Task).await.unwrap();
        assert_eq!(tasks.len(), 3, "no task, child and loose: {tasks:?}");
        assert_eq!(tasks.iter().find(|r| r.key.is_none()).unwrap().requests, 3);
        let child = row(&tasks, &s.child.id.to_string());
        assert_eq!(child.requests, 1);
        assert!(
            (child.input - 100.0).abs() < 1e-6,
            "half of the split record"
        );
        assert!((child.cost_micros - 105.0).abs() < 1e-6);

        let projects = by(UsageGroup::Project).await.unwrap();
        assert_eq!(projects.len(), 2);
        let home = s.a.agent.project_id.unwrap().to_string();
        assert_eq!(row(&projects, &home).requests, 3);
        let agents = by(UsageGroup::Agent).await.unwrap();
        assert_eq!(row(&agents, &s.b.agent.id.to_string()).requests, 1);
        assert_eq!(
            row(&by(UsageGroup::Model).await.unwrap(), "gpt-5").requests,
            1
        );
        assert_eq!(row(&by(UsageGroup::Host).await.unwrap(), "mac").requests, 3);
        assert_eq!(
            row(&by(UsageGroup::Harness).await.unwrap(), "codex").requests,
            1
        );
    }
);

store_matrix!(usage_filters, |store: Arc<dyn Store>| async move {
    let s = seeded(store).await;
    let f = UsageFilter::default;

    let r = one(
        &s,
        UsageFilter {
            harness: Some("codex".into()),
            ..f()
        },
    )
    .await;
    assert_eq!((r.requests, r.sessions), (1, 1));
    assert!((r.input - 400.0).abs() < 1e-6);
    let requests = |filter: UsageFilter| {
        let s = &s;
        async move { one(s, filter).await.requests }
    };
    assert_eq!(
        requests(UsageFilter {
            host: Some("mac".into()),
            ..f()
        })
        .await,
        3
    );
    assert_eq!(
        requests(UsageFilter {
            agent: Some(s.b.agent.id),
            ..f()
        })
        .await,
        1
    );
    assert_eq!(
        requests(UsageFilter {
            project: s.b.agent.project_id,
            ..f()
        })
        .await,
        1
    );
    assert_eq!(
        requests(UsageFilter {
            model: Some("claude-other".into()),
            ..f()
        })
        .await,
        1
    );
    let api = one(
        &s,
        UsageFilter {
            billing: Some(Billing::Api),
            ..f()
        },
    )
    .await;
    assert_eq!((api.requests, api.estimated), (1, 0));

    // `from` is inclusive, `to` is exclusive.
    let mid = T0 + Duration::minutes(30);
    assert_eq!(
        requests(UsageFilter {
            from: Some(mid),
            ..f()
        })
        .await,
        2
    );
    assert_eq!(
        requests(UsageFilter {
            to: Some(mid),
            ..f()
        })
        .await,
        2
    );
    let none = one(
        &s,
        UsageFilter {
            from: Some(T0 + DAY * 2),
            ..f()
        },
    )
    .await;
    assert_eq!(
        (none.requests, none.sessions, none.input, none.cost_micros),
        (0, 0, 0.0, 0.0)
    );

    // No task: the three records that held none, whole.
    let no_task = one(
        &s,
        UsageFilter {
            task: Some(TaskScope::NoTask),
            ..f()
        },
    )
    .await;
    assert_eq!(no_task.requests, 3);
    assert!((no_task.input - 1300.0).abs() < 1e-6);

    // Tasks: the split record counts as one request, half of its tokens.
    let scoped = |id, subtasks| UsageFilter {
        task: Some(TaskScope::Task { id, subtasks }),
        ..f()
    };
    let child = one(&s, scoped(s.child.id, false)).await;
    assert_eq!(child.requests, 1);
    assert!((child.input - 100.0).abs() < 1e-6);
    assert_eq!(requests(scoped(s.parent.id, false)).await, 0);
    let tree = one(&s, scoped(s.parent.id, true)).await;
    assert_eq!(tree.requests, 1);
    assert!((tree.input - 100.0).abs() < 1e-6, "the child's share");
    let loose = one(&s, scoped(s.loose.id, true)).await;
    assert!((loose.input - 100.0).abs() < 1e-6);

    // Filters combine.
    assert_eq!(
        requests(UsageFilter {
            harness: Some("claude-code".into()),
            from: Some(mid),
            ..f()
        })
        .await,
        1
    );
});

store_matrix!(usage_reprice_pages, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let later = T0 + Duration::hours(1);
    report(&env, &a, &[rec("u1", T0, 1)], &PerToken).await;
    report(
        &env,
        &a,
        &[rec("u2", T0 + Duration::minutes(1), 2)],
        &NoPrice,
    )
    .await;
    report(&env, &a, &[rec("u3", later, 3)], &PerToken).await;
    let mut ids = Vec::new();
    for k in ["u1", "u2", "u3"] {
        let u = env.store.get_usage("claude-code", k).await.unwrap();
        ids.push(u.unwrap().id);
    }
    let mut sorted = ids.clone();
    sorted.sort_by_key(|id| id.to_string());

    let unpriced = env
        .store
        .usage_to_reprice(None, true, None, 10)
        .await
        .unwrap();
    assert_eq!(unpriced.len(), 1);
    assert_eq!(unpriced[0].id, ids[1]);
    assert_eq!(unpriced[0].model, "claude-opus-5-5");
    assert_eq!(unpriced[0].speed, None);
    assert_eq!(unpriced[0].counts.input, 2);

    let page1 = env
        .store
        .usage_to_reprice(None, false, None, 2)
        .await
        .unwrap();
    assert_eq!(page1.iter().map(|r| r.id).collect::<Vec<_>>(), sorted[..2]);
    let page2 = env
        .store
        .usage_to_reprice(None, false, Some(sorted[1]), 2)
        .await
        .unwrap();
    assert_eq!(page2.iter().map(|r| r.id).collect::<Vec<_>>(), sorted[2..]);

    let since = env
        .store
        .usage_to_reprice(Some(later), false, None, 10)
        .await
        .unwrap();
    assert_eq!(since.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[2]]);

    let set = Priced {
        cost_micros: 77,
        price_ref: "test#new".into(),
    };
    env.store
        .set_usage_costs(&[(ids[1], Some(set)), (ids[0], None)])
        .await
        .unwrap();
    let get = |k: &'static str| {
        let store = env.store.clone();
        async move { store.get_usage("claude-code", k).await.unwrap().unwrap() }
    };
    let (u1, u2, u3) = (get("u1").await, get("u2").await, get("u3").await);
    assert_eq!((u1.cost_micros, u1.price_ref), (None, None));
    assert_eq!(
        (u2.cost_micros, u2.price_ref.as_deref()),
        (Some(77), Some("test#new"))
    );
    assert_eq!(u3.cost_micros, Some(13), "untouched");
});
