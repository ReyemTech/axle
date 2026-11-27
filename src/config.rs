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
    #[error("invalid COORD_BIND address: {0}")]
    BadBind(String),
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
