use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "coord", version, about = "MCP coordination server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server
    Serve,
    /// Run database migrations
    Migrate,
    /// Manage projects
    Project,
    /// Manage agent tokens
    Token,
}

fn main() {
    let _cli = Cli::parse();
    eprintln!("not implemented");
    std::process::exit(2);
}
