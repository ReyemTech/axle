use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use coord::clock::{Clock, SystemClock};
use coord::config::{ServerConfig, backend_from_env, trusted_header_name};
use coord::domain::TokenId;
use coord::http::AppState;
use coord::service::auth::AuthService;
use coord::store::{Store, open_store};
use time::format_description::well_known::Rfc3339;

#[derive(Parser)]
#[command(name = "coord", version, about = "MCP coordination server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the coordination server (needs DATABASE_URL and COORD_ADMIN_SECRET)
    Serve {
        /// Address to listen on; overrides COORD_BIND (default 127.0.0.1:7420)
        #[arg(long)]
        bind: Option<SocketAddr>,
        /// Sign UI requests in by this header (set by an auth proxy such as oauth2-proxy)
        /// instead of the admin secret; overrides COORD_TRUSTED_HEADER. Only safe when the
        /// proxy strips the header from client requests.
        #[arg(long, value_name = "NAME")]
        trusted_header: Option<String>,
    },
    /// Stdio MCP bridge a harness spawns per session (reads COORD_TOKEN or ~/.coord/token,
    /// COORD_SERVER_URL, COORD_PROJECT)
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
            let _ = writeln!(std::io::stderr(), "coord hook: internal error");
        }));
        let event = std::env::args()
            .nth(2)
            .and_then(|a| <coord::hook::Event as clap::ValueEnum>::from_str(&a, true).ok());
        if let Some(event) = event {
            hook(event).await;
        }
        std::process::exit(0);
    }
    let result = match Cli::parse().command {
        Command::Bridge { harness } => {
            coord::bridge::run(&harness, stop_signal()).await;
            // Exit now: a pending blocking read of stdin would otherwise hold up runtime
            // shutdown after a signal. The bridge fails open, so this is always success.
            std::process::exit(0);
        }
        Command::Hook { .. } => unreachable!("handled before argument parsing"),
        Command::Serve {
            bind,
            trusted_header,
        } => serve(bind, trusted_header).await,
        Command::Migrate => migrate().await,
        Command::Token { command } => token(command).await,
        Command::Project => Err("not implemented".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("coord: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Never fails: every problem is at most one stderr line, and stdout stays empty.
async fn hook(event: coord::hook::Event) {
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
        let token = std::env::var("COORD_TOKEN").ok();
        coord::hook::run(event, &stdin, home.as_deref(), token.as_deref()).await
    };
    // Hard ceiling on total runtime, including a stdin that never closes.
    match tokio::time::timeout(std::time::Duration::from_secs(5), tokio::spawn(work)).await {
        Ok(Ok(Ok(Some(out)))) => {
            let _ = writeln!(std::io::stdout(), "{out}");
        }
        Ok(Ok(Ok(None))) => {}
        Ok(Ok(Err(e))) => {
            let _ = writeln!(std::io::stderr(), "coord hook: {e}");
        }
        Ok(Err(_)) => {} // the panic hook already wrote the one line
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "coord hook: timed out");
        }
    }
}

async fn serve(bind: Option<SocketAddr>, trusted_header: Option<String>) -> CliResult {
    let mut config = ServerConfig::from_env()?;
    if let Some(bind) = bind {
        config.bind = bind;
    }
    if let Some(raw) = trusted_header {
        config.trusted_header = trusted_header_name(&raw)?;
    }
    let store = open_store(&config.backend).await?;
    let addr = config.bind;
    let state = AppState::new(store, Arc::new(SystemClock), config);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("coord: listening on http://{}", listener.local_addr()?);
    coord::http::serve(listener, state, stop_signal()).await?;
    eprintln!("coord: stopped");
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
                eprintln!("coord: cannot listen for SIGTERM: {e}");
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
    eprintln!("coord: shutting down");
}

async fn migrate() -> CliResult {
    open_store(&backend_from_env()?).await?;
    eprintln!("coord: migrations applied");
    Ok(())
}

async fn token(command: TokenCommand) -> CliResult {
    let store: Arc<dyn Store> = open_store(&backend_from_env()?).await?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let auth = AuthService::new(store, clock);
    match command {
        TokenCommand::Create { label } => {
            let (id, plaintext) = auth.create(&label).await?;
            eprintln!("coord: created token {id} ({label}); it is shown only once");
            // Only the token on stdout, so `$(coord token create --label x)` captures it.
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
            eprintln!("coord: revoked token {id}");
        }
    }
    Ok(())
}
