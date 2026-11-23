use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::{
    collections::HashMap,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub position: f64,
    pub duration: f64,
    pub watched: bool,
    pub updated: i64,
}

impl Progress {
    pub fn status(&self) -> String {
        if self.watched {
            "Watched".to_owned()
        } else if self.position > 0.0 {
            format!("Resume {}", time(self.position))
        } else {
            "Unwatched".to_owned()
        }
    }
}

pub fn time(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

pub struct Store(Connection);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("Open history database: {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(3))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS progress (
                 path BLOB PRIMARY KEY,
                 position REAL NOT NULL,
                 duration REAL NOT NULL,
                 watched INTEGER NOT NULL,
                 updated INTEGER NOT NULL
             );",
        )?;
        Ok(Self(connection))
    }

    pub fn all(&self) -> Result<HashMap<PathBuf, Progress>> {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let mut statement = self
            .0
            .prepare("SELECT path, position, duration, watched, updated FROM progress")?;
        let rows = statement.query_map([], |row| {
            Ok((
                PathBuf::from(OsString::from_vec(row.get(0)?)),
                Progress {
                    position: row.get(1)?,
                    duration: row.get(2)?,
                    watched: row.get(3)?,
                    updated: row.get(4)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save(&self, path: &Path, progress: &Progress) -> Result<()> {
        self.0.execute(
            "INSERT INTO progress (path, position, duration, watched, updated)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET position=excluded.position,
             duration=excluded.duration, watched=excluded.watched, updated=excluded.updated",
            params![
                path.as_os_str().as_bytes(),
                progress.position,
                progress.duration,
                progress.watched,
                progress.updated
            ],
        )?;
        Ok(())
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_progress_and_replaces_watched_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.sqlite3");
        let path = Path::new("/Movies/quote's episode.mkv");
        Store::open(&db)
            .unwrap()
            .save(
                path,
                &Progress {
                    position: 91.0,
                    duration: 120.0,
                    watched: false,
                    updated: 1,
                },
            )
            .unwrap();
        let store = Store::open(&db).unwrap();
        assert_eq!(store.all().unwrap()[path].position, 91.0);
        store
            .save(
                path,
                &Progress {
                    watched: true,
                    ..Progress::default()
                },
            )
            .unwrap();
        let saved = store.all().unwrap();
        assert!(saved[path].watched);
        assert_eq!(saved.len(), 1);
    }
}
