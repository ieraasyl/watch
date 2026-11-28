mod config;
mod db;
mod library;
mod player;
mod resume;
mod ui;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, Paths};
use library::Library;
use std::{
    collections::HashMap,
    io::{self, IsTerminal},
    path::PathBuf,
    sync::atomic::Ordering,
};

#[derive(Parser)]
#[command(
    name = "wvd",
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
    /// Resume the latest unfinished video, or continue its series
    Resume,
    /// Print configuration and history locations
    Config,
    /// Back up or merge watch history
    #[command(subcommand)]
    History(HistoryCommand),
}

#[derive(Subcommand)]
enum HistoryCommand {
    /// Save a copy of your watch history to a new file
    Export { file: PathBuf },
    /// Merge watch history from an exported file, keeping the newest entry for each video
    Import { file: PathBuf },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("wvd: {error:#}");
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
    if let Some(Command::History(HistoryCommand::Export { file })) = &cli.command {
        let count = db::Store::open(&paths.database)?.export(file)?;
        println!("Exported {count} history entries to {}", file.display());
        return Ok(());
    }
    if let Some(Command::History(HistoryCommand::Import { file })) = &cli.command {
        // Merge before reading the configuration, so a configuration problem cannot block it.
        let mut store = db::Store::open(&paths.database)?;
        let imported = store.import(file)?;
        println!(
            "Imported {} new and {} updated history entries. Skipped {} that were not newer",
            imported.added, imported.updated, imported.kept
        );
        if imported.displaced > 0 {
            println!(
                "Kept {} entries until wvd finds their videos",
                imported.displaced
            );
        }
        // The import is already saved, so library problems are reported without failing it.
        let explicit = cli.config.is_some();
        match restore_imported(explicit, &cli.roots, &paths, &mut store) {
            Ok(Some(message)) => println!("{message}"),
            Ok(None) => {}
            Err(error) => {
                eprintln!("wvd: Could not check your library for moved videos: {error:#}")
            }
        }
        return Ok(());
    }
    let mut config = Config::load(&paths.config, cli.config.is_some())?;
    if !cli.roots.is_empty() {
        config.media_dirs = cli.roots;
    }
    let mut library = Library::scan(&config);
    for warning in &library.warnings {
        eprintln!("wvd: {warning}");
    }
    let mut store = db::Store::open(&paths.database)?;
    let restored =
        ui::restored_message(store.reattach(library.items().map(|item| item.path.as_path())));
    if matches!(cli.command, Some(Command::List | Command::Resume))
        && let Some(message) = &restored
    {
        eprintln!("wvd: {message}");
    }
    let progress = store.all()?;
    if matches!(cli.command, Some(Command::List)) {
        return match list(&library, &progress, io::stdout().lock()) {
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error.into()),
            _ => Ok(()),
        };
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("The browser needs an interactive terminal. Use `wvd list` to print your library");
    }
    ctrlc::set_handler(|| player::INTERRUPTED.store(true, Ordering::Relaxed))?;
    let mut browser = ui::Browser::new(cli.query.unwrap_or_default());
    let mut action = if matches!(cli.command, Some(Command::Resume)) {
        let candidate = resume::candidates(&library, &progress)
            .into_iter()
            .next()
            .context("Nothing to resume in this library. Open `wvd` to choose a video")?;
        ui::Action::Play {
            media: candidate.clone(),
            restart: false,
        }
    } else {
        browser.inform(restored);
        browser.run(&mut library, &config, &mut store)?
    };
    loop {
        match action {
            ui::Action::Quit => break,
            ui::Action::Play { media, restart } => {
                let history = store.all()?;
                let previous = if restart {
                    db::Progress::default()
                } else {
                    history.get(&media.path).cloned().unwrap_or_default()
                };
                println!("Playing {} · {}", media.group(), media.label);
                match player::play(&media, &previous, &config, &store) {
                    Ok(finished) => {
                        browser.notice.clear();
                        if finished {
                            browser.next =
                                resume::next_episode(&library, &media, &store.all()?).cloned();
                        }
                    }
                    Err(error) => browser.notice = format!("{error:#}"),
                }
                if player::INTERRUPTED.load(Ordering::Relaxed) {
                    break;
                }
                action = browser.run(&mut library, &config, &mut store)?;
            }
        }
    }
    Ok(())
}

/// Scan the library and restore imported history to moved videos.
fn restore_imported(
    explicit_config: bool,
    roots: &[PathBuf],
    paths: &Paths,
    store: &mut db::Store,
) -> Result<Option<String>> {
    let mut config = Config::load(&paths.config, explicit_config)?;
    if !roots.is_empty() {
        config.media_dirs = roots.to_vec();
    }
    let library = Library::scan(&config);
    for warning in &library.warnings {
        eprintln!("wvd: {warning}");
    }
    let restored = store.reattach(library.items().map(|item| item.path.as_path()))?;
    Ok(ui::restored_message(Ok(restored)))
}

fn list(
    library: &Library,
    progress: &HashMap<PathBuf, db::Progress>,
    mut output: impl io::Write,
) -> io::Result<()> {
    for (group, items) in &library.groups {
        writeln!(output, "{group}")?;
        for item in items {
            let status = progress
                .get(&item.path)
                .cloned()
                .unwrap_or_default()
                .status();
            writeln!(
                output,
                "  {}{}  [{status}]",
                if item.extra { "Extra: " } else { "" },
                item.label
            )?;
        }
    }
    writeln!(output, "{} media files", library.items().count())?;
    output.flush()
}
