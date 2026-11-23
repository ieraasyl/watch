use anyhow::{Context, Result, bail};
use directories::{BaseDirs, ProjectDirs};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::PathBuf};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub media_dirs: Vec<PathBuf>,
    pub player: PathBuf,
    pub completion_threshold: f64,
    pub titles: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            media_dirs: vec![
                BaseDirs::new()
                    .map(|d| d.home_dir().join("Movies"))
                    .unwrap_or_else(|| PathBuf::from("Movies")),
            ],
            player: PathBuf::from("mpv"),
            completion_threshold: 0.95,
            titles: BTreeMap::new(),
        }
    }
}

pub struct Paths {
    pub config: PathBuf,
    pub database: PathBuf,
}

impl Paths {
    pub fn new(config: Option<PathBuf>, data_dir: Option<PathBuf>) -> Result<Self> {
        let project = ProjectDirs::from("", "", "watch-cli")
            .context("Could not determine your application directories")?;
        Ok(Self {
            config: config.unwrap_or_else(|| project.config_dir().join("config.toml")),
            database: data_dir
                .unwrap_or_else(|| project.data_local_dir().to_path_buf())
                .join("history.sqlite3"),
        })
    }
}

impl Config {
    pub fn load(path: &std::path::Path, explicit: bool) -> Result<Self> {
        let mut config: Self = match fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents)
                .with_context(|| format!("Invalid configuration: {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("Read {}", path.display())),
        };
        if !config.completion_threshold.is_finite()
            || config.completion_threshold <= 0.0
            || config.completion_threshold > 1.0
        {
            bail!("completion_threshold must be greater than 0 and at most 1");
        }
        if config.media_dirs.is_empty() {
            bail!("media_dirs must contain at least one directory");
        }
        for dir in &mut config.media_dirs {
            *dir = expand_home(dir)?;
        }
        config.player = expand_home(&config.player)?;
        Ok(config)
    }
}

fn expand_home(path: &std::path::Path) -> Result<PathBuf> {
    if path.starts_with("~") {
        let home = BaseDirs::new().context("Could not determine your home directory")?;
        Ok(home.home_dir().join(path.strip_prefix("~")?))
    } else {
        Ok(path.to_path_buf())
    }
}
