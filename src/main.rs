mod config;
mod db;
mod library;
mod player;
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
    let mut config = Config::load(&paths.config, cli.config.is_some())?;
    if !cli.roots.is_empty() {
        config.media_dirs = cli.roots;
    }
    let library = Library::scan(&config);
    for warning in &library.warnings {
        eprintln!("wvd: {warning}");
    }
    let store = db::Store::open(&paths.database)?;
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
        let candidate = resume_candidate(&library, &progress)
            .context("Nothing to resume yet. Run `wvd` and play something first")?;
        ui::Action::Play {
            media: candidate,
            restart: false,
        }
    } else {
        browser.run(&library, &store)?
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
                            browser.next = next_episode(&library, &media, &store.all()?);
                        }
                    }
                    Err(error) => browser.notice = format!("{error:#}"),
                }
                if player::INTERRUPTED.load(Ordering::Relaxed) {
                    break;
                }
                action = browser.run(&library, &store)?;
            }
        }
    }
    Ok(())
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

fn next_episode(
    library: &Library,
    media: &library::Media,
    history: &std::collections::HashMap<PathBuf, db::Progress>,
) -> Option<library::Media> {
    if media.extra || media.episode.is_none() {
        return None;
    }
    let items = library.groups.get(&media.group())?;
    let current = items.iter().position(|item| item.path == media.path)?;
    items
        .iter()
        .skip(current + 1)
        .find(|item| {
            !item.extra
                && item.episode.is_some()
                && !history.get(&item.path).is_some_and(|p| p.watched)
        })
        .cloned()
}

fn resume_candidate(
    library: &Library,
    history: &std::collections::HashMap<PathBuf, db::Progress>,
) -> Option<library::Media> {
    let latest = library
        .items()
        .filter(|item| {
            history
                .get(&item.path)
                .is_some_and(|p| p.position > 0.0 && !p.watched)
        })
        .max_by_key(|item| history[&item.path].updated);
    if let Some(media) = latest {
        return Some(media.clone());
    }
    let latest = library
        .items()
        .filter(|item| history.contains_key(&item.path))
        .max_by_key(|item| history[&item.path].updated)?;
    next_episode(library, latest, history)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_prefers_partial_playback_then_next_unwatched_episode() {
        let first = library::parse(PathBuf::from("Show - 01.mkv"), None);
        let second = library::parse(PathBuf::from("Show - 02.mkv"), None);
        let third = library::parse(PathBuf::from("Show - 03.mkv"), None);
        let library = Library {
            groups: [(
                "Show".to_owned(),
                vec![first.clone(), second.clone(), third.clone()],
            )]
            .into(),
            warnings: vec![],
        };
        let mut history = std::collections::HashMap::new();
        history.insert(
            first.path.clone(),
            db::Progress {
                position: 60.0,
                updated: 1,
                ..Default::default()
            },
        );
        history.insert(
            second.path.clone(),
            db::Progress {
                watched: true,
                updated: 2,
                ..Default::default()
            },
        );
        assert_eq!(
            resume_candidate(&library, &history).unwrap().path,
            first.path
        );
        history.get_mut(&first.path).unwrap().watched = true;
        assert_eq!(
            resume_candidate(&library, &history).unwrap().path,
            third.path
        );
        assert_eq!(
            next_episode(&library, &first, &history).unwrap().path,
            third.path
        );
    }
}
