use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use coord::clock::{Clock, SystemClock};
use coord::config::{ServerConfig, backend_from_env};
use coord::domain::TokenId;
use coord::http::{AppState, router};
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
    let app = router(AppState::new(store, Arc::new(SystemClock), config));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("coord: listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
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
