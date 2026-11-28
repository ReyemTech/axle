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
    #[error("invalid AXL_BIND address: {0}")]
    BadBind(String),
    #[error("{0} must be true or false, got {1:?}")]
    BadBool(&'static str, String),
    #[error("invalid trusted header name: {0:?}")]
    BadHeader(String),
    #[error(
        "AXL_PUBLIC_URL must be an http(s) address with a host and no path, using only letters, \
         digits and . : / - [ ], got {0:?}"
    )]
    BadPublicUrl(String),
    #[error("AXL_ADMIN_SECRET must be at least {0} characters")]
    ShortSecret(usize),
    #[error("AXL_UI_AUTH must be auto, none, secret or trusted-header, got {0:?}")]
    BadUiAuth(String),
    #[error("AXL_UI_AUTH={0} needs {1} to be set")]
    ModeNeeds(&'static str, &'static str),
    #[error(
        "the web UI has no sign-in configured and {0} is not a loopback address. Set \
         AXL_ADMIN_SECRET (at least 16 characters) to sign in with a secret, set \
         AXL_TRUSTED_HEADER behind an auth proxy that sets it, or set AXL_UI_AUTH=none to \
         run the UI without sign-in (only behind a VPN or a proxy that authenticates)"
    )]
    NoUiAuth(SocketAddr),
}

/// The requested UI sign-in mode (`AXL_UI_AUTH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAuthMode {
    /// Trusted header if set, else the admin secret if set, else none on a loopback bind,
    /// else refuse to start.
    Auto,
    None,
    Secret,
    TrustedHeader,
}

impl UiAuthMode {
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "none" => Ok(Self::None),
            "secret" => Ok(Self::Secret),
            "trusted-header" => Ok(Self::TrustedHeader),
            _ => Err(ConfigError::BadUiAuth(raw.to_string())),
        }
    }
}

/// How a human signs in to the web UI (agent bearer tokens are unaffected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAuth {
    /// No sign-in: every UI request is the human. An anonymous session still carries the
    /// CSRF token, and only allowlisted `Host` headers are served (DNS-rebinding guard).
    None,
    /// The login form checks this admin secret.
    Secret(String),
    /// A request carrying `header` (set by an auth proxy) is signed in; the login form takes
    /// `secret` when one is configured.
    TrustedHeader {
        header: String,
        secret: Option<String>,
    },
}

impl UiAuth {
    /// The secret the login form accepts, if any.
    pub fn secret(&self) -> Option<&str> {
        match self {
            UiAuth::None => None,
            UiAuth::Secret(s) => Some(s),
            UiAuth::TrustedHeader { secret, .. } => secret.as_deref(),
        }
    }

    /// The trusted-header-mode header name, if in that mode.
    pub fn trusted_header(&self) -> Option<&str> {
        match self {
            UiAuth::TrustedHeader { header, .. } => Some(header),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            UiAuth::None => "none",
            UiAuth::Secret(_) => "secret",
            UiAuth::TrustedHeader { .. } => "trusted-header",
        }
    }
}

/// Picks the UI sign-in mode from the request, the bind address and what is configured.
pub fn resolve_ui_auth(
    mode: UiAuthMode,
    bind: SocketAddr,
    secret: Option<String>,
    trusted_header: Option<String>,
) -> Result<UiAuth, ConfigError> {
    match mode {
        UiAuthMode::None => Ok(UiAuth::None),
        UiAuthMode::Secret => secret
            .map(UiAuth::Secret)
            .ok_or(ConfigError::ModeNeeds("secret", "AXL_ADMIN_SECRET")),
        UiAuthMode::TrustedHeader => trusted_header
            .map(|header| UiAuth::TrustedHeader { header, secret })
            .ok_or(ConfigError::ModeNeeds(
                "trusted-header",
                "AXL_TRUSTED_HEADER",
            )),
        UiAuthMode::Auto => match (trusted_header, secret) {
            (Some(header), secret) => Ok(UiAuth::TrustedHeader { header, secret }),
            (None, Some(secret)) => Ok(UiAuth::Secret(secret)),
            (None, None) if bind.ip().is_loopback() => Ok(UiAuth::None),
            (None, None) => Err(ConfigError::NoUiAuth(bind)),
        },
    }
}

/// The host part of a `Host` header or allowlist entry, lowercased and without the port
/// (`[::1]:7420` is `[::1]`, `Example.com:80` is `example.com`).
pub fn host_without_port(raw: &str) -> String {
    let raw = raw.trim();
    let host = if raw.starts_with('[') {
        raw.split_inclusive(']').next().unwrap_or(raw)
    } else {
        match raw.rsplit_once(':') {
            Some((host, port))
                if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) =>
            {
                host
            }
            _ => raw,
        }
    };
    host.to_ascii_lowercase()
}

/// Hosts the UI answers to with any port: loopback names, the bind host (unless it is a
/// wildcard) and `AXL_ALLOWED_HOSTS` (comma-separated).
fn allowed_hosts(bind: SocketAddr, extra: Option<&str>) -> Vec<String> {
    let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "[::1]"]
        .into_iter()
        .map(String::from)
        .collect();
    if !bind.ip().is_unspecified() {
        hosts.push(match bind {
            SocketAddr::V4(a) => a.ip().to_string(),
            SocketAddr::V6(a) => format!("[{}]", a.ip()),
        });
    }
    for entry in extra.unwrap_or_default().split(',') {
        let host = host_without_port(entry);
        if !host.is_empty() {
            hosts.push(host);
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Command-line values that take precedence over the environment (`axl serve` flags).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub bind: Option<SocketAddr>,
    pub trusted_header: Option<String>,
}

/// Shortest accepted `AXL_ADMIN_SECRET`, in characters (ignoring surrounding whitespace).
pub const ADMIN_SECRET_MIN: usize = 16;

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
    /// How a human signs in to the web UI (`AXL_UI_AUTH`, see [`resolve_ui_auth`]).
    pub ui_auth: UiAuth,
    /// Hosts (lowercase, no port) the UI answers to when `ui_auth` is none.
    pub allowed_hosts: Vec<String>,
    pub lease_ttl: Duration,
    pub presence_stale: Duration,
    pub idle_renew_cap: Duration,
    /// Per-token sustained request rate (`AXL_RATE_LIMIT_PER_SEC`, default 20).
    pub rate_limit_per_sec: u32,
    /// Per-token burst size (`AXL_RATE_LIMIT_BURST`, default 100).
    pub rate_limit_burst: u32,
    /// `AXL_DONE_AUTO_ACK_DAYS` (default 7 days).
    pub done_auto_ack: Duration,
    /// Mark the UI session cookie `Secure` (`AXL_COOKIE_SECURE`, default false). Set it
    /// whenever the UI is reached over https, e.g. behind a TLS ingress.
    pub cookie_secure: bool,
    /// The address clients reach this server at (`AXL_PUBLIC_URL`), without a trailing `/`.
    pub public_url: Option<String>,
    /// Header naming the client IP behind a proxy (`AXL_TRUSTED_PROXY_HEADER`); its right-most
    /// entry keys the per-IP limits. Unset: the socket address is used and the header ignored.
    pub trusted_proxy_header: Option<String>,
    /// Price overrides merged over the built-in table (`AXL_PRICES_FILE`; blank = none).
    pub prices_file: Option<std::path::PathBuf>,
}

/// The server cannot tell clients its own address: it is bound beyond loopback and
/// `AXL_PUBLIC_URL` is unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "set AXL_PUBLIC_URL to this server's public address (for example https://axle.example.com)"
)]
pub struct MissingPublicUrl;

/// A validated `AXL_PUBLIC_URL`: trimmed, no trailing `/`. The value is spliced into a shell
/// script, so only a conservative character set is accepted.
fn parse_public_url(raw: &str) -> Result<Option<String>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let bad = || ConfigError::BadPublicUrl(raw.to_string());
    let url = value.strip_suffix('/').unwrap_or(value);
    let safe = |c: char| c.is_ascii_alphanumeric() || ".:/-[]".contains(c);
    if !url.chars().all(safe) {
        return Err(bad());
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(bad)?;
    if rest.is_empty() || rest.contains('/') || rest.starts_with(':') {
        return Err(bad());
    }
    Ok(Some(url.to_string()))
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
        Self::from_lookup_with(get, Overrides::default())
    }

    /// Like [`ServerConfig::from_env`], with `axl serve` flags taking precedence.
    pub fn from_env_with(overrides: Overrides) -> Result<Self, ConfigError> {
        Self::from_lookup_with(|k| std::env::var(k).ok(), overrides)
    }

    pub fn from_lookup_with(
        get: impl Fn(&str) -> Option<String>,
        overrides: Overrides,
    ) -> Result<Self, ConfigError> {
        let url = required(&get, "DATABASE_URL")?;
        let mode = match get("AXL_UI_AUTH") {
            Some(raw) => UiAuthMode::parse(&raw)?,
            None => UiAuthMode::Auto,
        };
        // A set secret must be usable, unless the UI is explicitly without sign-in.
        let admin_secret = match get("AXL_ADMIN_SECRET") {
            Some(_) if mode == UiAuthMode::None => None,
            Some(_) => {
                let secret = required(&get, "AXL_ADMIN_SECRET")?;
                if secret.trim().chars().count() < ADMIN_SECRET_MIN {
                    return Err(ConfigError::ShortSecret(ADMIN_SECRET_MIN));
                }
                Some(secret)
            }
            None => None,
        };
        let bind = match overrides.bind {
            Some(bind) => bind,
            None => parse_bind(&get("AXL_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string()))?,
        };
        let trusted_header = match overrides
            .trusted_header
            .or_else(|| get("AXL_TRUSTED_HEADER"))
        {
            Some(raw) => trusted_header_name(&raw)?,
            None => None,
        };
        let public_url = match get("AXL_PUBLIC_URL") {
            Some(raw) => parse_public_url(&raw)?,
            None => None,
        };
        let trusted_proxy_header = match get("AXL_TRUSTED_PROXY_HEADER") {
            Some(raw) => trusted_header_name(&raw)?,
            None => None,
        };
        Ok(Self {
            backend: Backend::from_url(&url)?,
            bind,
            ui_auth: resolve_ui_auth(mode, bind, admin_secret, trusted_header)?,
            allowed_hosts: allowed_hosts(bind, get("AXL_ALLOWED_HOSTS").as_deref()),
            lease_ttl: Duration::minutes(15),
            presence_stale: Duration::seconds(90),
            idle_renew_cap: Duration::hours(2),
            rate_limit_per_sec: positive(&get, "AXL_RATE_LIMIT_PER_SEC", 20)?,
            rate_limit_burst: positive(&get, "AXL_RATE_LIMIT_BURST", 100)?,
            done_auto_ack: Duration::days(i64::from(positive(&get, "AXL_DONE_AUTO_ACK_DAYS", 7)?)),
            cookie_secure: boolean(&get, "AXL_COOKIE_SECURE", false)?,
            public_url,
            trusted_proxy_header,
            prices_file: get("AXL_PRICES_FILE")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(std::path::PathBuf::from),
        })
    }

    /// The address to give clients: `AXL_PUBLIC_URL` if set; on a loopback bind, the request's
    /// `Host` when it names a loopback host, else the bind address; otherwise an error (a
    /// `Host` header on a reachable server is client-controlled and the scheme is unknown
    /// behind a TLS proxy).
    pub fn advertised_url(&self, host_header: Option<&str>) -> Result<String, MissingPublicUrl> {
        if let Some(url) = &self.public_url {
            return Ok(url.clone());
        }
        if !self.bind.ip().is_loopback() {
            return Err(MissingPublicUrl);
        }
        if let Some(host) = host_header.map(str::trim)
            && matches!(
                host_without_port(host).as_str(),
                "127.0.0.1" | "localhost" | "[::1]"
            )
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".:-[]".contains(c))
        {
            return Ok(format!("http://{host}"));
        }
        Ok(format!("http://{}", self.bind))
    }

    /// Whether the UI answers a request for `host` (a `Host` header value, port allowed).
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host_without_port(host);
        self.allowed_hosts.contains(&host)
    }

    /// The UI has no sign-in and is reachable beyond this machine.
    pub fn ui_exposed_without_auth(&self) -> bool {
        self.ui_auth == UiAuth::None && !self.bind.ip().is_loopback()
    }
}

/// An `AXL_BIND` value: a socket address, or `localhost:<port>` (loopback IPv4).
fn parse_bind(raw: &str) -> Result<SocketAddr, ConfigError> {
    if let Ok(addr) = raw.trim().parse() {
        return Ok(addr);
    }
    raw.trim()
        .strip_prefix("localhost:")
        .and_then(|port| port.parse::<u16>().ok())
        .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
        .ok_or_else(|| ConfigError::BadBind(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn sqlite_url_selects_sqlite() {
        assert!(matches!(
            Backend::from_url("sqlite://./axl.db").unwrap(),
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
            "DATABASE_URL" => Some("sqlite://./axl.db".into()),
            "AXL_ADMIN_SECRET" => Some("s3cret-s3cret-s3cret".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.lease_ttl, Duration::minutes(15));
        assert_eq!(c.presence_stale, Duration::seconds(90));
        assert_eq!(c.idle_renew_cap, Duration::hours(2));
        assert_eq!(c.bind.to_string(), "127.0.0.1:7420");
        assert_eq!(c.rate_limit_per_sec, 20);
        assert_eq!(c.rate_limit_burst, 100);
        assert_eq!(c.done_auto_ack, Duration::days(7));
        assert!(!c.cookie_secure);
        assert_eq!(c.ui_auth, UiAuth::Secret("s3cret-s3cret-s3cret".into()));
    }

    #[test]
    fn done_auto_ack_days() {
        let base = [
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_ADMIN_SECRET", "s3cret-s3cret-s3cret"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let vars: Vec<(&str, &str)> = base.iter().chain(extra).copied().collect();
            ServerConfig::from_lookup(lookup(&vars))
        };
        assert_eq!(
            with(&[("AXL_DONE_AUTO_ACK_DAYS", "3")])
                .unwrap()
                .done_auto_ack,
            Duration::days(3)
        );
        assert_eq!(
            with(&[("AXL_DONE_AUTO_ACK_DAYS", "1")])
                .unwrap()
                .done_auto_ack,
            Duration::days(1)
        );
        for bad in ["0", "-1", "week", "1.5", ""] {
            let e = with(&[("AXL_DONE_AUTO_ACK_DAYS", bad)]).unwrap_err();
            assert!(
                matches!(e, ConfigError::BadNumber("AXL_DONE_AUTO_ACK_DAYS", _)),
                "{bad}: {e}"
            );
            assert!(e.to_string().contains("AXL_DONE_AUTO_ACK_DAYS"));
        }
    }

    #[test]
    fn cookie_secure_and_trusted_header() {
        let base = [
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_ADMIN_SECRET", "s3cret-s3cret-s3cret"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let vars: Vec<(&str, &str)> = base.iter().chain(extra).copied().collect();
            ServerConfig::from_lookup(lookup(&vars))
        };
        let c = with(&[
            ("AXL_COOKIE_SECURE", "TRUE"),
            ("AXL_TRUSTED_HEADER", " X-Forwarded-User "),
        ])
        .unwrap();
        assert!(c.cookie_secure);
        assert_eq!(c.ui_auth.trusted_header(), Some("X-Forwarded-User"));
        assert_eq!(c.ui_auth.secret(), Some("s3cret-s3cret-s3cret"));
        assert!(!with(&[("AXL_COOKIE_SECURE", "0")]).unwrap().cookie_secure);
        assert_eq!(
            with(&[("AXL_TRUSTED_HEADER", "  ")])
                .unwrap()
                .ui_auth
                .name(),
            "secret"
        );
        let e = with(&[("AXL_TRUSTED_HEADER", "x user")]).unwrap_err();
        assert!(
            matches!(e, ConfigError::BadHeader(ref h) if h == "x user"),
            "{e}"
        );
        let e = with(&[("AXL_COOKIE_SECURE", "yes")]).unwrap_err();
        assert!(
            matches!(e, ConfigError::BadBool("AXL_COOKIE_SECURE", _)),
            "{e}"
        );
    }

    #[test]
    fn rate_limit_overrides_and_bad_values() {
        let base = [
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_ADMIN_SECRET", "s3cret-s3cret-s3cret"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let vars: Vec<(&str, &str)> = base.iter().chain(extra).copied().collect();
            ServerConfig::from_lookup(lookup(&vars))
        };
        let c = with(&[
            ("AXL_RATE_LIMIT_PER_SEC", "5"),
            ("AXL_RATE_LIMIT_BURST", "7"),
        ])
        .unwrap();
        assert_eq!((c.rate_limit_per_sec, c.rate_limit_burst), (5, 7));
        for (name, value) in [
            ("AXL_RATE_LIMIT_PER_SEC", "0"),
            ("AXL_RATE_LIMIT_PER_SEC", "fast"),
            ("AXL_RATE_LIMIT_BURST", "0"),
            ("AXL_RATE_LIMIT_BURST", "-1"),
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
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_ADMIN_SECRET", "  "),
        ]))
        .unwrap_err();
        assert!(matches!(e, ConfigError::Empty("AXL_ADMIN_SECRET")), "{e}");
        assert!(e.to_string().contains("AXL_ADMIN_SECRET"));
    }

    #[test]
    fn empty_database_url_is_rejected() {
        let e = ServerConfig::from_lookup(lookup(&[
            ("DATABASE_URL", ""),
            ("AXL_ADMIN_SECRET", "s3cret-s3cret-s3cret"),
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
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_ADMIN_SECRET", "s3cret-s3cret-s3cret"),
            ("AXL_BIND", "0.0.0.0:8080"),
        ]))
        .unwrap();
        assert_eq!(c.bind.to_string(), "0.0.0.0:8080");

        let e = ServerConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_ADMIN_SECRET", "s3cret-s3cret-s3cret"),
            ("AXL_BIND", "localhost"),
        ]))
        .unwrap_err();
        assert!(
            matches!(e, ConfigError::BadBind(ref b) if b == "localhost"),
            "{e}"
        );
    }

    #[test]
    fn short_admin_secret_is_rejected() {
        let with = |secret: &'static str| {
            ServerConfig::from_lookup(lookup(&[
                ("DATABASE_URL", "sqlite://./axl.db"),
                ("AXL_ADMIN_SECRET", secret),
            ]))
        };
        let e = with("fifteen-chars-x").unwrap_err();
        assert!(matches!(e, ConfigError::ShortSecret(16)), "{e}");
        assert!(e.to_string().contains("AXL_ADMIN_SECRET"), "{e}");
        assert!(e.to_string().contains("16"), "{e}");
        // Surrounding whitespace does not count.
        assert!(matches!(
            with("   fifteen-chars-x   "),
            Err(ConfigError::ShortSecret(16))
        ));
        assert_eq!(
            with("sixteen-chars-xx").unwrap().ui_auth.secret(),
            Some("sixteen-chars-xx")
        );
    }

    const SECRET: &str = "s3cret-s3cret-s3cret";

    /// The resolved UI sign-in mode name (or the error) for these variables plus a DB URL.
    fn ui_auth(vars: &[(&str, &str)]) -> Result<&'static str, ConfigError> {
        let all: Vec<(&str, &str)> = [("DATABASE_URL", "sqlite://./axl.db")]
            .iter()
            .chain(vars)
            .copied()
            .collect();
        ServerConfig::from_lookup(lookup(&all)).map(|c| c.ui_auth.name())
    }

    #[test]
    fn ui_auth_resolution_table() {
        let lo = ("AXL_BIND", "127.0.0.1:7420");
        let lo6 = ("AXL_BIND", "[::1]:7420");
        let lo_name = ("AXL_BIND", "localhost:7420");
        let lo_other = ("AXL_BIND", "127.0.0.2:7420");
        let any = ("AXL_BIND", "0.0.0.0:7420");
        let lan = ("AXL_BIND", "192.168.1.5:7420");
        let secret = ("AXL_ADMIN_SECRET", SECRET);
        let header = ("AXL_TRUSTED_HEADER", "X-Forwarded-User");
        let mode = |m| ("AXL_UI_AUTH", m);
        let ok: &[(&[(&str, &str)], &str)] = &[
            // auto: nothing set, loopback (and the default bind) -> none
            (&[], "none"),
            (&[lo], "none"),
            (&[lo6], "none"),
            (&[lo_name], "none"),
            (&[lo_other], "none"),
            (&[mode("AUTO"), lo], "none"),
            (&[mode(""), lo], "none"),
            // auto: secret, then trusted header wins
            (&[secret, lo], "secret"),
            (&[secret, any], "secret"),
            (&[header, any], "trusted-header"),
            (&[header, secret, lan], "trusted-header"),
            // explicit none anywhere, even with a secret set (even a short one)
            (&[mode("none"), any], "none"),
            (&[mode("none"), lan, secret], "none"),
            (&[mode("none"), ("AXL_ADMIN_SECRET", "short")], "none"),
            (&[mode("none"), header, any], "none"),
            // explicit secret / trusted-header
            (&[mode("secret"), secret, any], "secret"),
            (&[mode("secret"), secret, header, any], "secret"),
            (&[mode("trusted-header"), header, any], "trusted-header"),
        ];
        for (vars, want) in ok {
            assert_eq!(ui_auth(vars).unwrap(), *want, "{vars:?}");
        }

        // auto with nothing configured off loopback refuses, naming the three ways out.
        for bind in [any, lan, ("AXL_BIND", "[::]:7420")] {
            let e = ui_auth(&[bind]).unwrap_err();
            assert!(matches!(e, ConfigError::NoUiAuth(_)), "{e}");
            let msg = e.to_string();
            for way in ["AXL_ADMIN_SECRET", "AXL_TRUSTED_HEADER", "AXL_UI_AUTH=none"] {
                assert!(msg.contains(way), "{msg}");
            }
        }
        // explicit modes need their setting, even on loopback
        let e = ui_auth(&[mode("secret"), lo]).unwrap_err();
        assert!(
            matches!(e, ConfigError::ModeNeeds("secret", "AXL_ADMIN_SECRET")),
            "{e}"
        );
        let e = ui_auth(&[mode("trusted-header"), secret]).unwrap_err();
        assert!(
            matches!(e, ConfigError::ModeNeeds(_, "AXL_TRUSTED_HEADER")),
            "{e}"
        );
        // a bad mode, and a short secret still rejected outside none mode
        let e = ui_auth(&[mode("open")]).unwrap_err();
        assert!(
            matches!(e, ConfigError::BadUiAuth(ref m) if m == "open"),
            "{e}"
        );
        assert!(matches!(
            ui_auth(&[mode("secret"), ("AXL_ADMIN_SECRET", "short")]),
            Err(ConfigError::ShortSecret(16))
        ));
        assert!(matches!(
            ui_auth(&[("AXL_ADMIN_SECRET", "short"), lo]),
            Err(ConfigError::ShortSecret(16))
        ));
    }

    #[test]
    fn cli_overrides_feed_ui_auth_resolution() {
        let vars = [("DATABASE_URL", "sqlite://./axl.db")];
        // --bind off loopback with nothing configured refuses ...
        let e = ServerConfig::from_lookup_with(
            lookup(&vars),
            Overrides {
                bind: Some("0.0.0.0:7420".parse().unwrap()),
                trusted_header: None,
            },
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::NoUiAuth(_)), "{e}");
        // ... and --trusted-header is a way out.
        let c = ServerConfig::from_lookup_with(
            lookup(&vars),
            Overrides {
                bind: Some("0.0.0.0:7420".parse().unwrap()),
                trusted_header: Some("X-User".into()),
            },
        )
        .unwrap();
        assert_eq!(c.ui_auth.trusted_header(), Some("X-User"));
        assert_eq!(c.bind.to_string(), "0.0.0.0:7420");
    }

    #[test]
    fn exposed_without_auth() {
        let c = |vars: &[(&str, &str)]| {
            let all: Vec<(&str, &str)> = [("DATABASE_URL", "sqlite://./axl.db")]
                .iter()
                .chain(vars)
                .copied()
                .collect();
            ServerConfig::from_lookup(lookup(&all)).unwrap()
        };
        assert!(!c(&[]).ui_exposed_without_auth());
        assert!(
            c(&[("AXL_UI_AUTH", "none"), ("AXL_BIND", "0.0.0.0:7420")]).ui_exposed_without_auth()
        );
        assert!(
            !c(&[("AXL_ADMIN_SECRET", SECRET), ("AXL_BIND", "0.0.0.0:7420")])
                .ui_exposed_without_auth()
        );
    }

    #[test]
    fn host_allowlist() {
        let c = ServerConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_UI_AUTH", "none"),
            ("AXL_BIND", "10.0.0.7:7420"),
            ("AXL_ALLOWED_HOSTS", " Axle.Internal , axle.lan:8443,, "),
        ]))
        .unwrap();
        for host in [
            "localhost",
            "localhost:7420",
            "127.0.0.1:9",
            "[::1]",
            "[::1]:7420",
            "10.0.0.7:7420",
            "axle.internal",
            "AXLE.INTERNAL:443",
            "axle.lan",
        ] {
            assert!(c.host_allowed(host), "{host}");
        }
        for host in [
            "",
            "evil.example",
            "localhost.evil.example",
            "127.0.0.2",
            "10.0.0.8:7420",
            "axle.internal.evil",
        ] {
            assert!(!c.host_allowed(host), "{host}");
        }
        // A wildcard bind adds no host of its own.
        let c = ServerConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_UI_AUTH", "none"),
            ("AXL_BIND", "0.0.0.0:7420"),
        ]))
        .unwrap();
        assert!(!c.host_allowed("0.0.0.0:7420"));
        assert_eq!(c.allowed_hosts, ["127.0.0.1", "[::1]", "localhost"]);
    }

    fn cfg(extra: &[(&'static str, &'static str)]) -> Result<ServerConfig, ConfigError> {
        let base = [
            ("DATABASE_URL", "sqlite://./axl.db"),
            ("AXL_ADMIN_SECRET", "s3cret-s3cret-s3cret"),
        ];
        let vars: Vec<(&str, &str)> = base.iter().chain(extra).copied().collect();
        ServerConfig::from_lookup(lookup(&vars))
    }

    #[test]
    fn public_url_parsed_and_trimmed() {
        let c = cfg(&[("AXL_PUBLIC_URL", " https://axle.example.com/ ")]).unwrap();
        assert_eq!(c.public_url.as_deref(), Some("https://axle.example.com"));
        let c = cfg(&[("AXL_PUBLIC_URL", "http://10.0.0.7:7420")]).unwrap();
        assert_eq!(c.public_url.as_deref(), Some("http://10.0.0.7:7420"));
        assert_eq!(cfg(&[]).unwrap().public_url, None);
        assert_eq!(cfg(&[("AXL_PUBLIC_URL", "  ")]).unwrap().public_url, None);
    }

    #[test]
    fn public_url_rejects_unsafe_values() {
        for bad in [
            "https://a.example/'x",
            "https://a b",
            "ftp://a",
            "https://a.example/sub",
            "https://$(id)",
            "https://",
            "axle.example.com",
        ] {
            let e = cfg(&[("AXL_PUBLIC_URL", bad)]).unwrap_err();
            assert!(matches!(e, ConfigError::BadPublicUrl(_)), "{bad}: {e}");
            assert!(e.to_string().contains("AXL_PUBLIC_URL"), "{e}");
        }
    }

    #[test]
    fn trusted_proxy_header_parsed() {
        let c = cfg(&[("AXL_TRUSTED_PROXY_HEADER", " X-Forwarded-For ")]).unwrap();
        assert_eq!(c.trusted_proxy_header.as_deref(), Some("X-Forwarded-For"));
        assert_eq!(cfg(&[]).unwrap().trusted_proxy_header, None);
        assert!(cfg(&[("AXL_TRUSTED_PROXY_HEADER", "x y")]).is_err());
    }

    #[test]
    fn advertised_url_rules() {
        let c = cfg(&[]).unwrap();
        assert_eq!(
            c.advertised_url(Some("localhost:7420")).unwrap(),
            "http://localhost:7420"
        );
        assert_eq!(
            c.advertised_url(Some("evil.example")).unwrap(),
            "http://127.0.0.1:7420"
        );
        assert_eq!(c.advertised_url(None).unwrap(), "http://127.0.0.1:7420");
        let lan = cfg(&[("AXL_BIND", "0.0.0.0:7420")]).unwrap();
        let e = lan.advertised_url(Some("localhost")).unwrap_err();
        assert!(e.to_string().contains("AXL_PUBLIC_URL"));
        let public = cfg(&[
            ("AXL_BIND", "0.0.0.0:7420"),
            ("AXL_PUBLIC_URL", "https://axle.example.com"),
        ])
        .unwrap();
        assert_eq!(
            public.advertised_url(Some("evil.example")).unwrap(),
            "https://axle.example.com"
        );
    }
}
