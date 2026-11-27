use std::net::SocketAddr;

use time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unsupported database scheme: {0}")]
    UnknownScheme(String),
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("environment variable {0} must not be empty")]
    Empty(&'static str),
    #[error("{0} must be a positive whole number, got {1:?}")]
    BadNumber(&'static str, String),
    #[error("invalid COORD_BIND address: {0}")]
    BadBind(String),
    #[error("{0} must be true or false, got {1:?}")]
    BadBool(&'static str, String),
    #[error("invalid trusted header name: {0:?}")]
    BadHeader(String),
}

/// A trusted-header-mode header name: blank means off; otherwise it must be a valid HTTP
/// header name.
pub fn trusted_header_name(raw: &str) -> Result<Option<String>, ConfigError> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok(None);
    }
    axum::http::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| ConfigError::BadHeader(name.to_string()))?;
    Ok(Some(name.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Sqlite(String),
    Postgres(String),
}

impl Backend {
    pub fn from_url(url: &str) -> Result<Backend, ConfigError> {
        let scheme = url.split_once(':').map_or(url, |(s, _)| s);
        match scheme {
            "sqlite" => Ok(Backend::Sqlite(url.to_string())),
            "postgres" | "postgresql" => Ok(Backend::Postgres(url.to_string())),
            other => Err(ConfigError::UnknownScheme(other.to_string())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub backend: Backend,
    pub bind: SocketAddr,
    pub admin_secret: String,
    pub lease_ttl: Duration,
    pub presence_stale: Duration,
    pub idle_renew_cap: Duration,
    /// Per-token sustained request rate (`COORD_RATE_LIMIT_PER_SEC`, default 20).
    pub rate_limit_per_sec: u32,
    /// Per-token burst size (`COORD_RATE_LIMIT_BURST`, default 100).
    pub rate_limit_burst: u32,
    /// Mark the UI session cookie `Secure` (`COORD_COOKIE_SECURE`, default false). Set it
    /// whenever the UI is reached over https, e.g. behind a TLS ingress.
    pub cookie_secure: bool,
    /// Trusted-header mode (`COORD_TRUSTED_HEADER` or `serve --trusted-header`): a UI
    /// request carrying this header is signed in without the admin secret. Only safe
    /// behind a proxy (e.g. oauth2-proxy) that sets it and strips it from client requests.
    pub trusted_header: Option<String>,
}

const DEFAULT_BIND: &str = "127.0.0.1:7420";

/// A required variable that is set and not blank.
fn required(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, ConfigError> {
    let value = get(name).ok_or(ConfigError::Missing(name))?;
    if value.trim().is_empty() {
        return Err(ConfigError::Empty(name));
    }
    Ok(value)
}

/// An optional positive integer variable, `default` when unset.
fn positive(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: u32,
) -> Result<u32, ConfigError> {
    match get(name) {
        None => Ok(default),
        Some(raw) => match raw.trim().parse::<u32>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(ConfigError::BadNumber(name, raw)),
        },
    }
}

/// An optional boolean variable (`true`/`false`/`1`/`0`, any case), `default` when unset.
fn boolean(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    match get(name) {
        None => Ok(default),
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(ConfigError::BadBool(name, raw)),
        },
    }
}

/// The backend named by `DATABASE_URL` (what the admin commands need, without the server's
/// other settings).
pub fn backend_from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Backend, ConfigError> {
    Backend::from_url(&required(&get, "DATABASE_URL")?)
}

pub fn backend_from_env() -> Result<Backend, ConfigError> {
    backend_from_lookup(|k| std::env::var(k).ok())
}

impl ServerConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Builds config from an arbitrary variable lookup (testable without mutating the process env).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let url = required(&get, "DATABASE_URL")?;
        let admin_secret = required(&get, "COORD_ADMIN_SECRET")?;
        let bind_raw = get("COORD_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind = bind_raw
            .parse()
            .map_err(|_| ConfigError::BadBind(bind_raw))?;
        Ok(Self {
            backend: Backend::from_url(&url)?,
            bind,
            admin_secret,
            lease_ttl: Duration::minutes(15),
            presence_stale: Duration::seconds(90),
            idle_renew_cap: Duration::hours(2),
            rate_limit_per_sec: positive(&get, "COORD_RATE_LIMIT_PER_SEC", 20)?,
            rate_limit_burst: positive(&get, "COORD_RATE_LIMIT_BURST", 100)?,
            cookie_secure: boolean(&get, "COORD_COOKIE_SECURE", false)?,
            trusted_header: match get("COORD_TRUSTED_HEADER") {
                Some(raw) => trusted_header_name(&raw)?,
                None => None,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn sqlite_url_selects_sqlite() {
        assert!(matches!(
            Backend::from_url("sqlite://./coord.db").unwrap(),
            Backend::Sqlite(_)
        ));
    }
    #[test]
    fn postgres_url_selects_postgres() {
        assert!(matches!(
            Backend::from_url("postgres://u@h/db").unwrap(),
            Backend::Postgres(_)
        ));
    }
    #[test]
    fn unknown_scheme_errors_with_scheme() {
        let e = Backend::from_url("mysql://x").unwrap_err();
        assert!(e.to_string().contains("mysql"));
    }
    #[test]
    fn defaults() {
        let c = ServerConfig::from_lookup(|k| match k {
            "DATABASE_URL" => Some("sqlite://./coord.db".into()),
            "COORD_ADMIN_SECRET" => Some("s3cret".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.lease_ttl, Duration::minutes(15));
        assert_eq!(c.presence_stale, Duration::seconds(90));
        assert_eq!(c.idle_renew_cap, Duration::hours(2));
        assert_eq!(c.bind.to_string(), "127.0.0.1:7420");
        assert_eq!(c.rate_limit_per_sec, 20);
        assert_eq!(c.rate_limit_burst, 100);
        assert!(!c.cookie_secure);
        assert_eq!(c.trusted_header, None);
    }

    #[test]
    fn cookie_secure_and_trusted_header() {
        let base = [
            ("DATABASE_URL", "sqlite://./coord.db"),
            ("COORD_ADMIN_SECRET", "s3cret"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let vars: Vec<(&str, &str)> = base.iter().chain(extra).copied().collect();
            ServerConfig::from_lookup(lookup(&vars))
        };
        let c = with(&[
            ("COORD_COOKIE_SECURE", "TRUE"),
            ("COORD_TRUSTED_HEADER", " X-Forwarded-User "),
        ])
        .unwrap();
        assert!(c.cookie_secure);
        assert_eq!(c.trusted_header.as_deref(), Some("X-Forwarded-User"));
        assert!(!with(&[("COORD_COOKIE_SECURE", "0")]).unwrap().cookie_secure);
        assert_eq!(
            with(&[("COORD_TRUSTED_HEADER", "  ")])
                .unwrap()
                .trusted_header,
            None
        );
        let e = with(&[("COORD_TRUSTED_HEADER", "x user")]).unwrap_err();
        assert!(
            matches!(e, ConfigError::BadHeader(ref h) if h == "x user"),
            "{e}"
        );
        let e = with(&[("COORD_COOKIE_SECURE", "yes")]).unwrap_err();
        assert!(
            matches!(e, ConfigError::BadBool("COORD_COOKIE_SECURE", _)),
            "{e}"
        );
    }

    #[test]
    fn rate_limit_overrides_and_bad_values() {
        let base = [
            ("DATABASE_URL", "sqlite://./coord.db"),
            ("COORD_ADMIN_SECRET", "s3cret"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let vars: Vec<(&str, &str)> = base.iter().chain(extra).copied().collect();
            ServerConfig::from_lookup(lookup(&vars))
        };
        let c = with(&[
            ("COORD_RATE_LIMIT_PER_SEC", "5"),
            ("COORD_RATE_LIMIT_BURST", "7"),
        ])
        .unwrap();
        assert_eq!((c.rate_limit_per_sec, c.rate_limit_burst), (5, 7));
        for (name, value) in [
            ("COORD_RATE_LIMIT_PER_SEC", "0"),
            ("COORD_RATE_LIMIT_PER_SEC", "fast"),
            ("COORD_RATE_LIMIT_BURST", "0"),
            ("COORD_RATE_LIMIT_BURST", "-1"),
        ] {
            let e = with(&[(name, value)]).unwrap_err();
            assert!(
                matches!(e, ConfigError::BadNumber(n, _) if n == name),
                "{e}"
            );
            assert!(e.to_string().contains(name), "{e}");
        }
    }
    #[test]
    fn missing_required_vars_error() {
        assert!(ServerConfig::from_lookup(|_| None).is_err());
    }

    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn empty_admin_secret_is_rejected() {
        let e = ServerConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "sqlite://./coord.db"),
            ("COORD_ADMIN_SECRET", "  "),
        ]))
        .unwrap_err();
        assert!(matches!(e, ConfigError::Empty("COORD_ADMIN_SECRET")), "{e}");
        assert!(e.to_string().contains("COORD_ADMIN_SECRET"));
    }

    #[test]
    fn empty_database_url_is_rejected() {
        let e = ServerConfig::from_lookup(lookup(&[
            ("DATABASE_URL", ""),
            ("COORD_ADMIN_SECRET", "s3cret"),
        ]))
        .unwrap_err();
        assert!(matches!(e, ConfigError::Empty("DATABASE_URL")), "{e}");
        assert!(matches!(
            backend_from_lookup(lookup(&[("DATABASE_URL", "")])),
            Err(ConfigError::Empty("DATABASE_URL"))
        ));
        assert!(matches!(
            backend_from_lookup(lookup(&[])),
            Err(ConfigError::Missing("DATABASE_URL"))
        ));
    }

    #[test]
    fn bind_override_and_bad_bind() {
        let c = ServerConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "sqlite://./coord.db"),
            ("COORD_ADMIN_SECRET", "s3cret"),
            ("COORD_BIND", "0.0.0.0:8080"),
        ]))
        .unwrap();
        assert_eq!(c.bind.to_string(), "0.0.0.0:8080");

        let e = ServerConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "sqlite://./coord.db"),
            ("COORD_ADMIN_SECRET", "s3cret"),
            ("COORD_BIND", "localhost"),
        ]))
        .unwrap_err();
        assert!(
            matches!(e, ConfigError::BadBind(ref b) if b == "localhost"),
            "{e}"
        );
    }
}
