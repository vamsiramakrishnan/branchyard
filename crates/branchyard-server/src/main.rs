use branchyard_server::{config::Config, http, Store};
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Branchyard durable admission server; no execution worker is bundled"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Generate a bootstrap credential once; protect stdout. Store only the hash in configuration.
    Token,
    /// Apply migrations with an administrative database role, including the PGMQ extension.
    Migrate {
        #[arg(long, env = "BRANCHYARD_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Serve HTTP behind a TLS reverse proxy or on loopback.
    Serve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, env = "BRANCHYARD_DATABASE_URL", hide_env_values = true)]
        database_url: String,
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
    },
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Args::parse().command {
        Command::Token => {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            println!(
                "{}",
                serde_json::json!({"token":token,"token_sha256":branchyard_protocol::sha256(token.as_bytes())})
            );
        }
        Command::Migrate { database_url } => {
            let pool = sqlx::PgPool::connect(&database_url)
                .await
                .map_err(|_| "database connection failed")?;
            Store::migrate(&pool).await?;
        }
        Command::Serve {
            config,
            database_url,
            bind,
        } => {
            let bytes = std::fs::read(config)?;
            if bytes.len() > 1024 * 1024 {
                return Err("configuration exceeds 1 MiB".into());
            }
            let config: Config =
                serde_json::from_slice(&bytes).map_err(|_| "invalid configuration JSON")?;
            let store = Store::connect(&database_url, config).await?;
            store.initialize().await?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            eprintln!(
                "Branchyard admission listening on {}; execution_ready=false",
                listener.local_addr()?
            );
            axum::serve(listener, http::router(store))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
    }
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
