mod config;
mod events;
mod library;
mod mpd;
mod subsonic;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;

use config::Config;
use events::EventBus;
use library::Library;

#[derive(Parser)]
#[command(version, about = "MPD-compatible daemon backed by Navidrome / OpenSubsonic")]
struct Cli {
    /// Config file [default: $XDG_CONFIG_HOME/npd/config.toml]
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (default).
    Run {
        /// Listen on these addresses instead of `mpd.bind` from the config.
        #[arg(long)]
        bind: Vec<String>,
    },
    /// Check the server connection and credentials.
    Ping,
    /// Run a full library sync in the foreground.
    Sync,
    /// Generate a token + salt pair for the config from a password.
    Token,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NPD_LOG")
                .unwrap_or_else(|_| "npd=info".into()),
        )
        .init();

    let cli = Cli::parse();
    if let Some(Command::Token) = cli.command {
        // Needs no config: this is how you write one.
        let password = if std::io::stdin().is_terminal() {
            rpassword::prompt_password("Navidrome password: ")?
        } else {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim_end_matches(['\r', '\n']).to_owned()
        };
        let (token, salt) = subsonic::new_token(&password);
        println!("token = \"{token}\"\nsalt = \"{salt}\"");
        return Ok(());
    }

    let config_path = match cli.config {
        Some(path) => path,
        None => Config::default_path()?,
    };
    let config = Config::load(&config_path)?;
    let client = Arc::new(subsonic::Client::new(
        &config.server.url,
        config.server.credentials()?,
    )?);

    match cli.command.unwrap_or(Command::Run { bind: Vec::new() }) {
        Command::Ping => ping(&client).await,
        Command::Sync => {
            let library = Library::open(&config.state_dir()?, client, EventBus::new())?;
            let report = library.update_now().await?;
            println!(
                "{} songs, {} albums, {} path collisions",
                report.songs, report.albums, report.uri_collisions
            );
            Ok(())
        }
        Command::Run { bind } => {
            let mut config = config;
            if !bind.is_empty() {
                config.mpd.bind = bind;
            }
            run(config, client).await
        }
        Command::Token => unreachable!(),
    }
}

async fn ping(client: &subsonic::Client) -> Result<()> {
    let info = client.ping().await.context("ping failed")?;
    println!(
        "{} {} (API {}{})",
        info.server_type.as_deref().unwrap_or("subsonic"),
        info.server_version.as_deref().unwrap_or("?"),
        info.version,
        if info.open_subsonic { ", OpenSubsonic" } else { "" }
    );
    if info.open_subsonic {
        for ext in client.extensions().await? {
            println!("  extension {} {:?}", ext.name, ext.versions);
        }
    }
    let scan = client.scan_status().await?;
    println!(
        "  {} songs, last scan {}",
        scan.count.map_or("?".into(), |c| c.to_string()),
        scan.last_scan.as_deref().unwrap_or("?")
    );
    Ok(())
}

async fn run(config: Config, client: Arc<subsonic::Client>) -> Result<()> {
    let events = EventBus::new();
    let library = Library::open(&config.state_dir()?, Arc::clone(&client), events.clone())?;
    if library.is_empty() {
        info!("no library yet; starting initial sync");
        library.start_update();
    }

    let ctx = Arc::new(mpd::Ctx {
        library,
        events,
        started: std::time::Instant::now(),
    });
    mpd::serve(&config.mpd, ctx).await?;

    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    Ok(())
}
