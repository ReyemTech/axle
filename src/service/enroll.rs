//! Host enrollment (onboarding spec §2): one-time codes that `axl enroll` exchanges for a
//! token, and the device flow where a person approves a short user code in the UI.
//!
//! Secrets (the one-time code, the device code, the user code) are stored only as SHA-256
//! hashes; the plaintext leaves this service once, in the reply that creates it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use time::{Duration, OffsetDateTime};

use crate::bridge::sanitize_label;
use crate::clock::Clock;
use crate::domain::{Enrollment, EnrollmentId, EnrollmentKind, EnrollmentStatus};
use crate::events::{Event, EventBus};
use crate::service::auth::{self, AuthError};
use crate::store::{NewEnrollment, NewToken, PollOutcome, Store, StoreError};

/// How long a one-time code can be exchanged.
pub const CODE_TTL: Duration = Duration::minutes(15);
/// How long a device login can be approved and collected.
pub const DEVICE_TTL: Duration = Duration::minutes(10);
/// Seconds between device polls. 7 s keeps polling under the 10/min per-IP bucket (refill
/// 1 per 6 s) for the whole 10-minute flow.
pub const POLL_INTERVAL_SECS: u64 = 7;
/// Crockford base32: no `I`, `L`, `O`, `U`.
pub const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

const ONE_TIME_LEN: usize = 10;
const USER_CODE_LEN: usize = 8;
const DEVICE_CODE_PREFIX: &str = "axld_";
const DEVICE_CODE_BYTES: usize = 32;
/// Fresh codes tried when an insert collides with an existing hash.
const ISSUE_ATTEMPTS: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    #[error("no system randomness available: {0}")]
    Random(String),
    #[error("could not issue a unique code")]
    Exhausted,
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// `len` random Crockford characters. `b & 31` is unbiased because 256 is a multiple of 32.
#[cfg(test)]
fn random_code(len: usize) -> String {
    try_random_code(len).expect("system randomness")
}

fn try_random_code(len: usize) -> Result<String, EnrollError> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|e| EnrollError::Random(e.to_string()))?;
    Ok(bytes
        .iter()
        .map(|b| CROCKFORD[(b & 31) as usize] as char)
        .collect())
}

/// `raw` as typed by a person, folded to canonical Crockford: upper case, no dashes or
/// whitespace, `I`/`L` read as `1`, `O` as `0`. `None` unless the result is exactly `len`
/// characters of the alphabet.
fn normalize_len(raw: &str, len: usize) -> Option<String> {
    let code: String = raw
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            c => c,
        })
        .collect();
    (code.len() == len && code.bytes().all(|b| CROCKFORD.contains(&b))).then_some(code)
}

/// A one-time code (10 characters) or a user code (8) in canonical form.
pub fn normalize_code(input: &str) -> Option<String> {
    normalize_len(input, ONE_TIME_LEN).or_else(|| normalize_len(input, USER_CODE_LEN))
}

/// Display form: 10 characters as `XXXXX-XXXXX`, 8 as `XXXX-XXXX`.
pub fn format_code(raw: &str) -> String {
    let (a, b) = raw.split_at(raw.len() / 2);
    format!("{a}-{b}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCode {
    pub id: EnrollmentId,
    /// Formatted `XXXXX-XXXXX`; shown once.
    pub code: String,
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStart {
    /// `axld_` + 64 hex; the poller's secret.
    pub device_code: String,
    /// Formatted `XXXX-XXXX`; what the person types in the UI.
    pub user_code: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollReply {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Token { token: String, host: String },
}

pub struct EnrollService {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    bus: EventBus,
    /// Last accepted poll per device-code hash (one server instance, so memory is enough).
    last_poll: Mutex<HashMap<String, OffsetDateTime>>,
}

impl EnrollService {
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>, bus: EventBus) -> Self {
        Self {
            store,
            clock,
            bus,
            last_poll: Mutex::new(HashMap::new()),
        }
    }

    async fn changed(&self, id: EnrollmentId, status: EnrollmentStatus) {
        self.bus.emit(Event::EnrollmentChanged { id, status }).await;
    }

    /// Creates a one-time code for a host to be named `label`.
    pub async fn issue_code(&self, label: &str) -> Result<IssuedCode, EnrollError> {
        let label = label.trim();
        let now = self.clock.now();
        let expires_at = now + CODE_TTL;
        for _ in 0..ISSUE_ATTEMPTS {
            let raw = try_random_code(ONE_TIME_LEN)?;
            let hash = auth::hash(&raw);
            let id = EnrollmentId::new();
            let new = NewEnrollment {
                id,
                kind: EnrollmentKind::Code,
                secret_hash: &hash,
                user_code_hash: None,
                label,
                host: "",
                requester_ip: None,
                now,
                expires_at,
            };
            match self.store.insert_enrollment(&new).await {
                Ok(()) => {
                    self.changed(id, EnrollmentStatus::Pending).await;
                    return Ok(IssuedCode {
                        id,
                        code: format_code(&raw),
                        expires_at,
                    });
                }
                Err(e) if is_collision(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(EnrollError::Exhausted)
    }

    /// Redeems a one-time code; the token's plaintext, or `None` for a malformed, unknown,
    /// used or expired code (indistinguishable on purpose).
    pub async fn exchange(
        &self,
        code: &str,
        host: &str,
        _ip: Option<&str>,
    ) -> Result<Option<String>, EnrollError> {
        let Some(raw) = normalize_len(code, ONE_TIME_LEN) else {
            return Ok(None);
        };
        let (token, token_hash) = auth::mint()?;
        let new_token = NewToken {
            id: crate::domain::TokenId::new(),
            hash: &token_hash,
        };
        let consumed = self
            .store
            .consume_code(
                &auth::hash(&raw),
                &sanitize_label(host),
                self.clock.now(),
                &new_token,
            )
            .await?;
        Ok(match consumed {
            Some(e) => {
                self.changed(e.id, EnrollmentStatus::Consumed).await;
                Some(token)
            }
            None => None,
        })
    }

    /// Starts a device login for `host`: the poller keeps `device_code`, the person types
    /// `user_code` in the UI.
    pub async fn start_device(
        &self,
        host: &str,
        ip: Option<&str>,
    ) -> Result<DeviceStart, EnrollError> {
        let host = sanitize_label(host);
        let now = self.clock.now();
        let expires_at = now + DEVICE_TTL;
        for _ in 0..ISSUE_ATTEMPTS {
            let mut secret = [0u8; DEVICE_CODE_BYTES];
            getrandom::fill(&mut secret).map_err(|e| EnrollError::Random(e.to_string()))?;
            let device_code = secret
                .iter()
                .fold(String::from(DEVICE_CODE_PREFIX), |mut out, b| {
                    let _ = write!(out, "{b:02x}");
                    out
                });
            let user_raw = try_random_code(USER_CODE_LEN)?;
            let secret_hash = auth::hash(&device_code);
            let user_hash = auth::hash(&user_raw);
            let id = EnrollmentId::new();
            let new = NewEnrollment {
                id,
                kind: EnrollmentKind::Device,
                secret_hash: &secret_hash,
                user_code_hash: Some(&user_hash),
                label: &host,
                host: &host,
                requester_ip: ip,
                now,
                expires_at,
            };
            match self.store.insert_enrollment(&new).await {
                Ok(()) => {
                    self.changed(id, EnrollmentStatus::Pending).await;
                    return Ok(DeviceStart {
                        device_code,
                        user_code: format_code(&user_raw),
                        interval: POLL_INTERVAL_SECS,
                        expires_in: DEVICE_TTL.whole_seconds() as u64,
                    });
                }
                Err(e) if is_collision(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(EnrollError::Exhausted)
    }

    /// One poll of the device flow. Polling again sooner than `POLL_INTERVAL_SECS` after the
    /// last accepted poll answers `SlowDown` without touching the store.
    pub async fn poll(&self, device_code: &str) -> Result<PollReply, EnrollError> {
        if !device_code.starts_with(DEVICE_CODE_PREFIX) {
            return Ok(PollReply::Expired);
        }
        let hash = auth::hash(device_code);
        let now = self.clock.now();
        {
            let mut last = self.last_poll.lock().expect("poll map");
            last.retain(|_, t| now - *t < DEVICE_TTL);
            if let Some(t) = last.get(&hash)
                && now - *t < Duration::seconds(POLL_INTERVAL_SECS as i64)
            {
                return Ok(PollReply::SlowDown);
            }
        }
        let (token, token_hash) = auth::mint()?;
        let new_token = NewToken {
            id: crate::domain::TokenId::new(),
            hash: &token_hash,
        };
        let outcome = self.store.poll_device(&hash, now, &new_token).await?;
        {
            let mut last = self.last_poll.lock().expect("poll map");
            match outcome {
                PollOutcome::Pending => {
                    last.insert(hash, now);
                }
                _ => {
                    last.remove(&hash);
                }
            }
        }
        Ok(match outcome {
            PollOutcome::Pending => PollReply::Pending,
            PollOutcome::Denied => PollReply::Denied,
            PollOutcome::Expired => PollReply::Expired,
            PollOutcome::Consumed(e) => {
                self.changed(e.id, EnrollmentStatus::Consumed).await;
                PollReply::Token {
                    token,
                    host: e.host,
                }
            }
        })
    }

    /// The pending device login whose user code the person typed, if any.
    pub async fn lookup_user_code(
        &self,
        user_code: &str,
    ) -> Result<Option<Enrollment>, EnrollError> {
        let Some(raw) = normalize_len(user_code, USER_CODE_LEN) else {
            return Ok(None);
        };
        Ok(self
            .store
            .device_by_user_code(&auth::hash(&raw), self.clock.now())
            .await?)
    }

    /// Approves or denies a pending device login; `false` if it is no longer pending.
    pub async fn decide(&self, id: EnrollmentId, approve: bool) -> Result<bool, EnrollError> {
        let decided = self
            .store
            .decide_device(id, approve, self.clock.now())
            .await?;
        Ok(match decided {
            Some(e) => {
                self.changed(e.id, e.status).await;
                true
            }
            None => false,
        })
    }

    /// Revokes a pending enrollment (code or device); `false` if it was not pending.
    pub async fn cancel(&self, id: EnrollmentId) -> Result<bool, EnrollError> {
        let cancelled = self.store.cancel_enrollment(id, self.clock.now()).await?;
        if cancelled {
            self.changed(id, EnrollmentStatus::Denied).await;
        }
        Ok(cancelled)
    }

    pub async fn pending(&self) -> Result<Vec<Enrollment>, EnrollError> {
        Ok(self
            .store
            .list_pending_enrollments(self.clock.now())
            .await?)
    }
}

/// An insert that lost to a UNIQUE constraint on a hash: retry with a fresh code.
fn is_collision(e: &StoreError) -> bool {
    matches!(e, StoreError::Database(sqlx::Error::Database(d)) if d.is_unique_violation())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::service::auth::AuthService;
    use crate::store::sqlite::SqliteStore;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use time::macros::datetime;

    struct Fixture {
        svc: EnrollService,
        auth: AuthService,
        clock: Arc<TestClock>,
        bus: EventBus,
    }

    async fn fixture() -> Fixture {
        let opts: SqliteConnectOptions = "sqlite::memory:".parse().unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts.foreign_keys(true))
            .await
            .unwrap();
        let store: Arc<dyn Store> = Arc::new(SqliteStore::from_pool(pool).await.unwrap());
        let clock = Arc::new(TestClock::new(datetime!(2026-11-27 9:00 UTC)));
        let bus = EventBus::new(store.clone(), clock.clone());
        Fixture {
            svc: EnrollService::new(store.clone(), clock.clone(), bus.clone()),
            auth: AuthService::new(store, clock.clone()),
            clock,
            bus,
        }
    }

    #[test]
    fn codes_are_crockford_and_shaped() {
        let raw = random_code(10);
        assert_eq!(raw.len(), 10);
        assert!(raw.bytes().all(|b| CROCKFORD.contains(&b)));
        assert_eq!(format_code("K7QM4ZP2RX"), "K7QM4-ZP2RX");
        assert_eq!(format_code("WXYZ1234"), "WXYZ-1234");
    }

    #[test]
    fn normalize_accepts_typing_variants() {
        assert_eq!(normalize_code("k7qm4-zp2rx").as_deref(), Some("K7QM4ZP2RX"));
        assert_eq!(
            normalize_code(" K7QM4 ZP2RX ").as_deref(),
            Some("K7QM4ZP2RX")
        );
        assert!(normalize_code("IOL").is_none());
        assert_eq!(normalize_code("10LO1-ABCDE").as_deref(), Some("10101ABCDE"));
        assert!(normalize_code("K7QM4-ZP2RU").is_none()); // U is not Crockford
        assert_eq!(normalize_code("wxyz-1234").as_deref(), Some("WXYZ1234"));
    }

    #[tokio::test]
    async fn exchange_mints_labelled_token_and_emits() {
        let f = fixture().await;
        let issued = f.svc.issue_code("work laptop").await.unwrap();
        let mut rx = f.bus.subscribe();

        let token = f
            .svc
            .exchange(&issued.code, "laptop", None)
            .await
            .unwrap()
            .expect("a token");
        assert!(token.starts_with("axl_"));
        let labels: Vec<String> = f
            .auth
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.label)
            .collect();
        assert_eq!(labels, vec!["work laptop (laptop)".to_string()]);
        assert_eq!(
            rx.try_recv().unwrap(),
            Event::EnrollmentChanged {
                id: issued.id,
                status: EnrollmentStatus::Consumed
            }
        );
        // Single use.
        assert!(
            f.svc
                .exchange(&issued.code, "laptop", None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn poll_too_fast_is_slow_down() {
        let f = fixture().await;
        let start = f.svc.start_device("mac", None).await.unwrap();
        assert_eq!(start.interval, 7);
        assert!(start.device_code.starts_with("axld_"));
        assert_eq!(start.device_code.len(), 5 + 64);

        assert_eq!(
            f.svc.poll(&start.device_code).await.unwrap(),
            PollReply::Pending
        );
        assert_eq!(
            f.svc.poll(&start.device_code).await.unwrap(),
            PollReply::SlowDown
        );
        f.clock.advance(Duration::seconds(7));
        assert_eq!(
            f.svc.poll(&start.device_code).await.unwrap(),
            PollReply::Pending
        );
    }

    #[tokio::test]
    async fn device_flow_approve_then_token() {
        let f = fixture().await;
        let start = f.svc.start_device("mac", Some("10.0.0.1")).await.unwrap();
        let found = f
            .svc
            .lookup_user_code(&start.user_code.to_lowercase())
            .await
            .unwrap()
            .expect("pending login");
        assert!(f.svc.decide(found.id, true).await.unwrap());
        assert!(!f.svc.decide(found.id, true).await.unwrap());
        f.clock.advance(Duration::seconds(7));
        match f.svc.poll(&start.device_code).await.unwrap() {
            PollReply::Token { token, host } => {
                assert!(token.starts_with("axl_"));
                assert_eq!(host, "mac");
            }
            other => panic!("expected a token, got {other:?}"),
        }
        let labels: Vec<String> = f
            .auth
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.label)
            .collect();
        assert_eq!(labels, vec!["mac (device login)".to_string()]);
        // Terminal: the entry is gone and the row is consumed.
        assert_eq!(
            f.svc.poll(&start.device_code).await.unwrap(),
            PollReply::Expired
        );
    }

    #[tokio::test]
    async fn malformed_code_is_none_without_store_hit() {
        let f = fixture().await;
        let mut rx = f.bus.subscribe();
        for bad in ["", "short", "K7QM4-ZP2RU", "K7QM4ZP2RXX"] {
            assert!(f.svc.exchange(bad, "h", None).await.unwrap().is_none());
        }
        assert!(f.auth.list().await.unwrap().is_empty());
        assert!(rx.try_recv().is_err());
        assert!(f.svc.lookup_user_code("nope").await.unwrap().is_none());
        assert_eq!(f.svc.poll("garbage").await.unwrap(), PollReply::Expired);
    }
}
