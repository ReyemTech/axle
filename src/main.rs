use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use coord::clock::{Clock, SystemClock};
use coord::config::{ServerConfig, backend_from_env};
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
    },
    /// Stdio MCP bridge a harness spawns per session (reads COORD_TOKEN or ~/.coord/token,
    /// COORD_SERVER_URL, COORD_PROJECT)
    Bridge {
        /// The harness this session runs in, e.g. claude-code, codex, opencode
        #[arg(long)]
        harness: String,
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
    let result = match Cli::parse().command {
        Command::Bridge { harness } => {
            coord::bridge::run(&harness, stop_signal()).await;
            // Exit now: a pending blocking read of stdin would otherwise hold up runtime
            // shutdown after a signal. The bridge fails open, so this is always success.
            std::process::exit(0);
        }
        Command::Serve { bind } => serve(bind).await,
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

async fn serve(bind: Option<SocketAddr>) -> CliResult {
    let mut config = ServerConfig::from_env()?;
    if let Some(bind) = bind {
        config.bind = bind;
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
