mod config;
mod db;
mod library;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::{Config, Paths};
use library::Library;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "watch",
    version,
    about = "Browse and resume your local media library"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Fuzzy-search the library when opening the browser
    query: Option<String>,
    /// Use a specific TOML configuration file
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Store history in this directory
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Scan this directory instead of the configured library (repeatable)
    #[arg(long = "root", global = true)]
    roots: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Print your media library without opening the terminal interface
    List,
    /// Print configuration and history locations
    Config,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("watch: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new(cli.config.clone(), cli.data_dir)?;
    if matches!(cli.command, Some(Command::Config)) {
        println!(
            "Config: {}\nHistory: {}",
            paths.config.display(),
            paths.database.display()
        );
        return Ok(());
    }
    let mut config = Config::load(&paths.config, cli.config.is_some())?;
    if !cli.roots.is_empty() {
        config.media_dirs = cli.roots;
    }
    let library = Library::scan(&config);
    for warning in &library.warnings {
        eprintln!("watch: {warning}");
    }
    let store = db::Store::open(&paths.database)?;
    let progress = store.all()?;
    for (group, items) in &library.groups {
        println!("{group}");
        for item in items {
            let status = progress
                .get(&item.path)
                .cloned()
                .unwrap_or_default()
                .status();
            println!(
                "  {}{}  [{status}]",
                if item.extra { "Extra: " } else { "" },
                item.label
            );
        }
    }
    println!("{} media files", library.items().count());
    Ok(())
}
