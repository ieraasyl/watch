use crate::config::Config;
use regex::{Captures, Regex};
use std::{collections::BTreeMap, path::PathBuf, sync::LazyLock};
use walkdir::WalkDir;

static BRACKETS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]").unwrap());
static STANDARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(.*?)\bS(\d{1,2})[.\s]*E(\d+(?:\.\d+)?)(?:\b|\s)").unwrap());
static CROSS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(.*?)\b(\d{1,2})x(\d{1,3}(?:\.\d+)?)(?:\b|\s)").unwrap());
static ANIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(.*?)\s+-\s+(\d+(?:\.\d+)?)(?:v\d+)?(?:\s|$)").unwrap());
static SEASON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:season\s*|s)(\d{1,2})\b").unwrap());
static JUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:\d{3,4}p|blu[ -]?ray|web[ -]?(?:dl|rip)|hdtv|hevc|x26[45]|h\.?26[45]|complete|collection|multi[ -]?sub|eac3)\b").unwrap()
});
static EXTRAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:bloopers?|creditless|specials?|ova|oad|extras?|ncop|nced)\b").unwrap()
});

#[derive(Clone, Debug)]
pub struct Media {
    pub path: PathBuf,
    pub title: String,
    pub season: Option<u32>,
    pub episode: Option<f64>,
    pub extra: bool,
    pub label: String,
}

impl Media {
    pub fn group(&self) -> String {
        match self.season {
            Some(season) => format!("{} · Season {season}", self.title),
            None => self.title.clone(),
        }
    }
}

pub struct Library {
    pub groups: BTreeMap<String, Vec<Media>>,
    pub warnings: Vec<String>,
}

impl Library {
    pub fn scan(config: &Config) -> Self {
        let mut library = Self {
            groups: BTreeMap::new(),
            warnings: Vec::new(),
        };
        let mut seen = std::collections::HashSet::new();
        for root in &config.media_dirs {
            if !root.is_dir() {
                library
                    .warnings
                    .push(format!("Not a directory: {}", root.display()));
                continue;
            }
            for entry in WalkDir::new(root).into_iter().filter_entry(|entry| {
                entry.depth() == 0
                    || (!entry.file_name().to_string_lossy().starts_with('.')
                        && !entry.path().extension().is_some_and(|e| e == "tvlibrary"))
            }) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        library.warnings.push(error.to_string());
                        continue;
                    }
                };
                let path = entry.path();
                if !entry.file_type().is_file() || !is_video(path) {
                    continue;
                }
                let path = match path.canonicalize() {
                    Ok(path) => path,
                    Err(error) => {
                        library
                            .warnings
                            .push(format!("{}: {error}", path.display()));
                        continue;
                    }
                };
                if !seen.insert(path.clone()) {
                    continue;
                }
                let folder = entry.path().parent().filter(|parent| *parent != root);
                let mut media = parse(path, folder);
                if let Some(title) = config.titles.get(&media.title) {
                    media.title = title.clone();
                }
                library.groups.entry(media.group()).or_default().push(media);
            }
        }
        for items in library.groups.values_mut() {
            items.sort_by(|a, b| {
                a.extra
                    .cmp(&b.extra)
                    .then_with(|| {
                        a.episode
                            .unwrap_or(f64::MAX)
                            .total_cmp(&b.episode.unwrap_or(f64::MAX))
                    })
                    .then_with(|| a.path.cmp(&b.path))
            });
        }
        library
    }

    pub fn items(&self) -> impl Iterator<Item = &Media> {
        self.groups.values().flatten()
    }
}

fn is_video(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|ext| {
        matches!(
            ext.to_string_lossy().to_lowercase().as_str(),
            "mkv" | "mp4" | "m4v" | "avi" | "mov" | "webm" | "ts" | "m2ts" | "mpg" | "mpeg" | "wmv"
        )
    })
}

fn spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean(text: &str) -> String {
    let text = text.replace('_', " ");
    let text = BRACKETS.replace_all(&text, " ");
    let text = JUNK.replace_all(&text, " ");
    spaces(text.trim_matches(|c: char| c.is_whitespace() || c == '-' || c == '.'))
}

fn title_season(text: &str) -> (String, Option<u32>) {
    let season = SEASON.captures(text).and_then(|c| c[1].parse().ok());
    let text = SEASON.replace_all(text, " ");
    (clean(&text), season)
}

fn scene_episode(name: &str) -> Option<Captures<'_>> {
    STANDARD.captures(name).or_else(|| CROSS.captures(name))
}

pub fn parse(path: PathBuf, folder: Option<&std::path::Path>) -> Media {
    let raw = path.file_stem().unwrap_or_default().to_string_lossy();
    // Preserve decimal episode numbers; dots in conventional scene names are separators.
    let mut normalized = BRACKETS.replace_all(&raw, " ").replace('_', " ");
    if normalized.contains('.') && !ANIME.is_match(&normalized) {
        let mut spaced = normalized.replace('.', " ");
        if let Some(episode) = scene_episode(&normalized).and_then(|captures| captures.get(3)) {
            spaced.replace_range(episode.range(), episode.as_str());
        }
        normalized = spaced;
    }
    let folder_name = folder.and_then(|p| p.file_name()).map(|s| {
        let text = s.to_string_lossy();
        let text = if text.starts_with('[') {
            text.split_once(']').map_or(text.as_ref(), |(_, rest)| rest)
        } else {
            text.as_ref()
        };
        clean(&text.replace(['[', ']'], " "))
    });
    let folder_parts = folder_name.as_deref().map(title_season);
    let (mut title, mut season, episode) = if let Some(c) = scene_episode(&normalized) {
        (clean(&c[1]), c[2].parse().ok(), c[3].parse().ok())
    } else if let Some(c) = ANIME.captures(&normalized) {
        let (title, season) = title_season(&c[1]);
        (title, season, c[2].parse().ok())
    } else {
        let (title, season) = title_season(&normalized);
        (title, season, None)
    };
    let extra = EXTRAS.is_match(&normalized) || episode.is_some_and(|n: f64| n.fract() != 0.0);
    if let Some((folder_title, folder_season)) = folder_parts {
        season = season.or(folder_season);
        if (extra || title.is_empty()) && !folder_title.is_empty() {
            title = folder_title;
            season = folder_season.or(season);
        }
    }
    if title.is_empty() {
        title = raw.into_owned();
    }
    let label = if extra {
        clean(&normalized)
    } else if let Some(episode) = episode {
        format!("Episode {episode:02}")
    } else {
        clean(&normalized)
    };
    Media {
        path,
        title,
        season,
        episode,
        extra,
        label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_real_anime_releases_and_extras() {
        let folder =
            Path::new("[Saizen]_Kuroko's_Basketball_[Complete_1080p_S1_Blu-Ray_Collection]");
        let regular = parse(
            PathBuf::from("[Saizen]_Kuroko's_Basketball_-_03v2_[1080p][Blu-Ray][52D92AB8].mkv"),
            Some(folder),
        );
        assert_eq!(regular.title, "Kuroko's Basketball");
        assert_eq!(regular.season, Some(1));
        assert_eq!(regular.episode, Some(3.0));
        assert!(!regular.extra);
        let ova = parse(
            PathBuf::from("[Saizen]_Kuroko's_Basketball_-_22.5_Tip_Off_OVA_[1080p].mkv"),
            Some(folder),
        );
        assert_eq!(ova.episode, Some(22.5));
        assert!(ova.extra);
        let blooper = parse(
            PathBuf::from("[Saizen]_Kuroko's_Basketball_Bloopers_S1_Vol_5_[1080p].mkv"),
            Some(folder),
        );
        assert!(blooper.extra);
        assert_eq!(blooper.group(), regular.group());
    }

    #[test]
    fn parses_scene_names_and_loose_files() {
        let tv = parse(PathBuf::from("Some.Show.S02E10.1080p.mkv"), None);
        assert_eq!(
            (tv.title.as_str(), tv.season, tv.episode),
            ("Some Show", Some(2), Some(10.0))
        );
        for (file, label) in [
            ("Some.Show.S02E10.5.1080p.mkv", "Some Show S02E10.5"),
            ("Some.Show.S02.E10.5.1080p.mkv", "Some Show S02 E10.5"),
            ("Some.Show.2x10.5.1080p.mkv", "Some Show 2x10.5"),
        ] {
            let special = parse(PathBuf::from(file), None);
            assert_eq!(special.group(), tv.group());
            assert_eq!(special.episode, Some(10.5));
            assert!(special.extra);
            assert_eq!(special.label, label);
        }
        let anime = parse(
            PathBuf::from(
                "[Erai-raws] Ao no Hako Season 2 - 06 [1080p NF WEBRip HEVC EAC3][MultiSub].mkv",
            ),
            None,
        );
        assert_eq!(
            (anime.title.as_str(), anime.season, anime.episode),
            ("Ao no Hako", Some(2), Some(6.0))
        );
        let movie = parse(PathBuf::from("2001 A Space Odyssey (1968).mkv"), None);
        assert_eq!(movie.episode, None);
        assert_eq!(movie.title, "2001 A Space Odyssey (1968)");
    }

    #[test]
    fn scans_nested_roots_once_and_ignores_non_media() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Show")).unwrap();
        for file in [
            "Show/Show - 10.mkv",
            "Show/Show - 02.mkv",
            "Show/readme.txt",
            "Show/.hidden.mp4",
        ] {
            std::fs::write(dir.path().join(file), []).unwrap();
        }
        let config = Config {
            media_dirs: vec![dir.path().to_owned(), dir.path().join("Show")],
            ..Config::default()
        };
        let library = Library::scan(&config);
        let items = library.groups.get("Show").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].episode, Some(2.0));
        assert_eq!(items[1].episode, Some(10.0));
    }
}
