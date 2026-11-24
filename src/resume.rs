use crate::{
    db::Progress,
    library::{Library, Media},
};
use std::{cmp::Ordering, collections::HashMap, path::PathBuf};

pub fn candidates<'a>(
    library: &'a Library,
    history: &HashMap<PathBuf, Progress>,
) -> Vec<&'a Media> {
    let mut series: HashMap<&str, Vec<&Media>> = HashMap::new();
    let mut candidates = Vec::new();
    for item in library.items() {
        if item.regular_episode().is_some() {
            series.entry(&item.title).or_default().push(item);
        } else if let Some(progress) = history.get(&item.path)
            && !progress.watched
            && progress.position > 0.0
        {
            candidates.push((true, progress.updated, item));
        }
    }

    for episodes in series.into_values() {
        let most_recent = |watched| {
            episodes
                .iter()
                .copied()
                .filter_map(|item| {
                    let progress = history.get(&item.path)?;
                    (progress.watched == watched && (watched || progress.position > 0.0))
                        .then_some((item, progress.updated))
                })
                .max_by(|(a, a_updated), (b, b_updated)| {
                    a_updated.cmp(b_updated).then_with(|| episode_order(a, b))
                })
        };
        if let Some((item, updated)) = most_recent(false) {
            candidates.push((true, updated, item));
        } else if let Some((item, updated)) = most_recent(true)
            && let Some(next) = next_after(episodes.into_iter(), item, history)
        {
            candidates.push((false, updated, next));
        }
    }

    candidates.sort_unstable_by(|(a_partial, a_updated, a), (b_partial, b_updated, b)| {
        b_partial
            .cmp(a_partial)
            .then_with(|| b_updated.cmp(a_updated))
            .then_with(|| a.path.cmp(&b.path))
    });
    candidates.into_iter().map(|(_, _, item)| item).collect()
}

pub fn next_episode<'a>(
    library: &'a Library,
    current: &Media,
    history: &HashMap<PathBuf, Progress>,
) -> Option<&'a Media> {
    let series = library.items().filter(|item| item.title == current.title);
    next_after(series, current, history)
}

fn next_after<'a>(
    series: impl Iterator<Item = &'a Media>,
    current: &Media,
    history: &HashMap<PathBuf, Progress>,
) -> Option<&'a Media> {
    let current_episode = current.regular_episode()?;
    series
        .filter(|item| {
            item.regular_episode().is_some_and(|episode| {
                if item.season == current.season {
                    episode > current_episode
                } else {
                    matches!((item.season, current.season), (Some(a), Some(b)) if a > b)
                }
            }) && !history
                .get(&item.path)
                .is_some_and(|progress| progress.watched)
        })
        .min_by(|a, b| episode_order(a, b))
}

fn episode_order(a: &Media, b: &Media) -> Ordering {
    a.season
        .cmp(&b.season)
        .then_with(|| {
            a.episode
                .unwrap_or_default()
                .total_cmp(&b.episode.unwrap_or_default())
        })
        .then_with(|| a.path.cmp(&b.path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumes_one_regular_episode_per_series_and_skips_finished_movies() {
        let [
            first,
            second,
            third,
            next_season,
            later_season,
            extra,
            other,
            other_next,
            movie,
        ] = [
            "Show.S01E01.mkv",
            "Show.S01E02.mkv",
            "Show.S01E03.mkv",
            "Show.S02E01.mkv",
            "Show.S10E01.mkv",
            "Show.S01E03.5.mkv",
            "Other - 01.mkv",
            "Other - 02.mkv",
            "Movie.mkv",
        ]
        .map(|name| crate::library::parse(PathBuf::from(name), None));
        let mut library = Library {
            groups: Default::default(),
            warnings: vec![],
        };
        for media in [
            &first,
            &second,
            &third,
            &next_season,
            &later_season,
            &extra,
            &other,
            &other_next,
            &movie,
        ] {
            library
                .groups
                .entry(media.group())
                .or_default()
                .push(media.clone());
        }
        let mut history: HashMap<_, _> = [
            (&first, false, 1),
            (&second, false, 2),
            (&third, true, 5),
            (&other, true, 4),
            (&movie, true, 6),
        ]
        .into_iter()
        .map(|(media, watched, updated)| {
            (
                media.path.clone(),
                Progress {
                    watched,
                    updated,
                    position: 60.0,
                    ..Progress::default()
                },
            )
        })
        .collect();
        let continuing_paths = |history: &HashMap<PathBuf, Progress>| {
            candidates(&library, history)
                .into_iter()
                .map(|item| item.path.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            continuing_paths(&history),
            [second.path.clone(), other_next.path.clone()]
        );

        for episode in [&first, &second] {
            history.get_mut(&episode.path).unwrap().watched = true;
        }
        assert_eq!(
            continuing_paths(&history),
            [next_season.path.clone(), other_next.path]
        );
        assert_eq!(
            next_episode(&library, &third, &history).unwrap().path,
            next_season.path
        );
        assert!(next_episode(&library, &extra, &history).is_none());
        assert!(next_episode(&library, &later_season, &history).is_none());
    }
}
