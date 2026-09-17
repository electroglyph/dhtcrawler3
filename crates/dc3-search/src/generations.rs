//! Index generations (design §11).
//!
//! An index root holds numbered generation directories (`gen-000001`,
//! `gen-000002`, ...) and a `CURRENT` file naming the live one. `CURRENT` is
//! one line with the generation number and a newline, for example `"3\n"`.
//! Anything else is an error: a reader never guesses which generation is
//! live.
//!
//! `CURRENT` is replaced atomically (temporary file, fsync, rename, fsync of
//! the root). Changes to the root (initialising it, creating a generation,
//! promoting one, removing old ones) are serialised across processes by an
//! exclusive `flock` on `.dc3-root.lock`. Readers never take that lock.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tantivy::directory::error::LockError;
use tantivy::indexer::IndexWriterOptions;
use tantivy::{Index, TantivyDocument, TantivyError};
use tokio::sync::Semaphore;

use crate::index::{
    Result, SearchError, SearchIndex, index_exists, open_existing_index, open_or_create_index,
};

/// Name of the file that names the live generation.
pub const CURRENT_FILE: &str = "CURRENT";
/// Prefix of generation directory names.
pub const GENERATION_PREFIX: &str = "gen-";
/// Generation numbers in directory names are zero-padded to this width.
pub const GENERATION_DIGITS: usize = 6;
/// The generation a new root starts with.
pub const FIRST_GENERATION: u64 = 1;
/// Writer heap used by [`IndexRoot::writer_locked`]'s probe; Tantivy's
/// minimum is 15 MB per indexing thread.
pub const WRITER_PROBE_HEAP_BYTES: usize = 15 * 1024 * 1024;
/// Longest wait for the root lock.
pub const ROOT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest valid `CURRENT`: the 20 digits of `u64::MAX` plus the newline.
const CURRENT_MAX_BYTES: usize = 21;
/// Pause between attempts to take the root lock.
const ROOT_LOCK_RETRY: Duration = Duration::from_millis(10);
/// The root lock file. Names starting with '.' are never generations.
const ROOT_LOCK_FILE: &str = ".dc3-root.lock";
/// Temporary file used while replacing `CURRENT`.
const CURRENT_TEMP_FILE: &str = ".CURRENT.tmp";
/// Prefix for generation directories that are being deleted.
const TRASH_PREFIX: &str = ".trash-";

/// The directory name of `generation`, e.g. `gen-000001`.
pub fn generation_dir_name(generation: u64) -> String {
    format!(
        "{GENERATION_PREFIX}{generation:0width$}",
        width = GENERATION_DIGITS
    )
}

/// Parses a generation directory name. Only the exact form produced by
/// [`generation_dir_name`] for a generation ≥ 1 is accepted.
pub fn parse_generation_dir_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix(GENERATION_PREFIX)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let generation: u64 = digits.parse().ok()?;
    (generation >= FIRST_GENERATION && generation_dir_name(generation) == name)
        .then_some(generation)
}

/// Parses the contents of `CURRENT`.
fn parse_current(bytes: &[u8]) -> Result<u64> {
    let line = bytes
        .strip_suffix(b"\n")
        .ok_or(SearchError::InvalidCurrent("it must end with a newline"))?;
    if line.is_empty() {
        return Err(SearchError::InvalidCurrent("it is empty"));
    }
    if !line.iter().all(u8::is_ascii_digit) {
        return Err(SearchError::InvalidCurrent(
            "it must be one line with a decimal generation number",
        ));
    }
    if line.first() == Some(&b'0') {
        return Err(SearchError::InvalidCurrent(
            "the generation number is zero or has a leading zero",
        ));
    }
    std::str::from_utf8(line)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(SearchError::InvalidCurrent(
            "the generation number is out of range",
        ))
}

fn check_generation(generation: u64) -> Result<()> {
    if generation < FIRST_GENERATION {
        return Err(SearchError::InvalidGeneration(generation));
    }
    Ok(())
}

fn is_not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// Flushes directory entries (new, renamed or removed files) to disk.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Held while the root is being changed; dropping it releases the lock.
struct RootLock {
    _file: File,
}

/// A root directory of index generations.
#[derive(Debug, Clone)]
pub struct IndexRoot {
    root: PathBuf,
}

impl IndexRoot {
    /// Opens `root`, creating it, `gen-000001` and `CURRENT` when `CURRENT`
    /// is missing. A present but invalid `CURRENT` is an error. So is a
    /// missing `CURRENT` next to generations other than the first, because
    /// the live one cannot be known.
    pub fn open(root: &Path) -> Result<IndexRoot> {
        fs::create_dir_all(root)?;
        let this = IndexRoot {
            root: root.to_path_buf(),
        };
        if this.current_if_present()?.is_some() {
            return Ok(this);
        }
        let _lock = this.lock()?;
        // Another process may have initialised the root meanwhile.
        if this.current_if_present()?.is_some() {
            return Ok(this);
        }
        if this
            .scan()?
            .iter()
            .any(|(generation, _)| *generation != FIRST_GENERATION)
        {
            return Err(SearchError::MissingCurrent);
        }
        let dir = this.generation_dir(FIRST_GENERATION);
        fs::create_dir_all(&dir)?;
        sync_dir(&this.root)?;
        drop(open_or_create_index(&dir)?);
        this.write_current(FIRST_GENERATION)?;
        tracing::info!(
            generation = FIRST_GENERATION,
            "search index root initialised"
        );
        Ok(this)
    }

    /// The root directory.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The directory of `generation` (which may not exist).
    pub fn generation_dir(&self, generation: u64) -> PathBuf {
        self.root.join(generation_dir_name(generation))
    }

    /// The live generation and its directory, read from `CURRENT`.
    pub fn current(&self) -> Result<(u64, PathBuf)> {
        let generation = self
            .current_if_present()?
            .ok_or(SearchError::MissingCurrent)?;
        Ok((generation, self.generation_dir(generation)))
    }

    /// Opens the live generation.
    pub fn open_current(&self) -> Result<SearchIndex> {
        let (generation, _) = self.current()?;
        self.open_generation(generation)
    }

    /// Opens an existing generation; never creates anything.
    pub fn open_generation(&self, generation: u64) -> Result<SearchIndex> {
        let permits = Arc::new(Semaphore::new(crate::MAX_CONCURRENT_SEARCHES));
        self.open_generation_with_permits(generation, permits)
    }

    pub(crate) fn open_generation_with_permits(
        &self,
        generation: u64,
        permits: Arc<Semaphore>,
    ) -> Result<SearchIndex> {
        let dir = self.existing_index_dir(generation)?;
        SearchIndex::open_generation(&dir, generation, permits)
    }

    /// Generation directories in the root, in ascending order.
    pub fn generations(&self) -> Result<Vec<u64>> {
        Ok(self
            .scan()?
            .into_iter()
            .filter_map(|(generation, is_dir)| is_dir.then_some(generation))
            .collect())
    }

    /// Creates a new, empty generation numbered one above every existing
    /// one, and returns it with its index. It is not live until
    /// [`promote`](Self::promote)d.
    pub fn create_next(&self) -> Result<(u64, SearchIndex)> {
        let _lock = self.lock()?;
        let current = self.current_if_present()?.unwrap_or(0);
        let highest = self
            .scan()?
            .into_iter()
            .map(|(generation, _)| generation)
            .fold(current, u64::max);
        let next = highest
            .checked_add(1)
            .ok_or(SearchError::InvalidGeneration(highest))?;
        let dir = self.generation_dir(next);
        fs::create_dir(&dir)?;
        sync_dir(&self.root)?;
        let index = SearchIndex::create_generation(&dir, next)?;
        tracing::info!(generation = next, "search index generation created");
        Ok((next, index))
    }

    /// Makes `generation` the live one by rewriting `CURRENT` atomically.
    /// The generation must hold an index with the current schema. The
    /// previously live generation's directory gets a fresh mtime, so
    /// [`cleanup`](Self::cleanup)'s age check counts from its retirement.
    pub fn promote(&self, generation: u64) -> Result<()> {
        let dir = self.existing_index_dir(generation)?;
        let _lock = self.lock()?;
        drop(open_existing_index(&dir)?);
        // An unreadable CURRENT is replaced; there is nothing to retire.
        let previous = self.current_if_present().ok().flatten();
        self.write_current(generation)?;
        if let Some(previous) = previous.filter(|p| *p != generation) {
            let retired = self.generation_dir(previous);
            if let Err(e) = File::open(&retired).and_then(|f| f.set_modified(SystemTime::now()))
                && !is_not_found(&e)
            {
                tracing::warn!(
                    generation = previous,
                    error = %e,
                    "cannot mark the retired search index generation"
                );
            }
        }
        tracing::info!(generation, ?previous, "search index generation promoted");
        Ok(())
    }

    /// Removes generation directories that are not live, not in `except`,
    /// at least `older_than` old (by mtime), and not held by a writer.
    /// Returns the removed generations. An unreadable `CURRENT`, or one that
    /// names a missing generation, is an error and nothing is removed.
    pub fn cleanup(&self, except: &[u64], older_than: Duration) -> Result<Vec<u64>> {
        let mut doomed: Vec<(u64, PathBuf)> = Vec::new();
        {
            let _lock = self.lock()?;
            let (live, _) = self.current()?;
            // Never delete anything while the live generation is broken.
            self.existing_index_dir(live)?;
            remove_file_if_present(&self.root.join(CURRENT_TEMP_FILE))?;
            let now = SystemTime::now();
            for generation in self.generations()? {
                if generation == live || except.contains(&generation) {
                    continue;
                }
                let dir = self.generation_dir(generation);
                let modified = fs::symlink_metadata(&dir)?.modified()?;
                let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
                if age < older_than {
                    continue;
                }
                match self.writer_locked(generation) {
                    Ok(false) => {}
                    Ok(true) => {
                        tracing::info!(
                            generation,
                            "keeping a search index generation with a writer"
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            generation,
                            error = %e,
                            "keeping a search index generation whose writer lock cannot be checked"
                        );
                        continue;
                    }
                }
                let trash = self.root.join(trash_name(generation));
                fs::rename(&dir, &trash)?;
                doomed.push((generation, trash));
            }
            if !doomed.is_empty() {
                sync_dir(&self.root)?;
            }
        }
        // Deleting can take a while, so it happens without the root lock.
        // A directory left behind by a crash is swept up below next time.
        let mut removed = Vec::with_capacity(doomed.len());
        for (generation, trash) in doomed {
            remove_dir_if_present(&trash)?;
            tracing::info!(generation, "search index generation removed");
            removed.push(generation);
        }
        self.sweep_trash()?;
        Ok(removed)
    }

    /// True while an `IndexWriter` (in any process, this one included) holds
    /// the writer lock of `generation`. A generation directory without an
    /// index has no writer.
    ///
    /// It probes by opening a writer on a fresh index handle, so for a few
    /// milliseconds the probe itself holds the lock.
    pub fn writer_locked(&self, generation: u64) -> Result<bool> {
        let dir = self.existing_dir(generation)?;
        if !index_exists(&dir)? {
            return Ok(false);
        }
        let index = Index::open(tantivy::directory::MmapDirectory::open(&dir)?)?;
        let options = IndexWriterOptions::builder()
            .num_worker_threads(1)
            .num_merge_threads(1)
            .memory_budget_per_thread(WRITER_PROBE_HEAP_BYTES)
            .build();
        match index.writer_with_options::<TantivyDocument>(options) {
            Ok(writer) => {
                drop(writer);
                Ok(false)
            }
            Err(TantivyError::LockFailure(LockError::LockBusy, _)) => Ok(true),
            Err(e) => Err(e.into()),
        }
    }

    /// Reads `CURRENT`; `None` when the file does not exist.
    fn current_if_present(&self) -> Result<Option<u64>> {
        let file = match File::open(self.root.join(CURRENT_FILE)) {
            Ok(file) => file,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let limit = u64::try_from(CURRENT_MAX_BYTES)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(CURRENT_MAX_BYTES);
        file.take(limit).read_to_end(&mut bytes)?;
        if bytes.len() > CURRENT_MAX_BYTES {
            return Err(SearchError::InvalidCurrent("it is too long"));
        }
        parse_current(&bytes).map(Some)
    }

    /// Replaces `CURRENT`. The caller holds the root lock.
    fn write_current(&self, generation: u64) -> Result<()> {
        let temp = self.root.join(CURRENT_TEMP_FILE);
        remove_file_if_present(&temp)?;
        let contents = format!("{generation}\n");
        let written = replace_file(&self.root, &temp, CURRENT_FILE, contents.as_bytes());
        if written.is_err() {
            // Best effort; the original error is what matters.
            let _ = fs::remove_file(&temp);
        }
        written.map_err(SearchError::from)
    }

    /// Every name in the root that parses as a generation, with whether it
    /// is a real directory (symlinks are not), in ascending order.
    fn scan(&self) -> Result<Vec<(u64, bool)>> {
        let mut found = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let Some(generation) = entry
                .file_name()
                .to_str()
                .and_then(parse_generation_dir_name)
            else {
                continue;
            };
            found.push((generation, entry.file_type()?.is_dir()));
        }
        found.sort_unstable();
        Ok(found)
    }

    /// The directory of `generation`, which must exist.
    fn existing_dir(&self, generation: u64) -> Result<PathBuf> {
        check_generation(generation)?;
        let dir = self.generation_dir(generation);
        if !dir.is_dir() {
            return Err(SearchError::MissingGeneration(generation));
        }
        Ok(dir)
    }

    /// The directory of `generation`, which must hold an index.
    fn existing_index_dir(&self, generation: u64) -> Result<PathBuf> {
        let dir = self.existing_dir(generation)?;
        if !index_exists(&dir)? {
            return Err(SearchError::MissingGeneration(generation));
        }
        Ok(dir)
    }

    /// Takes the root lock, waiting up to [`ROOT_LOCK_TIMEOUT`].
    fn lock(&self) -> Result<RootLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(ROOT_LOCK_FILE))?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(RootLock { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() >= ROOT_LOCK_TIMEOUT {
                        return Err(SearchError::RootLocked);
                    }
                    std::thread::sleep(ROOT_LOCK_RETRY);
                }
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    /// Deletes directories left in the trash by interrupted cleanups.
    fn sweep_trash(&self) -> Result<()> {
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let is_trash = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(TRASH_PREFIX));
            if is_trash && entry.file_type()?.is_dir() {
                remove_dir_if_present(&entry.path())?;
            }
        }
        Ok(())
    }
}

/// A fresh name for a generation directory that is about to be deleted.
/// Process ID, clock and a counter keep names from different processes and
/// interrupted runs apart.
fn trash_name(generation: u64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{TRASH_PREFIX}{}-{}-{nanos}-{}",
        generation_dir_name(generation),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Writes `contents` to `temp` in `dir`, fsyncs it, renames it to `name`
/// and fsyncs `dir`.
fn replace_file(dir: &Path, temp: &Path, name: &str, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, dir.join(name))?;
    sync_dir(dir)
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if !is_not_found(&e) => Err(e),
        _ => Ok(()),
    }
}

/// Removes a directory tree; one already removed (e.g. by a concurrent
/// cleanup) is not an error.
fn remove_dir_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if !is_not_found(&e) => Err(e),
        _ => Ok(()),
    }
}
