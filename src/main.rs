mod app;
mod apt;
mod config;
mod gather;
mod ssh;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(
    name = "aptmatic",
    about = "TUI for managing apt across multiple remote Debian/Ubuntu hosts",
    version
)]
struct Cli {
    /// Path to the configuration file
    #[arg(short, long, value_name = "FILE")]
    configfile: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.configfile.unwrap_or_else(default_config_path);

    let config = config::Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    if config.resolved_hosts().is_empty() {
        eprintln!("No hosts defined in {}", config_path.display());
        eprintln!(
            "\nExample config:\n\
             [defaults]\n\
             user = \"ubuntu\"\n\
             \n\
             [[groups]]\n\
             name = \"webservers\"\n\
             [[groups.hosts]]\n\
             hostname = \"web1.example.com\"\n"
        );
        std::process::exit(1);
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let application = app::App::new(&config, tx);
    app::run(application, rx).await
}

fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aptmatic.toml")
}
