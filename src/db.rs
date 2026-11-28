use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, backup::Backup, params};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

/// Version 1 adds the file size and content fingerprint used to follow moved files.
const SCHEMA_VERSION: i64 = 1;
/// Bytes hashed from the start, middle, and end of a file to fingerprint its content.
const FINGERPRINT_SPAN: u64 = 64 * 1024;
/// File size and content fingerprint, which together identify a video wherever it is stored.
type Key = (i64, i64);
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

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
        connection.busy_timeout(BUSY_TIMEOUT)?;
        let mut store = Self(connection);
        // Refuse newer history before changing anything about the file, even its journal mode.
        store.check_version(store.version()?, path)?;
        store.0.execute_batch("PRAGMA journal_mode=WAL;")?;
        store.migrate(path)?;
        Ok(store)
    }

    fn check_version(&self, version: i64, path: &Path) -> Result<()> {
        if version > SCHEMA_VERSION {
            bail!(
                "History at {} was written by a newer version of wvd. Update wvd to use it",
                path.display()
            );
        }
        Ok(())
    }

    fn migrate(&mut self, path: &Path) -> Result<()> {
        if self.version()? == SCHEMA_VERSION {
            return Ok(());
        }
        // Holding the write lock keeps other wvd processes from saving between the backup and
        // the schema change, and from migrating at the same time.
        let transaction = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        if version > SCHEMA_VERSION {
            bail!(
                "History at {} was written by a newer version of wvd. Update wvd to use it",
                path.display()
            );
        }
        let has_history: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='progress')",
            [],
            |row| row.get(0),
        )?;
        if has_history {
            backup(path, version)?;
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS progress (
                 path BLOB PRIMARY KEY,
                 position REAL NOT NULL,
                 duration REAL NOT NULL,
                 watched INTEGER NOT NULL,
                 updated INTEGER NOT NULL
             );",
        )?;
        let columns = transaction
            .prepare("SELECT name FROM pragma_table_info('progress')")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        for column in ["size", "fingerprint"] {
            if !columns.contains(column) {
                transaction
                    .execute_batch(&format!("ALTER TABLE progress ADD COLUMN {column} INTEGER"))?;
            }
        }
        transaction.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        transaction.commit()?;
        Ok(())
    }

    fn version(&self) -> Result<i64> {
        Ok(self
            .0
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Copy history to library files that were moved or renamed.
    ///
    /// A file inherits history from an entry whose path no longer exists when their sizes and
    /// content fingerprints match, or from an entry whose path is another spelling of the same
    /// file, as after a case-only rename. Old entries stay in place so a video on a disconnected
    /// drive keeps its history when the drive returns. An existing entry is replaced only by
    /// newer history. Returns the number of files restored.
    ///
    /// Files are read before the database is locked so slow drives never block playback saves,
    /// and every write is checked again against what was read.
    pub fn reattach<'a>(&mut self, library: impl Iterator<Item = &'a Path>) -> Result<usize> {
        let present: HashSet<&Path> = library.collect();
        let rows = self.identities()?;
        let known: HashMap<&Path, &IdentityRow> =
            rows.iter().map(|row| (row.path.as_path(), row)).collect();
        let mut backfill = Vec::new();
        let mut orphans: HashMap<Key, &IdentityRow> = HashMap::new();
        let mut aliases: HashMap<PathBuf, Vec<&IdentityRow>> = HashMap::new();
        for row in &rows {
            // A file can disappear after the scan, so the disk decides, not the library. A
            // symlink left behind by a move points nowhere, which counts as missing too.
            let state = fs::symlink_metadata(&row.path).and_then(|metadata| {
                if metadata.is_symlink() {
                    fs::metadata(&row.path)
                } else {
                    Ok(metadata)
                }
            });
            // A file of another size at the old path means the video moved and something else
            // took its place. The entry stays, but its history can follow the video.
            if let Ok(metadata) = &state
                && row
                    .identity
                    .size
                    .is_some_and(|size| size != metadata.len() as i64)
            {
                offer(&mut orphans, row);
            }
            match state {
                // A file where a folder on the old path used to be also means it is gone.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    offer(&mut orphans, row)
                }
                Ok(_) if present.contains(row.path.as_path()) => {
                    // History saved before version 1. Rows with an identity are refreshed by
                    // playback saves, which read the file as it is then.
                    if row.identity.fingerprint.is_none()
                        && let Ok(key) = fingerprint(&row.path)
                    {
                        backfill.push((row.path.as_path(), key));
                    }
                }
                Ok(_) => {
                    // On case-insensitive volumes the old spelling still opens the renamed file.
                    if let Ok(canonical) = fs::canonicalize(&row.path)
                        && canonical != row.path
                        && present.contains(canonical.as_path())
                    {
                        aliases.entry(canonical).or_default().push(row);
                    }
                }
                Err(_) => {}
            }
        }

        let mut sizes: HashMap<i64, Vec<Key>> = HashMap::new();
        for &key in orphans.keys() {
            sizes.entry(key.0).or_default().push(key);
        }
        let mut restores = Vec::new();
        // With nothing to restore from, the library is not touched at all.
        let candidates = present
            .iter()
            .filter(|_| !sizes.is_empty() || !aliases.is_empty());
        for &path in candidates {
            let alias_rows = aliases.get(path).map_or(&[][..], Vec::as_slice);
            let destination = known.get(path);
            // Only files that could match are read.
            if alias_rows.is_empty() {
                let Some(size) = fs::metadata(path).ok().map(|m| m.len() as i64) else {
                    continue;
                };
                let Some(keys) = sizes.get(&size) else {
                    continue;
                };
                // A file whose stored identity is the only candidate of its size, with no newer
                // source, is not read again, so videos that moved long ago do not cost a read
                // on every scan. Another candidate of the same size could be this file now.
                if let Some(row) = destination
                    && let Some(stored) = row.identity.key()
                    && keys == &[stored]
                    && !orphans
                        .get(&stored)
                        .is_some_and(|source| newer(source, row))
                {
                    continue;
                }
            }
            // Read the file itself, since its stored identity may describe a file that used to
            // be at this path.
            let Ok(key) = fingerprint(path) else {
                continue;
            };
            let mut source = orphans.get(&key).copied();
            for &row in alias_rows {
                // A symlink can also make an old path open a different video, so the content
                // must match. History saved before version 1 has no fingerprint to compare, so
                // it is trusted only when the spelling differs by case alone.
                let same = match row.identity.key() {
                    Some(stored) => stored == key,
                    None => differs_only_by_case(&row.path, path),
                };
                if same && source.is_none_or(|best| ranks_above(row, best)) {
                    source = Some(row);
                }
            }
            let Some(source) = source else {
                continue;
            };
            let destination = destination.map(|row| row.progress.updated);
            if destination.is_none_or(|updated| source.progress.updated > updated) {
                restores.push((path, source, key, destination));
            }
        }
        if backfill.is_empty() && restores.is_empty() {
            return Ok(0);
        }

        let transaction = self.0.transaction()?;
        for (path, (size, fingerprint)) in backfill {
            // Another process may have saved, and fingerprinted the file, since it was read.
            transaction.execute(
                "UPDATE progress SET size=?2, fingerprint=?3 WHERE path=?1 AND fingerprint IS NULL",
                params![path.as_os_str().as_bytes(), size, fingerprint],
            )?;
        }
        let mut restored = 0;
        for (path, source, (size, fingerprint), destination) in restores {
            // Replace an existing entry only if nothing saved to it since it was read.
            restored += transaction.execute(
                "INSERT INTO progress
                     (path, position, duration, watched, updated, size, fingerprint)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET
                     position=excluded.position,
                     duration=excluded.duration,
                     watched=excluded.watched,
                     updated=excluded.updated,
                     size=excluded.size,
                     fingerprint=excluded.fingerprint
                 WHERE excluded.updated > progress.updated AND progress.updated IS ?8",
                params![
                    path.as_os_str().as_bytes(),
                    source.progress.position,
                    source.progress.duration,
                    source.progress.watched,
                    source.progress.updated,
                    size,
                    fingerprint,
                    destination,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(restored)
    }

    fn identities(&self) -> Result<Vec<IdentityRow>> {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let mut statement = self.0.prepare(
            "SELECT path, position, duration, watched, updated, size, fingerprint FROM progress",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(IdentityRow {
                path: PathBuf::from(OsString::from_vec(row.get(0)?)),
                progress: Progress {
                    position: row.get(1)?,
                    duration: row.get(2)?,
                    watched: row.get(3)?,
                    updated: row.get(4)?,
                },
                identity: Identity {
                    size: row.get(5)?,
                    fingerprint: row.get(6)?,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        self.write_progress(path, progress, Identity::of(path), false)
    }

    /// Save playback progress, keeping the watched status.
    ///
    /// `identity` is read once when playback starts, so a checkpoint never describes a
    /// different file that took the path while the video was playing.
    pub fn save_playback(
        &self,
        path: &Path,
        progress: &Progress,
        identity: Identity,
    ) -> Result<()> {
        self.write_progress(path, progress, identity, true)
    }

    pub fn mark_watched_through<'a>(
        &mut self,
        paths: impl Iterator<Item = &'a Path>,
        through: &Path,
    ) -> Result<usize> {
        let updated = now();
        let paths = paths
            .map(|path| (path, Identity::of(path)))
            .collect::<Vec<_>>();
        let transaction = self.0.transaction()?;
        let changed = {
            let mut statement = transaction.prepare(
                "INSERT INTO progress (path, position, duration, watched, updated, size, fingerprint)
                 VALUES (?1, 0, 0, 1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET watched=1, updated=excluded.updated
                 WHERE NOT progress.watched",
            )?;
            // Refresh identity even for videos already watched, without counting them as changed.
            let mut refresh = transaction.prepare(
                "UPDATE progress SET size=?2, fingerprint=?3 WHERE path=?1 AND ?2 IS NOT NULL",
            )?;
            let mut changed = 0;
            for (path, identity) in paths {
                refresh.execute(params![
                    path.as_os_str().as_bytes(),
                    identity.size,
                    identity.fingerprint,
                ])?;
                changed += statement.execute(params![
                    path.as_os_str().as_bytes(),
                    updated,
                    identity.size,
                    identity.fingerprint,
                ])?;
            }
            changed
        };
        if changed > 0 {
            // Continue from the selected episode even if it was already watched.
            transaction.execute(
                "UPDATE progress SET updated=?2 WHERE path=?1",
                params![through.as_os_str().as_bytes(), updated],
            )?;
        }
        transaction.commit()?;
        Ok(changed)
    }

    fn write_progress(
        &self,
        path: &Path,
        progress: &Progress,
        identity: Identity,
        preserve_watched: bool,
    ) -> Result<()> {
        // Keep the stored identity when the file cannot be read, such as on an unmounted drive.
        self.0.execute(
            "INSERT INTO progress (path, position, duration, watched, updated, size, fingerprint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET
                 position=excluded.position,
                 duration=excluded.duration,
                 watched=excluded.watched OR (?6 AND progress.watched),
                 updated=excluded.updated,
                 size=COALESCE(excluded.size, progress.size),
                 fingerprint=CASE WHEN excluded.size IS NULL
                     THEN progress.fingerprint ELSE excluded.fingerprint END",
            params![
                path.as_os_str().as_bytes(),
                progress.position,
                progress.duration,
                progress.watched,
                progress.updated,
                preserve_watched,
                identity.size,
                identity.fingerprint,
            ],
        )?;
        Ok(())
    }
}

/// Size and content fingerprint of a file, used to recognize it after a move.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Identity {
    size: Option<i64>,
    fingerprint: Option<i64>,
}

impl Identity {
    /// Read a file's identity, or nothing when it cannot be read.
    pub fn of(path: &Path) -> Self {
        match fingerprint(path) {
            Ok((size, fingerprint)) => Self {
                size: Some(size),
                fingerprint: Some(fingerprint),
            },
            Err(_) => Self::default(),
        }
    }

    fn key(self) -> Option<Key> {
        Some((self.size?, self.fingerprint?))
    }
}

struct IdentityRow {
    path: PathBuf,
    progress: Progress,
    identity: Identity,
}

/// Make an entry a restore source for files with its content.
fn offer<'a>(sources: &mut HashMap<Key, &'a IdentityRow>, row: &'a IdentityRow) {
    // Every empty file looks the same, so emptiness says nothing.
    if let Some(key) = row.identity.key()
        && key.0 > 0
    {
        let best = sources.entry(key).or_insert(row);
        if ranks_above(row, best) {
            *best = row;
        }
    }
}

fn newer(a: &IdentityRow, b: &IdentityRow) -> bool {
    a.progress.updated > b.progress.updated
}

/// Order sources by most recent update, then by path so the choice is stable.
fn ranks_above(a: &IdentityRow, b: &IdentityRow) -> bool {
    (a.progress.updated, &a.path) > (b.progress.updated, &b.path)
}

fn differs_only_by_case(a: &Path, b: &Path) -> bool {
    let lower = |path: &Path| path.as_os_str().to_string_lossy().to_lowercase();
    a != b && lower(a) == lower(b)
}

/// Copy history to `<name>.v<version>.bak` before migrating it.
///
/// The caller holds the write lock, so the copy matches what is migrated. It replaces any
/// earlier backup, which may predate saves made after an interrupted migration. The copy is
/// written to a temporary file and renamed into place, so a backup is always complete.
fn backup(path: &Path, version: i64) -> Result<()> {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let backup = path.with_file_name(format!("{file_name}.v{version}.bak"));
    let partial = path.with_file_name(format!("{file_name}.v{version}.bak.tmp"));
    let _ = fs::remove_file(&partial);
    // History reveals what someone watches, so only its owner can read the copy.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)?;
    let context = || format!("Back up history to {}", backup.display());
    let copy = || -> Result<()> {
        let source = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        source.busy_timeout(BUSY_TIMEOUT)?;
        let mut copy = Connection::open_with_flags(
            &partial,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Backup::new(&source, &mut copy)?.run_to_completion(
            256,
            std::time::Duration::from_millis(10),
            None,
        )?;
        // A self-contained file, with no WAL that a reader of an older backup could reuse.
        let _: String = copy.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        Ok(())
    };
    copy().with_context(context)?;
    fs::rename(&partial, &backup).with_context(context)
}

/// File size and a 64-bit FNV-1a hash of 64 KiB from its start, middle, and end.
///
/// Both come from one open file so they describe the same version of it. The hash is stored in
/// the history database, so its format must stay the same across releases.
fn fingerprint(path: &Path) -> io::Result<Key> {
    // Opening a FIFO or device could block, so only regular files are read.
    let regular = |metadata: fs::Metadata| {
        if metadata.is_file() {
            Ok(metadata)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ))
        }
    };
    regular(fs::metadata(path)?)?;
    let file = File::open(path)?;
    let size = regular(file.metadata()?)?.len();
    let span = size.min(FINGERPRINT_SPAN);
    let mut buffer = vec![0; span as usize];
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for offset in [0, (size - span) / 2, size - span] {
        file.read_exact_at(&mut buffer, offset)?;
        for byte in &buffer {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Ok((size as i64, hash as i64))
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
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn persists_progress_and_preserves_watched_until_cleared() {
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
        store
            .save_playback(
                path,
                &Progress {
                    position: 12.0,
                    duration: 120.0,
                    ..Progress::default()
                },
                Identity::default(),
            )
            .unwrap();
        let saved = store.all().unwrap();
        assert!(saved[path].watched);
        assert_eq!(saved[path].position, 12.0);
        assert_eq!(saved.len(), 1);

        store.save(path, &Progress::default()).unwrap();
        assert!(!store.all().unwrap()[path].watched);
    }

    fn progress(position: f64, updated: i64) -> Progress {
        Progress {
            position,
            duration: 120.0,
            watched: false,
            updated,
        }
    }

    fn version(db: &Path) -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn migrates_version_zero_history_after_backing_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.sqlite3");
        let video = dir.path().join("episode.mkv");
        fs::write(&video, b"episode one").unwrap();
        Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "CREATE TABLE progress (
                     path BLOB PRIMARY KEY,
                     position REAL NOT NULL,
                     duration REAL NOT NULL,
                     watched INTEGER NOT NULL,
                     updated INTEGER NOT NULL
                 );
                 INSERT INTO progress VALUES (CAST('{}' AS BLOB), 42, 120, 0, 7);",
                video.display()
            ))
            .unwrap();

        let mut store = Store::open(&db).unwrap();
        assert_eq!(version(&db), SCHEMA_VERSION);
        let backup = dir.path().join("history.sqlite3.v0.bak");
        assert_eq!(version(&backup), 0);
        assert_eq!(
            Store(Connection::open(&backup).unwrap()).all().unwrap()[&video].position,
            42.0
        );
        assert_eq!(store.all().unwrap()[&video].position, 42.0);
        drop(store);
        store = Store::open(&db).unwrap();

        // History saved before fingerprints existed still follows its file after a move.
        assert_eq!(store.reattach([video.as_path()].into_iter()).unwrap(), 0);
        let moved = dir.path().join("renamed.mkv");
        fs::rename(&video, &moved).unwrap();
        assert_eq!(store.reattach([moved.as_path()].into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&moved].position, 42.0);
    }

    #[test]
    fn refuses_history_from_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.sqlite3");
        Connection::open(&db)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99")
            .unwrap();
        let error = Store::open(&db).err().unwrap().to_string();
        assert!(error.contains("newer version"), "{error}");
    }

    #[test]
    fn copies_history_to_moved_files_only_when_content_matches() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let old = dir.path().join("Show - 01.mkv");
        fs::write(&old, b"first episode").unwrap();
        store.save(&old, &progress(30.0, 1)).unwrap();

        // Same size, different content.
        let impostor = dir.path().join("impostor.mkv");
        fs::write(&impostor, b"other episode").unwrap();
        // Same content while the original still exists.
        let copy = dir.path().join("copy.mkv");
        fs::copy(&old, &copy).unwrap();
        let paths = [old.as_path(), impostor.as_path(), copy.as_path()];
        assert_eq!(store.reattach(paths.into_iter()).unwrap(), 0);

        fs::create_dir(dir.path().join("Show")).unwrap();
        let moved = dir.path().join("Show/Show S01E01.mkv");
        fs::rename(&old, &moved).unwrap();
        fs::remove_file(&copy).unwrap();
        let paths = [moved.as_path(), impostor.as_path()];
        assert_eq!(store.reattach(paths.into_iter()).unwrap(), 1);
        let history = store.all().unwrap();
        assert_eq!(history[&moved].position, 30.0);
        assert_eq!(history[&moved].updated, 1);
        assert!(!history.contains_key(&impostor));
        // The old entry stays in case its path returns.
        assert!(history.contains_key(&old));
        assert_eq!(store.reattach(paths.into_iter()).unwrap(), 0);
    }

    #[test]
    fn restores_the_newest_history_when_content_moved_more_than_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let [first, second, third] = ["a.mkv", "b.mkv", "c.mkv"].map(|name| dir.path().join(name));
        fs::write(&first, b"episode").unwrap();
        store.save(&first, &progress(10.0, 1)).unwrap();
        fs::rename(&first, &second).unwrap();
        assert_eq!(store.reattach([second.as_path()].into_iter()).unwrap(), 1);
        store.save(&second, &progress(50.0, 2)).unwrap();
        fs::rename(&second, &third).unwrap();
        assert_eq!(store.reattach([third.as_path()].into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&third].position, 50.0);
    }

    #[test]
    fn records_identity_when_marking_and_ignores_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let [watched, empty] = ["watched.mkv", "empty.mkv"].map(|name| dir.path().join(name));
        fs::write(&watched, b"watched episode").unwrap();
        fs::write(&empty, b"").unwrap();
        let paths = [watched.as_path(), empty.as_path()];
        assert_eq!(
            store
                .mark_watched_through(paths.into_iter(), &watched)
                .unwrap(),
            2
        );
        let [moved, moved_empty] =
            ["moved.mkv", "moved-empty.mkv"].map(|name| dir.path().join(name));
        fs::rename(&watched, &moved).unwrap();
        fs::rename(&empty, &moved_empty).unwrap();
        let paths = [moved.as_path(), moved_empty.as_path()];
        assert_eq!(store.reattach(paths.into_iter()).unwrap(), 1);
        assert!(store.all().unwrap()[&moved].watched);
    }

    #[test]
    fn checks_the_file_now_at_a_path_before_restoring_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let [reused, gone] = ["reused.mkv", "gone.mkv"].map(|name| dir.path().join(name));
        fs::write(&reused, b"aaaa").unwrap();
        store.save(&reused, &progress(10.0, 1)).unwrap();
        // A copy of the same video with newer history disappears.
        fs::write(&gone, b"aaaa").unwrap();
        store.save(&gone, &progress(90.0, 5)).unwrap();
        fs::remove_file(&gone).unwrap();
        // A different video of the same size now sits at the reused path.
        fs::write(&reused, b"cccc").unwrap();
        assert_eq!(store.reattach([reused.as_path()].into_iter()).unwrap(), 0);
        assert_eq!(store.all().unwrap()[&reused].position, 10.0);
    }

    #[test]
    fn ignores_old_paths_that_now_link_to_other_videos() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let mut store = Store::open(&dir.join("history.sqlite3")).unwrap();
        let [watched, other] = ["watched.mkv", "other.mkv"].map(|name| dir.join(name));
        fs::write(&watched, b"watched episode").unwrap();
        store.save(&watched, &progress(30.0, 1)).unwrap();
        fs::write(&other, b"another episode").unwrap();
        fs::remove_file(&watched).unwrap();
        std::os::unix::fs::symlink(&other, &watched).unwrap();
        assert_eq!(store.reattach([other.as_path()].into_iter()).unwrap(), 0);
        assert!(!store.all().unwrap().contains_key(&other));
    }

    #[test]
    fn restores_past_a_dangling_symlink_left_at_the_old_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let [old, new] = ["old.mkv", "new.mkv"].map(|name| dir.path().join(name));
        fs::write(&old, b"episode").unwrap();
        store.save(&old, &progress(35.0, 1)).unwrap();
        fs::rename(&old, &new).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere.mkv"), &old).unwrap();
        assert_eq!(store.reattach([new.as_path()].into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&new].position, 35.0);
    }

    #[test]
    fn playback_saves_the_identity_read_when_playback_started() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let path = dir.path().join("episode.mkv");
        fs::write(&path, b"the video being played").unwrap();
        let playing = Identity::of(&path);
        // Another file takes the path while the original keeps playing.
        fs::write(&path, b"a replacement").unwrap();
        store
            .save_playback(&path, &progress(12.0, 1), playing)
            .unwrap();
        assert_eq!(store.identities().unwrap()[0].identity, playing);
    }

    #[test]
    fn restores_when_a_different_video_takes_the_old_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let [old, moved] = ["old.mkv", "moved.mkv"].map(|name| dir.path().join(name));
        fs::write(&old, b"episode one").unwrap();
        store.save(&old, &progress(42.0, 1)).unwrap();
        fs::rename(&old, &moved).unwrap();
        fs::write(&old, b"a longer, different video").unwrap();
        let library = [old.as_path(), moved.as_path()];
        assert_eq!(store.reattach(library.into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&moved].position, 42.0);
    }

    #[test]
    fn restores_when_a_file_replaced_a_folder_on_the_old_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let folder = dir.path().join("Show");
        fs::create_dir(&folder).unwrap();
        let [old, moved] = [folder.join("episode.mkv"), dir.path().join("episode.mkv")];
        fs::write(&old, b"episode").unwrap();
        store.save(&old, &progress(20.0, 1)).unwrap();
        fs::rename(&old, &moved).unwrap();
        fs::remove_dir(&folder).unwrap();
        fs::write(&folder, b"not a folder").unwrap();
        assert_eq!(store.reattach([moved.as_path()].into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&moved].position, 20.0);
    }

    #[test]
    fn checks_files_whose_size_matches_another_moved_video() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let [a, b] = ["a.mkv", "b.mkv"].map(|name| dir.path().join(name));
        fs::write(&a, b"video a").unwrap();
        fs::write(&b, b"video b").unwrap();
        store.save(&a, &progress(10.0, 1)).unwrap();
        store.save(&b, &progress(50.0, 2)).unwrap();
        // B, the same size as A, is moved over A's path.
        fs::rename(&b, &a).unwrap();
        assert_eq!(store.reattach([a.as_path()].into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&a].position, 50.0);
    }

    #[test]
    fn restores_from_a_file_that_disappeared_after_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let [old, new] = ["old.mkv", "new.mkv"].map(|name| dir.path().join(name));
        fs::write(&old, b"episode").unwrap();
        store.save(&old, &progress(40.0, 1)).unwrap();
        fs::rename(&old, &new).unwrap();
        // The scan still listed the old path.
        let paths = [old.as_path(), new.as_path()];
        assert_eq!(store.reattach(paths.into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&new].position, 40.0);
    }

    #[test]
    fn never_reads_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.mkv");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        // Opening a FIFO with no writer would block forever.
        assert_eq!(
            fingerprint(&fifo).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(Identity::of(&fifo), Identity::default());
    }

    #[test]
    fn fingerprints_keep_their_stored_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("video.mkv");
        // Reference values computed independently; changing them orphans saved fingerprints.
        fs::write(&path, b"wvd").unwrap();
        assert_eq!(fingerprint(&path).unwrap(), (3, 909061652449369646));
        let mut content: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &content).unwrap();
        let original = fingerprint(&path).unwrap();
        assert_eq!(original, (204800, 5617238105452960501));
        for offset in [0, content.len() / 2, content.len() - 1] {
            content[offset] ^= 1;
            fs::write(&path, &content).unwrap();
            assert_ne!(fingerprint(&path).unwrap(), original, "{offset}");
            content[offset] ^= 1;
        }
    }

    #[test]
    fn replaces_earlier_backups_and_leaves_newer_schemas_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.sqlite3");
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE progress (
                     path BLOB PRIMARY KEY,
                     position REAL NOT NULL,
                     duration REAL NOT NULL,
                     watched INTEGER NOT NULL,
                     updated INTEGER NOT NULL
                 );
                 INSERT INTO progress VALUES (CAST('/m/a.mkv' AS BLOB), 42, 120, 0, 7);",
            )
            .unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        let partial = dir.path().join("history.sqlite3.v0.bak.tmp");
        fs::write(&partial, b"interrupted").unwrap();
        // A backup from an interrupted migration may predate later saves, so it is replaced.
        fs::write(dir.path().join("history.sqlite3.v0.bak"), b"stale").unwrap();
        Store::open(&db).unwrap();
        assert!(!partial.exists());
        let backup_path = dir.path().join("history.sqlite3.v0.bak");
        let mode = fs::metadata(&backup_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let backup = Store(Connection::open(&backup_path).unwrap());
        let mode: String = backup
            .0
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        assert_eq!(backup.all().unwrap()[Path::new("/m/a.mkv")].position, 42.0);

        let newer = dir.path().join("newer.sqlite3");
        Connection::open(&newer)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99")
            .unwrap();
        assert!(Store::open(&newer).is_err());
        let mode: String = Connection::open(&newer)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }

    #[test]
    fn marking_watched_refreshes_identity_of_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let video = dir.path().join("download.mkv");
        fs::write(&video, b"partial download").unwrap();
        store.save(&video, &progress(5.0, 1)).unwrap();
        fs::write(&video, b"partial download, now complete").unwrap();
        store
            .mark_watched_through([video.as_path()].into_iter(), &video)
            .unwrap();
        let moved = dir.path().join("moved.mkv");
        fs::rename(&video, &moved).unwrap();
        assert_eq!(store.reattach([moved.as_path()].into_iter()).unwrap(), 1);
        assert!(store.all().unwrap()[&moved].watched);
    }

    #[test]
    fn follows_case_only_renames() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let old = dir.path().canonicalize().unwrap().join("episode.mkv");
        fs::write(&old, b"episode").unwrap();
        store.save(&old, &progress(25.0, 1)).unwrap();
        // On case-insensitive volumes the old spelling still opens the file.
        let renamed = old.with_file_name("Episode.mkv");
        fs::rename(&old, &renamed).unwrap();
        let renamed = renamed.canonicalize().unwrap();
        assert_eq!(store.reattach([renamed.as_path()].into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&renamed].position, 25.0);
        assert_eq!(store.reattach([renamed.as_path()].into_iter()).unwrap(), 0);
    }

    #[test]
    fn newer_history_follows_a_video_moved_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let [home, away] = ["home.mkv", "away.mkv"].map(|name| dir.path().join(name));
        fs::write(&home, b"episode").unwrap();
        store.save(&home, &progress(10.0, 1)).unwrap();
        fs::rename(&home, &away).unwrap();
        assert_eq!(store.reattach([away.as_path()].into_iter()).unwrap(), 1);
        store.save(&away, &progress(50.0, 2)).unwrap();
        fs::rename(&away, &home).unwrap();
        assert_eq!(store.reattach([home.as_path()].into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&home].position, 50.0);
        // Equal or older history never replaces what is already there.
        assert_eq!(store.reattach([home.as_path()].into_iter()).unwrap(), 0);
    }
}
