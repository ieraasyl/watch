use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, backup::Backup,
    params,
};
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
/// Version 2 adds the `detached` table, so builds that do not know it refuse the file.
const SCHEMA_VERSION: i64 = 2;
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
        // History that could not be attached to a path, such as an imported entry whose path
        // now holds a different video. It is kept by content until wvd finds that video, and
        // removed once restored.
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS detached (
                 size INTEGER NOT NULL,
                 fingerprint INTEGER NOT NULL,
                 position REAL NOT NULL,
                 duration REAL NOT NULL,
                 watched INTEGER NOT NULL,
                 updated INTEGER NOT NULL,
                 PRIMARY KEY (size, fingerprint)
             );",
        )?;
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
        let detached = read_detached(&self.0)?;
        self.reattach_with(library, &detached)
    }

    /// `reattach` with extra entries that can only be restored by content, not by path.
    fn reattach_with<'a>(
        &mut self,
        library: impl Iterator<Item = &'a Path>,
        displaced: &[IdentityRow],
    ) -> Result<usize> {
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

        for row in displaced {
            offer(&mut orphans, row);
        }
        // The newest history known for each video, wherever it is. A restore takes this, so
        // an old detached entry never brings back progress a copy has since moved past.
        let mut latest: HashMap<Key, &IdentityRow> = HashMap::new();
        for row in rows.iter().chain(displaced) {
            offer(&mut latest, row);
        }
        let mut sizes: HashMap<i64, Vec<Key>> = HashMap::new();
        for &key in orphans.keys() {
            sizes.entry(key.0).or_default().push(key);
        }
        let mut restores = Vec::new();
        // Detached entries whose video already has their history.
        let mut satisfied: Vec<Key> = Vec::new();
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
                    && let Some(&pending) = orphans.get(&stored)
                {
                    let best = latest
                        .get(&stored)
                        .copied()
                        .filter(|&newest| ranks_above(newest, pending))
                        .unwrap_or(pending);
                    if !newer(best, row) {
                        if is_detached(pending) {
                            satisfied.push(stored);
                        }
                        continue;
                    }
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
            let Some(mut source) = source else {
                continue;
            };
            let from_detached = is_detached(source);
            if let Some(&newest) = latest.get(&key)
                && ranks_above(newest, source)
            {
                source = newest;
            }
            // An entry that describes a different video than the one now at its path is
            // stale. It is replaced whatever its age, and kept by content for its own video.
            let stale = destination
                .copied()
                .filter(|row| row.identity.key().is_some_and(|k| k != key));
            let updated = destination.map(|row| row.progress.updated);
            if stale.is_none()
                && let Some(updated) = updated
                && source.progress.updated <= updated
            {
                // The video already has this history or newer, so it needs nothing kept aside.
                if from_detached {
                    satisfied.push(key);
                }
            } else {
                restores.push(Restore {
                    path,
                    source,
                    key,
                    updated,
                    stale,
                    from_detached: from_detached.then_some(key),
                });
            }
        }
        if backfill.is_empty() && restores.is_empty() && satisfied.is_empty() {
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
        for restore in restores {
            if let Some(stale) = restore.stale {
                keep_detached(&transaction, stale)?;
            }
            let (size, fingerprint) = restore.key;
            // Replace an existing entry only if nothing saved to it since it was read.
            let changed = transaction.execute(
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
                 WHERE (?9 OR excluded.updated > progress.updated) AND progress.updated IS ?8",
                params![
                    restore.path.as_os_str().as_bytes(),
                    restore.source.progress.position,
                    restore.source.progress.duration,
                    restore.source.progress.watched,
                    restore.source.progress.updated,
                    size,
                    fingerprint,
                    restore.updated,
                    restore.stale.is_some(),
                ],
            )?;
            restored += changed;
            // Detached history has found its video, which now carries it.
            if changed > 0
                && let Some(key) = restore.from_detached
            {
                satisfied.push(key);
            }
        }
        for (size, fingerprint) in satisfied {
            // Checked under the lock: the entry goes only while a video with this identity holds
            // history at least as new, whatever other processes saved since the scan.
            transaction.execute(
                "DELETE FROM detached WHERE size=?1 AND fingerprint=?2 AND EXISTS (
                     SELECT 1 FROM progress
                     WHERE progress.size=?1 AND progress.fingerprint=?2
                         AND progress.updated >= detached.updated
                 )",
                params![size, fingerprint],
            )?;
        }
        transaction.commit()?;
        Ok(restored)
    }

    /// Write a consistent copy of the history database to a new file.
    ///
    /// Returns the number of entries in the copy. An existing file is never replaced.
    pub fn export(&self, destination: &Path) -> Result<usize> {
        let context = || format!("Export history to {}", destination.display());
        if fs::symlink_metadata(destination).is_ok() {
            bail!("{} already exists", destination.display());
        }
        let directory = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // A short random name in the same directory, created exclusively and removed on drop.
        let partial = tempfile::Builder::new()
            .prefix(".wvd-export-")
            .suffix(".tmp")
            .tempfile_in(directory)
            .with_context(context)?
            .into_temp_path();
        let count = self.write_export(&partial).with_context(context)?;
        // Publishing fails rather than replace a file that appeared meanwhile.
        partial
            .persist_noclobber(destination)
            .map_err(|error| match error.error.kind() {
                io::ErrorKind::AlreadyExists => {
                    anyhow::anyhow!("{} already exists", destination.display())
                }
                _ => anyhow::Error::from(error.error).context(context()),
            })?;
        Ok(count)
    }

    fn write_export(&self, partial: &Path) -> Result<usize> {
        // Without SQLITE_OPEN_URI, a name such as `file:x` is an ordinary file name.
        let mut copy = Connection::open_with_flags(
            partial,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Backup::new(&self.0, &mut copy)?.run_to_completion(
            256,
            std::time::Duration::from_millis(10),
            None,
        )?;
        // A self-contained file can be imported from read-only storage.
        let _: String = copy.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        let count: i64 = copy.query_row(
            "SELECT (SELECT COUNT(*) FROM progress) + (SELECT COUNT(*) FROM detached)",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Merge history saved by `export`, or a history database copied with SQLite's backup tools.
    ///
    /// An entry with a fingerprint attaches to its path only when the file there is the same
    /// video. Otherwise it is kept by content, and a later scan restores it once wvd finds that
    /// video. For the same video, the entry updated most recently wins. Local history that
    /// describes a different video than the one now at its path is kept by content as well.
    /// History saved before fingerprints existed can only be merged by path.
    pub fn import(&mut self, source: &Path) -> Result<Imported> {
        let (rows, source_detached) = read_export(source)
            .with_context(|| format!("Import history from {}", source.display()))?;
        let mut by_path = Vec::new();
        let mut verified = Vec::new();
        let mut detached = Vec::new();
        for row in rows {
            match row.identity.key() {
                None => by_path.push(row),
                // Files are read before locking, so slow drives never block playback saves.
                Some(key) if fingerprint(&row.path).is_ok_and(|on_disk| on_disk == key) => {
                    verified.push(row);
                }
                // Missing, different, or unreadable: only its own video may take it.
                Some(_) => detached.push(row),
            }
        }
        detached.extend(source_detached);

        let mut imported = Imported::default();
        // Lock first so a playback save cannot invalidate the read snapshot mid-merge.
        let transaction = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut exists = transaction.prepare("SELECT 1 FROM progress WHERE path=?1")?;
            // A replaced entry takes the imported identity. History without one gets a fresh
            // fingerprint from the file at its path on the next scan.
            let mut statement = transaction.prepare(
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
                 WHERE ?8 OR excluded.updated > progress.updated",
            )?;
            let entries = by_path
                .iter()
                .map(|row| (row, false))
                .chain(verified.iter().map(|row| (row, true)));
            for (row, verified) in entries {
                // Read under the lock, so a playback save made meanwhile is what is compared.
                let stale = verified && archive_if_replaced(&transaction, &row.path, row.identity)?;
                // History without a fingerprint cannot show which video it describes, so newer
                // history it replaces is kept by content in case that video has moved.
                if !verified
                    && let Some(local) = row_at(&transaction, &row.path)?
                    && local.identity.key().is_some()
                    && row.progress.updated > local.progress.updated
                {
                    keep_detached(&transaction, &local)?;
                }
                let path = row.path.as_os_str().as_bytes();
                let existed = exists.exists([path])?;
                let changed = statement.execute(params![
                    path,
                    row.progress.position,
                    row.progress.duration,
                    row.progress.watched,
                    row.progress.updated,
                    row.identity.size,
                    row.identity.fingerprint,
                    stale,
                ])?;
                match (changed, existed) {
                    (0, _) => imported.kept += 1,
                    (_, false) => imported.added += 1,
                    (_, true) => imported.updated += 1,
                }
            }
            for row in &detached {
                match keep_detached(&transaction, row)? {
                    0 => imported.kept += 1,
                    _ => imported.displaced += 1,
                }
            }
        }
        transaction.commit()?;
        Ok(imported)
    }

    fn identities(&self) -> Result<Vec<IdentityRow>> {
        self.identities_with("size, fingerprint")
    }

    fn identities_with(&self, identity: &str) -> Result<Vec<IdentityRow>> {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let mut statement = self.0.prepare(&format!(
            "SELECT path, position, duration, watched, updated, {identity} FROM progress"
        ))?;
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
        let transaction = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
                if archive_if_replaced(&transaction, path, identity)? {
                    // Nothing of the old video's progress carries over to this one.
                    transaction.execute(
                        "UPDATE progress SET position=0, duration=0, watched=0 WHERE path=?1",
                        [path.as_os_str().as_bytes()],
                    )?;
                }
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
        let transaction = Transaction::new_unchecked(&self.0, TransactionBehavior::Immediate)?;
        let replaced = archive_if_replaced(&transaction, path, identity)?;
        // Keep the stored identity when the file cannot be read, such as on an unmounted drive.
        // A different video at the path does not inherit the watched status of the old one.
        transaction.execute(
            "INSERT INTO progress (path, position, duration, watched, updated, size, fingerprint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET
                 position=excluded.position,
                 duration=excluded.duration,
                 watched=excluded.watched OR (?6 AND NOT ?9 AND progress.watched),
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
                replaced,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

impl Store {
    /// Whether the history stored for `path` describes a different video than `identity`.
    pub fn holds_other_video(&self, path: &Path, identity: Identity) -> Result<bool> {
        let Some(key) = identity.key() else {
            return Ok(false);
        };
        Ok(row_at(&self.0, path)?
            .and_then(|row| row.identity.key())
            .is_some_and(|stored| stored != key))
    }
}

/// The history entry stored for a path.
fn row_at(connection: &Connection, path: &Path) -> Result<Option<IdentityRow>> {
    Ok(connection
        .query_row(
            "SELECT position, duration, watched, updated, size, fingerprint
             FROM progress WHERE path=?1",
            [path.as_os_str().as_bytes()],
            |row| {
                Ok(IdentityRow {
                    path: path.to_owned(),
                    progress: Progress {
                        position: row.get(0)?,
                        duration: row.get(1)?,
                        watched: row.get(2)?,
                        updated: row.get(3)?,
                    },
                    identity: Identity {
                        size: row.get(4)?,
                        fingerprint: row.get(5)?,
                    },
                })
            },
        )
        .optional()?)
}

/// Before history for a new video is written at `path`, keep the history of the different
/// video stored there by content, so it is not lost if that video turns up elsewhere.
/// Returns whether the stored entry described a different video.
fn archive_if_replaced(connection: &Connection, path: &Path, identity: Identity) -> Result<bool> {
    let Some(new) = identity.key() else {
        return Ok(false);
    };
    match row_at(connection, path)? {
        Some(old) if old.identity.key().is_some_and(|key| key != new) => {
            keep_detached(connection, &old)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// How an import changed the history.
#[derive(Debug, Default)]
pub struct Imported {
    /// Entries for paths that had no history.
    pub added: usize,
    /// Entries that replaced older local history.
    pub updated: usize,
    /// Entries skipped because local history was as new or newer.
    pub kept: usize,
    /// Entries kept by content until wvd finds their videos.
    pub displaced: usize,
}

/// A planned restore of history to a library file.
struct Restore<'a> {
    path: &'a Path,
    source: &'a IdentityRow,
    /// Identity of the file at `path`, read during the scan.
    key: Key,
    /// When the entry at `path` was last updated, as read during the scan.
    updated: Option<i64>,
    /// The entry at `path`, if it describes a different video.
    stale: Option<&'a IdentityRow>,
    /// The detached entry this restore takes, which is then removed.
    from_detached: Option<Key>,
}

/// Keep history by content in the `detached` table, unless newer history is already there.
/// Returns the number of rows changed.
fn keep_detached(connection: &Connection, row: &IdentityRow) -> Result<usize> {
    let Some((size, fingerprint)) = row.identity.key().filter(|key| key.0 > 0) else {
        return Ok(0);
    };
    Ok(connection.execute(
        "INSERT INTO detached (size, fingerprint, position, duration, watched, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(size, fingerprint) DO UPDATE SET
             position=excluded.position,
             duration=excluded.duration,
             watched=excluded.watched,
             updated=excluded.updated
         WHERE excluded.updated > detached.updated",
        params![
            size,
            fingerprint,
            row.progress.position,
            row.progress.duration,
            row.progress.watched,
            row.progress.updated,
        ],
    )?)
}

/// History kept by content in the `detached` table, as restore sources without a path.
fn read_detached(connection: &Connection) -> Result<Vec<IdentityRow>> {
    let mut statement = connection
        .prepare("SELECT size, fingerprint, position, duration, watched, updated FROM detached")?;
    let rows = statement.query_map([], |row| {
        Ok(IdentityRow {
            path: PathBuf::new(),
            identity: Identity {
                size: row.get(0)?,
                fingerprint: row.get(1)?,
            },
            progress: Progress {
                position: row.get(2)?,
                duration: row.get(3)?,
                watched: row.get(4)?,
                updated: row.get(5)?,
            },
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Entries with paths, and entries kept by content, from an exported history file.
fn read_export(source: &Path) -> Result<(Vec<IdentityRow>, Vec<IdentityRow>)> {
    let connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        bail!("It was written by a newer version of wvd. Update wvd to import it");
    }
    let has_table: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='progress')",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        bail!("It is not a wvd history file");
    }
    let columns = connection
        .prepare("SELECT name FROM pragma_table_info('progress')")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    let required = ["path", "position", "duration", "watched", "updated"];
    if !required.iter().all(|column| columns.contains(*column)) {
        bail!("It is not a wvd history file");
    }
    // Version 0 history has no identity columns; it is fingerprinted once files are found.
    let identity = if columns.contains("size") && columns.contains("fingerprint") {
        "size, fingerprint"
    } else {
        "NULL, NULL"
    };
    let has_detached: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='detached')",
        [],
        |row| row.get(0),
    )?;
    let detached = if has_detached {
        read_detached(&connection)?
    } else {
        Vec::new()
    };
    Ok((Store(connection).identities_with(identity)?, detached))
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

#[derive(Debug)]
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

/// Whether a restore source comes from the `detached` table, which has no path.
fn is_detached(row: &IdentityRow) -> bool {
    row.path.as_os_str().is_empty()
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
        assert!(read_detached(&store.0).unwrap().is_empty());
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
    fn exports_and_merges_history_by_most_recent_update() {
        let dir = tempfile::tempdir().unwrap();
        let laptop = Store::open(&dir.path().join("laptop.sqlite3")).unwrap();
        let mut desktop = Store::open(&dir.path().join("desktop.sqlite3")).unwrap();
        let [shared, older, laptop_only, desktop_only] = [
            "/m/shared.mkv",
            "/m/older.mkv",
            "/m/laptop.mkv",
            "/m/desktop.mkv",
        ]
        .map(Path::new);
        laptop.save(shared, &progress(90.0, 20)).unwrap();
        laptop.save(older, &progress(10.0, 1)).unwrap();
        laptop.save(laptop_only, &progress(5.0, 3)).unwrap();
        desktop.save(shared, &progress(30.0, 10)).unwrap();
        desktop.save(older, &progress(70.0, 5)).unwrap();
        desktop.save(desktop_only, &progress(15.0, 4)).unwrap();

        let export = dir.path().join("laptop export.sqlite3");
        assert_eq!(laptop.export(&export).unwrap(), 3);
        assert!(
            laptop
                .export(&export)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        let imported = desktop.import(&export).unwrap();
        assert_eq!(
            (
                imported.added,
                imported.updated,
                imported.kept,
                imported.displaced
            ),
            (1, 1, 1, 0)
        );
        let merged = desktop.all().unwrap();
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[shared].position, 90.0);
        assert_eq!(merged[older].position, 70.0);
        assert_eq!(merged[laptop_only].position, 5.0);
        assert_eq!(merged[desktop_only].position, 15.0);
        // Importing the same file again changes nothing.
        assert_eq!(desktop.import(&export).unwrap().kept, 3);
    }

    #[test]
    fn exports_self_contained_files_that_import_from_read_only_folders() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        store
            .save(Path::new("/m/a.mkv"), &progress(1.0, 1))
            .unwrap();
        let shelf = dir.path().join("shelf");
        fs::create_dir(&shelf).unwrap();
        let export = shelf.join("export.sqlite3");
        assert_eq!(store.export(&export).unwrap(), 1);
        // Only the export itself is left behind, with no temporary or WAL files.
        let names: Vec<_> = fs::read_dir(&shelf)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["export.sqlite3"]);
        fs::set_permissions(&shelf, fs::Permissions::from_mode(0o500)).unwrap();
        let mut other = Store::open(&dir.path().join("other.sqlite3")).unwrap();
        let imported = other.import(&export);
        fs::set_permissions(&shelf, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(imported.unwrap().added, 1);
    }

    fn export_of(dir: &Path, name: &str, saves: &[(&Path, Progress)]) -> PathBuf {
        let store = Store::open(&dir.join(format!("{name}.sqlite3"))).unwrap();
        for (path, progress) in saves {
            store.save(path, progress).unwrap();
        }
        let export = dir.join(format!("{name} export.sqlite3"));
        store.export(&export).unwrap();
        export
    }

    #[test]
    fn keeps_history_for_a_different_video_at_its_path_until_found() {
        let dir = tempfile::tempdir().unwrap();
        let [path, moved] = ["Show - 01.mkv", "Elsewhere.mkv"].map(|n| dir.path().join(n));
        fs::write(&path, b"episode one").unwrap();
        let export = export_of(dir.path(), "old", &[(&path, progress(42.0, 1))]);
        // A different video took the path, and the episode is not in the library yet.
        fs::write(&path, b"episode two").unwrap();

        let db = dir.path().join("new.sqlite3");
        let mut store = Store::open(&db).unwrap();
        let imported = store.import(&export).unwrap();
        assert_eq!((imported.added, imported.displaced), (0, 1));
        assert!(store.all().unwrap().is_empty());
        assert_eq!(store.reattach([path.as_path()].into_iter()).unwrap(), 0);
        drop(store);

        // A later run finds the episode at its new path.
        fs::write(&moved, b"episode one").unwrap();
        let mut store = Store::open(&db).unwrap();
        let library = [path.as_path(), moved.as_path()];
        assert_eq!(store.reattach(library.into_iter()).unwrap(), 1);
        let history = store.all().unwrap();
        assert_eq!(history[&moved].position, 42.0);
        assert!(!history.contains_key(&path));
    }

    #[test]
    fn keeps_both_videos_when_two_databases_disagree_about_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.mkv");
        let [first, second] = ["first.mkv", "second.mkv"].map(|n| dir.path().join(n));
        fs::write(&shared, b"first video").unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        store.save(&shared, &progress(10.0, 1)).unwrap();
        fs::rename(&shared, &first).unwrap();
        fs::write(&shared, b"second video").unwrap();
        let export = export_of(dir.path(), "other", &[(&shared, progress(20.0, 2))]);
        fs::rename(&shared, &second).unwrap();

        assert_eq!(store.import(&export).unwrap().displaced, 1);
        let library = [first.as_path(), second.as_path()];
        assert_eq!(store.reattach(library.into_iter()).unwrap(), 2);
        let history = store.all().unwrap();
        assert_eq!(history[&first].position, 10.0);
        assert_eq!(history[&second].position, 20.0);
    }

    #[test]
    fn keeps_history_aside_when_the_video_at_its_path_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.mkv");
        fs::write(&path, b"episode one").unwrap();
        let export = export_of(dir.path(), "old", &[(&path, progress(42.0, 1))]);
        fs::write(&path, b"a different video").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let mut store = Store::open(&dir.path().join("new.sqlite3")).unwrap();
        let imported = store.import(&export).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(imported.displaced, 1);
        assert_eq!(store.reattach([path.as_path()].into_iter()).unwrap(), 0);
        assert!(store.all().unwrap().is_empty());
    }

    #[test]
    fn legacy_imports_drop_the_identity_they_replace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.mkv");
        fs::write(&path, b"first video").unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        store.save(&path, &progress(10.0, 1)).unwrap();
        let legacy = dir.path().join("legacy.sqlite3");
        Connection::open(&legacy)
            .unwrap()
            .execute_batch(&format!(
                "CREATE TABLE progress (
                     path BLOB PRIMARY KEY,
                     position REAL NOT NULL,
                     duration REAL NOT NULL,
                     watched INTEGER NOT NULL,
                     updated INTEGER NOT NULL
                 );
                 INSERT INTO progress VALUES (CAST('{}' AS BLOB), 99, 120, 1, 5);",
                path.display()
            ))
            .unwrap();
        // The path now holds a different video, which the legacy history describes.
        fs::write(&path, b"second video").unwrap();
        assert_eq!(store.import(&legacy).unwrap().updated, 1);
        assert_eq!(store.reattach([path.as_path()].into_iter()).unwrap(), 0);
        assert_eq!(store.identities().unwrap()[0].identity, Identity::of(&path));
    }

    #[test]
    fn exports_to_long_names_and_never_touches_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        let bystander = dir.path().join(format!(".long.{}.tmp", std::process::id()));
        fs::write(&bystander, "not ours").unwrap();
        let export = dir.path().join(format!("{}.sqlite3", "x".repeat(240)));
        assert_eq!(store.export(&export).unwrap(), 0);
        assert_eq!(fs::read_to_string(&bystander).unwrap(), "not ours");
    }

    #[test]
    fn imported_history_follows_files_to_new_paths() {
        let dir = tempfile::tempdir().unwrap();
        let old = Store::open(&dir.path().join("old.sqlite3")).unwrap();
        let video = dir.path().join("Old Machine/Show - 01.mkv");
        fs::create_dir(video.parent().unwrap()).unwrap();
        fs::write(&video, b"episode one").unwrap();
        old.save(&video, &progress(42.0, 1)).unwrap();
        let export = dir.path().join("export.sqlite3");
        old.export(&export).unwrap();

        let new_path = dir.path().join("Show - 01.mkv");
        fs::rename(&video, &new_path).unwrap();
        let mut new = Store::open(&dir.path().join("new.sqlite3")).unwrap();
        // The old path is gone here, so the entry waits by content for its video.
        assert_eq!(new.import(&export).unwrap().displaced, 1);
        assert_eq!(new.reattach([new_path.as_path()].into_iter()).unwrap(), 1);
        assert_eq!(new.all().unwrap()[&new_path].position, 42.0);
        assert!(read_detached(&new.0).unwrap().is_empty());
    }

    #[test]
    fn verified_imports_replace_history_for_a_different_video_at_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let [shared, elsewhere] = ["Shared.mkv", "Elsewhere.mkv"].map(|n| dir.path().join(n));
        fs::write(&shared, b"video a").unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        // Local history for video A is newer than the imported history for video B.
        store.save(&shared, &progress(12.0, 9)).unwrap();
        fs::write(&shared, b"video b").unwrap();
        let export = export_of(dir.path(), "other", &[(&shared, progress(38.0, 2))]);

        let imported = store.import(&export).unwrap();
        assert_eq!(imported.updated, 1);
        assert_eq!(store.all().unwrap()[&shared].position, 38.0);
        // Video A keeps its history and gets it back where it turns up.
        fs::write(&elsewhere, b"video a").unwrap();
        let library = [shared.as_path(), elsewhere.as_path()];
        assert_eq!(store.reattach(library.into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&elsewhere].position, 12.0);
    }

    #[test]
    fn imports_for_missing_paths_wait_for_their_own_video() {
        let dir = tempfile::tempdir().unwrap();
        let [path, elsewhere] = ["episode.mkv", "elsewhere.mkv"].map(|n| dir.path().join(n));
        fs::write(&path, b"episode one").unwrap();
        let export = export_of(dir.path(), "old", &[(&path, progress(42.0, 1))]);
        fs::remove_file(&path).unwrap();
        let mut store = Store::open(&dir.path().join("new.sqlite3")).unwrap();
        assert_eq!(store.import(&export).unwrap().displaced, 1);
        // An unrelated video takes the path; the episode turns up elsewhere.
        fs::write(&path, b"unrelated video").unwrap();
        fs::write(&elsewhere, b"episode one").unwrap();
        let library = [path.as_path(), elsewhere.as_path()];
        assert_eq!(store.reattach(library.into_iter()).unwrap(), 1);
        let history = store.all().unwrap();
        assert_eq!(history[&elsewhere].position, 42.0);
        assert!(!history.contains_key(&path));
    }

    #[test]
    fn restores_the_newest_progress_known_for_a_video() {
        let dir = tempfile::tempdir().unwrap();
        let [first, second] = ["first.mkv", "second.mkv"].map(|n| dir.path().join(n));
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        fs::write(&first, b"episode").unwrap();
        store.save(&first, &progress(100.0, 5)).unwrap();
        // An old export brings back older detached history for the same video.
        keep_detached(
            &store.0,
            &IdentityRow {
                path: PathBuf::new(),
                progress: progress(40.0, 1),
                identity: Identity::of(&first),
            },
        )
        .unwrap();
        fs::write(&second, b"episode").unwrap();
        let library = [first.as_path(), second.as_path()];
        assert_eq!(store.reattach(library.into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&second].position, 100.0);
        assert!(read_detached(&store.0).unwrap().is_empty());
    }

    #[test]
    fn saving_a_different_video_at_a_path_keeps_the_old_history() {
        let dir = tempfile::tempdir().unwrap();
        let [path, outside] = ["episode.mkv", "outside.mkv"].map(|n| dir.path().join(n));
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        fs::write(&path, b"video a").unwrap();
        store
            .save(
                &path,
                &Progress {
                    watched: true,
                    ..progress(10.0, 1)
                },
            )
            .unwrap();
        // A leaves the library and B is played at its old path.
        fs::rename(&path, &outside).unwrap();
        fs::write(&path, b"video b, longer").unwrap();
        store
            .save_playback(&path, &progress(5.0, 2), Identity::of(&path))
            .unwrap();
        assert!(!store.all().unwrap()[&path].watched);
        // A comes back at a new path with its history.
        let back = dir.path().join("back.mkv");
        fs::rename(&outside, &back).unwrap();
        let library = [path.as_path(), back.as_path()];
        assert_eq!(store.reattach(library.into_iter()).unwrap(), 1);
        assert!(store.all().unwrap()[&back].watched);
    }

    #[test]
    fn importing_twice_leaves_nothing_aside_for_restored_videos() {
        let dir = tempfile::tempdir().unwrap();
        let [path, moved] = ["episode.mkv", "moved.mkv"].map(|n| dir.path().join(n));
        fs::write(&path, b"episode").unwrap();
        let export = export_of(dir.path(), "old", &[(&path, progress(42.0, 1))]);
        fs::rename(&path, &moved).unwrap();
        let mut store = Store::open(&dir.path().join("new.sqlite3")).unwrap();
        for restored in [1, 0] {
            store.import(&export).unwrap();
            assert_eq!(
                store.reattach([moved.as_path()].into_iter()).unwrap(),
                restored
            );
            assert!(read_detached(&store.0).unwrap().is_empty());
        }
    }

    #[test]
    fn legacy_imports_keep_the_fingerprinted_history_they_replace() {
        let dir = tempfile::tempdir().unwrap();
        let [path, moved] = ["episode.mkv", "moved.mkv"].map(|n| dir.path().join(n));
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        fs::write(&path, b"video a").unwrap();
        store.save(&path, &progress(30.0, 1)).unwrap();
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"video b, longer").unwrap();
        let legacy = dir.path().join("legacy.sqlite3");
        Connection::open(&legacy)
            .unwrap()
            .execute_batch(&format!(
                "CREATE TABLE progress (
                     path BLOB PRIMARY KEY,
                     position REAL NOT NULL,
                     duration REAL NOT NULL,
                     watched INTEGER NOT NULL,
                     updated INTEGER NOT NULL
                 );
                 INSERT INTO progress VALUES (CAST('{}' AS BLOB), 99, 120, 1, 5);",
                path.display()
            ))
            .unwrap();
        assert_eq!(store.import(&legacy).unwrap().updated, 1);
        let library = [path.as_path(), moved.as_path()];
        assert_eq!(store.reattach(library.into_iter()).unwrap(), 1);
        assert_eq!(store.all().unwrap()[&moved].position, 30.0);
    }

    #[test]
    fn marking_a_different_video_watched_starts_it_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.mkv");
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        fs::write(&path, b"video a").unwrap();
        store.save(&path, &progress(42.0, 1)).unwrap();
        fs::write(&path, b"video b, longer").unwrap();
        assert!(store.holds_other_video(&path, Identity::of(&path)).unwrap());
        assert_eq!(
            store
                .mark_watched_through([path.as_path()].into_iter(), &path)
                .unwrap(),
            1
        );
        let row = &store.all().unwrap()[&path];
        assert!(row.watched);
        assert_eq!((row.position, row.duration), (0.0, 0.0));
        assert!(!store.holds_other_video(&path, Identity::of(&path)).unwrap());
    }

    #[test]
    fn counts_detached_history_in_exports_and_imports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.mkv");
        fs::write(&path, b"episode").unwrap();
        let source = Store::open(&dir.path().join("source.sqlite3")).unwrap();
        keep_detached(
            &source.0,
            &IdentityRow {
                path: PathBuf::new(),
                progress: progress(40.0, 1),
                identity: Identity::of(&path),
            },
        )
        .unwrap();
        let export = dir.path().join("export.sqlite3");
        assert_eq!(source.export(&export).unwrap(), 1);
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        assert_eq!(store.import(&export).unwrap().displaced, 1);
        assert_eq!(store.import(&export).unwrap().kept, 1);
    }

    #[test]
    fn imports_version_zero_history_and_rejects_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy.sqlite3");
        Connection::open(&legacy)
            .unwrap()
            .execute_batch(
                "CREATE TABLE progress (
                     path BLOB PRIMARY KEY,
                     position REAL NOT NULL,
                     duration REAL NOT NULL,
                     watched INTEGER NOT NULL,
                     updated INTEGER NOT NULL
                 );
                 INSERT INTO progress VALUES (CAST('/m/legacy.mkv' AS BLOB), 12, 120, 1, 2);",
            )
            .unwrap();
        let mut store = Store::open(&dir.path().join("history.sqlite3")).unwrap();
        assert_eq!(store.import(&legacy).unwrap().added, 1);
        assert!(store.all().unwrap()[Path::new("/m/legacy.mkv")].watched);
        // The source is opened read-only and left as it was.
        assert_eq!(version(&legacy), 0);

        let other = dir.path().join("other.sqlite3");
        Connection::open(&other)
            .unwrap()
            .execute_batch("CREATE TABLE notes (body TEXT)")
            .unwrap();
        let view = dir.path().join("view.sqlite3");
        Connection::open(&view)
            .unwrap()
            .execute_batch(
                "CREATE VIEW progress AS SELECT
                     X'2F' AS path, 0.0 AS position, 0.0 AS duration, 0 AS watched, 0 AS updated",
            )
            .unwrap();
        let newer = dir.path().join("newer.sqlite3");
        Connection::open(&newer)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99")
            .unwrap();
        let text = dir.path().join("notes.txt");
        fs::write(&text, "not a database").unwrap();
        for (file, message) in [
            (&other, "not a wvd history file"),
            (&view, "not a wvd history file"),
            (&newer, "newer version"),
            (&text, "notes.txt"),
            (&dir.path().join("missing.sqlite3"), "missing.sqlite3"),
        ] {
            let error = format!("{:#}", store.import(file).unwrap_err());
            assert!(error.contains(message), "{error}");
        }
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
