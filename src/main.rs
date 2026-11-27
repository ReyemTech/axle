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
    /// Claude Code hook: reads the hook JSON on stdin, prints hook JSON on stdout. Always
    /// exits 0 (fail-open); problems are one line on stderr.
    Hook {
        /// session-start | user-prompt-submit | post-tool-use | stop (anything else is ignored)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Connect this machine to an Axle server: with a one-time code, or by approving in the
    /// browser
    Login {
        /// Server URL (default: AXL_SERVER_URL, then ~/.axl/config.toml, then localhost)
        #[arg(long)]
        url: Option<String>,
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
    /// Forget this machine's token (optionally revoking it on the server)
    Logout {
        /// Also revoke the token on the server
        #[arg(long)]
        revoke: bool,
    },
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
            hook(event).await;
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
            code,
            no_browser,
            force,
        } => {
            login(axle::cli::LoginArgs {
                url,
                code,
                no_browser,
                force,
            })
            .await
        }
        Command::Logout { revoke } => logout(revoke).await,
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
async fn hook(event: axle::hook::Event) {
    use std::io::{Read, Write};
    let work = async move {
        let stdin = tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            let _ = std::io::stdin().take(1 << 20).read_to_string(&mut buf);
            buf
        })
        .await
        .unwrap_or_default();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let token = std::env::var("AXL_TOKEN").ok();
        axle::hook::run(event, &stdin, home.as_deref(), token.as_deref()).await
    };
    // Hard ceiling on total runtime, including a stdin that never closes.
    match tokio::time::timeout(std::time::Duration::from_secs(5), tokio::spawn(work)).await {
        Ok(Ok(Ok(Some(out)))) => {
            let _ = writeln!(std::io::stdout(), "{out}");
        }
        Ok(Ok(Ok(None))) => {}
        Ok(Ok(Err(e))) => {
            let _ = writeln!(std::io::stderr(), "axl hook: {e}");
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
    let store = open_store(&config.backend).await?;
    let addr = config.bind;
    let state = AppState::new(store, Arc::new(SystemClock), config);
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

async fn logout(revoke: bool) -> CliResult {
    axle::cli::logout(&home_dir()?, revoke, &mut std::io::stdout()).await?;
    Ok(())
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
