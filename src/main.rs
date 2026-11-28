use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use axle::clock::{Clock, SystemClock};
use axle::config::{Overrides, ServerConfig, backend_from_env};
use axle::domain::TokenId;
use axle::http::AppState;
use axle::service::auth::AuthService;
use axle::store::{Store, open_store};
use clap::{Parser, Subcommand};
use time::format_description::well_known::Rfc3339;

#[derive(Parser)]
#[command(name = "axl", version, about = "MCP coordination server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the coordination server (needs DATABASE_URL; see AXL_UI_AUTH for UI sign-in)
    Serve {
        /// Address to listen on; overrides AXL_BIND (default 127.0.0.1:7420)
        #[arg(long)]
        bind: Option<SocketAddr>,
        /// Sign UI requests in by this header (set by an auth proxy such as oauth2-proxy)
        /// instead of the admin secret; overrides AXL_TRUSTED_HEADER. Only safe when the
        /// proxy strips the header from client requests.
        #[arg(long, value_name = "NAME")]
        trusted_header: Option<String>,
    },
    /// Stdio MCP bridge a harness spawns per session (reads AXL_TOKEN or ~/.axl/token,
    /// AXL_SERVER_URL, AXL_PROJECT)
    Bridge {
        /// The harness this session runs in, e.g. claude-code, codex, opencode
        #[arg(long)]
        harness: String,
    },
    /// Claude Code or Codex hook: reads the hook JSON on stdin, prints hook JSON on stdout.
    /// Always exits 0 (fail-open); problems are one line on stderr. Codex's commands add
    /// `--harness codex`.
    Hook {
        /// session-start | user-prompt-submit | post-tool-use | stop (anything else is ignored)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Connect this machine to an Axle server: with a one-time code, or by approving in the
    /// browser
    Login {
        /// Server URL (default: AXL_SERVER_URL, then ~/.axl/config.toml, then localhost)
        #[arg(value_name = "URL")]
        url: Option<String>,
        /// The server URL as a flag (same as the positional URL; kept for older scripts)
        #[arg(long = "url", value_name = "URL", conflicts_with = "url", hide = true)]
        url_flag: Option<String>,
        /// One-time code from the web UI (XXXXX-XXXXX)
        #[arg(long)]
        code: Option<String>,
        /// Print the approval URL instead of opening a browser
        #[arg(long)]
        no_browser: bool,
        /// Switch to a different server than the one this machine is connected to
        #[arg(long)]
        force: bool,
    },
    /// Set up this machine: connect to a server (or run one locally), then configure every
    /// detected harness (Claude Code, Codex, OpenCode)
    Install {
        /// Connect to this server, logging in first if this machine has no token for it
        #[arg(long, value_name = "URL", conflicts_with = "local")]
        server: Option<String>,
        /// One-time code from the web UI (XXXXX-XXXXX), used with --server
        #[arg(long, requires = "server")]
        code: Option<String>,
        /// Only configure this harness (repeatable)
        #[arg(long, value_enum, value_name = "NAME")]
        harness: Vec<axle::install::Harness>,
        /// Run the server on this machine as a background service
        #[arg(long)]
        local: bool,
        /// Print the planned changes as a diff and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Ask nothing: without a configured server or --server, use local mode
        #[arg(long)]
        yes: bool,
        /// Switch to --server even if this machine is connected to another server
        #[arg(long, requires = "server")]
        force: bool,
        /// Port for the local service (implies --local)
        #[arg(long, value_name = "PORT", conflicts_with = "server")]
        port: Option<u16>,
    },
    /// Undo `axl install`: remove what it added to each harness (optionally also the local
    /// service and this machine's token)
    Uninstall {
        /// Only undo this harness (repeatable)
        #[arg(long, value_enum, value_name = "NAME")]
        harness: Vec<axle::install::Harness>,
        /// Also stop and remove the local background service (the database is kept)
        #[arg(long)]
        service: bool,
        /// With --service: also delete the local database
        #[arg(long, requires = "service")]
        purge: bool,
        /// Revoke this machine's token on the server and forget it
        #[arg(long)]
        revoke: bool,
    },
    /// Forget this machine's token (optionally revoking it on the server)
    Logout {
        /// Also revoke the token on the server
        #[arg(long)]
        revoke: bool,
    },
    /// Run the server in the background: a launchd agent (macOS) or systemd user unit (Linux)
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Show whether this machine is connected: server, token, local service, each harness
    Status,
    /// Run database migrations (needs DATABASE_URL)
    Migrate,
    /// Manage projects
    Project,
    /// Manage agent bearer tokens (needs DATABASE_URL)
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Install the service and start it (also at login)
    Install {
        /// Port to listen on (default 7420, or the one saved by an earlier install)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Stop and remove the service; the database is kept
    Uninstall {
        /// Also delete ~/.axl/axl.db
        #[arg(long)]
        purge: bool,
    },
    Start,
    Stop,
    Restart,
    Status,
    /// Print the end of ~/.axl/logs/axl.log
    Logs,
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Create a token and print it (shown only this once)
    Create {
        #[arg(long)]
        label: String,
    },
    /// List tokens: id, label, created, last used, revoked
    List,
    /// Revoke a token by id
    Revoke { id: TokenId },
}

type CliResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> ExitCode {
    // Hooks run inside the harness, where exit 2 blocks the user's prompt: bypass clap
    // (whose usage errors exit 2) and never exit non-zero.
    if std::env::args().nth(1).as_deref() == Some("hook") {
        std::panic::set_hook(Box::new(|_| {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "axl hook: internal error");
        }));
        let event = std::env::args()
            .nth(2)
            .and_then(|a| <axle::hook::Event as clap::ValueEnum>::from_str(&a, true).ok());
        if let Some(event) = event {
            let rest: Vec<String> = std::env::args().skip(3).collect();
            hook(event, axle::hook::Harness::from_args(&rest)).await;
        }
        std::process::exit(0);
    }
    let result = match Cli::parse().command {
        Command::Bridge { harness } => {
            axle::bridge::run(&harness, stop_signal()).await;
            // Exit now: a pending blocking read of stdin would otherwise hold up runtime
            // shutdown after a signal. The bridge fails open, so this is always success.
            std::process::exit(0);
        }
        Command::Hook { .. } => unreachable!("handled before argument parsing"),
        Command::Serve {
            bind,
            trusted_header,
        } => serve(bind, trusted_header).await,
        Command::Login {
            url,
            url_flag,
            code,
            no_browser,
            force,
        } => {
            login(axle::cli::LoginArgs {
                url: url.or(url_flag),
                code,
                no_browser,
                force,
            })
            .await
        }
        Command::Install {
            server,
            code,
            harness,
            local,
            dry_run,
            yes,
            force,
            port,
        } => {
            return install(axle::cli::InstallArgs {
                server,
                code,
                harness,
                local,
                dry_run,
                yes,
                force,
                port,
            })
            .await;
        }
        Command::Uninstall {
            harness,
            service,
            purge,
            revoke,
        } => {
            return uninstall(axle::cli::UninstallArgs {
                harness,
                service,
                purge,
                revoke,
            })
            .await;
        }
        Command::Status => return status().await,
        Command::Logout { revoke } => logout(revoke).await,
        Command::Service { command } => service(command),
        Command::Migrate => migrate().await,
        Command::Token { command } => token(command).await,
        Command::Project => Err("not implemented".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("axl: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Never fails: every problem is at most one stderr line, and stdout stays empty.
async fn hook(event: axle::hook::Event, harness: axle::hook::Harness) {
    use std::io::{Read, Write};
    let work = async move {
        // SessionStart needs nothing from its input, so it never waits on stdin.
        let stdin = if event == axle::hook::Event::SessionStart {
            String::new()
        } else {
            tokio::task::spawn_blocking(|| {
                let mut buf = String::new();
                let _ = std::io::stdin().take(1 << 20).read_to_string(&mut buf);
                buf
            })
            .await
            .unwrap_or_default()
        };
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let token = std::env::var("AXL_TOKEN").ok();
        axle::hook::run(event, harness, &stdin, home.as_deref(), token.as_deref()).await
    };
    // Hard ceiling on total runtime, including a stdin that never closes: below the
    // harnesses' 5 s hook timeout, so the hook ends on its own terms.
    match tokio::time::timeout(axle::hook::CEILING, tokio::spawn(work)).await {
        Ok(Ok(out)) => {
            if let Some(stdout) = out.stdout {
                let _ = writeln!(std::io::stdout(), "{stdout}");
            }
            if let Some(e) = out.stderr {
                let _ = writeln!(std::io::stderr(), "axl hook: {e}");
            }
        }
        Ok(Err(_)) => {} // the panic hook already wrote the one line
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "axl hook: timed out");
        }
    }
}

async fn serve(bind: Option<SocketAddr>, trusted_header: Option<String>) -> CliResult {
    let config = ServerConfig::from_env_with(Overrides {
        bind,
        trusted_header,
    })?;
    eprintln!("axl: UI sign-in: {}", config.ui_auth.name());
    if config.ui_exposed_without_auth() {
        eprintln!(
            "axl: WARNING: the web UI has NO SIGN-IN and listens on {}. Anyone who can reach \
             it acts as the human. Only run this behind a VPN or an authenticating proxy; \
             the UI answers only these Host names: {} (add more with AXL_ALLOWED_HOSTS).",
            config.bind,
            config.allowed_hosts.join(", ")
        );
    }
    let prices = axle::usage::prices::PriceTable::load(config.prices_file.as_deref())?;
    eprintln!(
        "axl: prices: {}, {} models{}",
        prices.version(),
        prices.len(),
        config
            .prices_file
            .as_ref()
            .map(|f| format!(" (+ {})", f.display()))
            .unwrap_or_default()
    );
    let store = open_store(&config.backend).await?;
    let addr = config.bind;
    let state = AppState::new(store, Arc::new(SystemClock), config).with_prices(prices);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("axl: listening on http://{}", listener.local_addr()?);
    axle::http::serve(listener, state, stop_signal()).await?;
    eprintln!("axl: stopped");
    Ok(())
}

/// Resolves on SIGINT (Ctrl-C) or, on Unix, SIGTERM (what Kubernetes sends).
async fn stop_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                eprintln!("axl: cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    eprintln!("axl: shutting down");
}

fn home_dir() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty())
        .ok_or_else(|| "HOME is not set".into())
}

async fn login(args: axle::cli::LoginArgs) -> CliResult {
    axle::cli::login(&home_dir()?, args, &mut std::io::stdout()).await?;
    Ok(())
}

async fn install(args: axle::cli::InstallArgs) -> ExitCode {
    use std::io::IsTerminal;
    let run = async {
        let home = home_dir()?;
        let service_home = home.clone();
        let service =
            move || axle::daemon::platform(&service_home, Box::new(axle::daemon::SystemRunner));
        let mut stdout = std::io::stdout();
        let mut env = axle::cli::InstallEnv {
            home,
            path_env: std::env::var_os("PATH"),
            current_exe: std::env::current_exe()?,
            // Both ends: with stdout redirected the question would be invisible.
            interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            prompt: &axle::cli::TerminalPrompt,
            out: &mut stdout,
            service: &service,
            login_hooks: axle::cli::LoginHooks::default(),
            now: time::OffsetDateTime::now_utc(),
        };
        let report = axle::cli::install(&mut env, args).await?;
        Ok::<_, Box<dyn std::error::Error>>(report.exit_code())
    };
    match run.await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("axl: {}", axle::cli::install::redact(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

async fn status() -> ExitCode {
    let run = async {
        let home = home_dir()?;
        let service_home = home.clone();
        let service =
            move || axle::daemon::platform(&service_home, Box::new(axle::daemon::SystemRunner));
        let mut stdout = std::io::stdout();
        let mut env = axle::cli::InstallEnv {
            home,
            path_env: std::env::var_os("PATH"),
            current_exe: std::env::current_exe()?,
            interactive: false,
            prompt: &axle::cli::TerminalPrompt,
            out: &mut stdout,
            service: &service,
            login_hooks: axle::cli::LoginHooks::default(),
            now: time::OffsetDateTime::now_utc(),
        };
        let report = axle::cli::status::run(&mut env).await;
        Ok::<_, Box<dyn std::error::Error>>(report.exit_code())
    };
    match run.await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("axl: {}", axle::cli::install::redact(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

async fn uninstall(args: axle::cli::UninstallArgs) -> ExitCode {
    let run = async {
        let home = home_dir()?;
        let service_home = home.clone();
        let service =
            move || axle::daemon::platform(&service_home, Box::new(axle::daemon::SystemRunner));
        let mut stdout = std::io::stdout();
        let mut env = axle::cli::InstallEnv {
            home,
            path_env: std::env::var_os("PATH"),
            current_exe: std::env::current_exe()?,
            interactive: false,
            prompt: &axle::cli::TerminalPrompt,
            out: &mut stdout,
            service: &service,
            login_hooks: axle::cli::LoginHooks::default(),
            now: time::OffsetDateTime::now_utc(),
        };
        axle::cli::uninstall(&mut env, args).await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    match run.await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("axl: {}", axle::cli::install::redact(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

async fn logout(revoke: bool) -> CliResult {
    axle::cli::logout(&home_dir()?, revoke, &mut std::io::stdout()).await?;
    Ok(())
}

fn service(command: ServiceCommand) -> CliResult {
    use axle::cli::ServiceAction;
    let action = match command {
        ServiceCommand::Install { port } => ServiceAction::Install { port },
        ServiceCommand::Uninstall { purge } => ServiceAction::Uninstall { purge },
        ServiceCommand::Start => ServiceAction::Start,
        ServiceCommand::Stop => ServiceAction::Stop,
        ServiceCommand::Restart => ServiceAction::Restart,
        ServiceCommand::Status => ServiceAction::Status,
        ServiceCommand::Logs => ServiceAction::Logs,
    };
    let exe = std::env::current_exe()?;
    let axl = axle::install::axl_path(&exe, std::env::var_os("PATH").as_deref());
    axle::cli::service(
        &home_dir()?,
        &axl,
        std::env::consts::OS,
        Box::new(axle::daemon::SystemRunner),
        action,
        &mut std::io::stdout(),
    )
}

async fn migrate() -> CliResult {
    open_store(&backend_from_env()?).await?;
    eprintln!("axl: migrations applied");
    Ok(())
}

async fn token(command: TokenCommand) -> CliResult {
    let store: Arc<dyn Store> = open_store(&backend_from_env()?).await?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let auth = AuthService::new(store, clock);
    match command {
        TokenCommand::Create { label } => {
            let (id, plaintext) = auth.create(&label).await?;
            eprintln!("axl: created token {id} ({label}); it is shown only once");
            // Only the token on stdout, so `$(axl token create --label x)` captures it.
            println!("{plaintext}");
        }
        TokenCommand::List => {
            let stamp = |t: Option<time::OffsetDateTime>| {
                t.and_then(|t| t.format(&Rfc3339).ok())
                    .unwrap_or_else(|| "-".into())
            };
            for t in auth.list().await? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    t.id,
                    t.label,
                    stamp(Some(t.created_at)),
                    stamp(t.last_used_at),
                    stamp(t.revoked_at)
                );
            }
        }
        TokenCommand::Revoke { id } => {
            auth.revoke(id).await?;
            eprintln!("axl: revoked token {id}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login_url(argv: &[&str]) -> Option<String> {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Login { url, url_flag, .. } => url.or(url_flag),
            _ => panic!("not a login command"),
        }
    }

    /// The exact command the web UI's "Connect a machine" panel shows.
    #[test]
    fn ui_login_command_parses() {
        let url = "https://axle.example.com";
        let code = "ABCDE-FGHJK";
        let shown = axle::ui::connect::login_command(url, code);
        let argv: Vec<&str> = shown.split(' ').collect();
        match Cli::try_parse_from(&argv).unwrap().command {
            Command::Install {
                server: u,
                code: c,
                force,
                local,
                ..
            } => {
                assert_eq!(u.as_deref(), Some(url));
                assert_eq!(c.as_deref(), Some(code));
                assert!(!force && !local);
            }
            _ => panic!("not an install command"),
        }
    }

    #[test]
    fn login_url_flag_still_works_and_conflicts_with_positional() {
        assert_eq!(
            login_url(&["axl", "login", "--url", "http://h:7420"]).as_deref(),
            Some("http://h:7420")
        );
        assert_eq!(login_url(&["axl", "login"]), None);
        assert!(Cli::try_parse_from(["axl", "login", "http://a", "--url", "http://b"]).is_err());
    }
}
