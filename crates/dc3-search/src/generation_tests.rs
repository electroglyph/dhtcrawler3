//! Tests for index generations, the search handle, and two index instances
//! sharing one directory (standing in for the index and web processes).
#![allow(clippy::arithmetic_side_effects)]

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::oneshot;

use crate::*;

const HEAP: usize = 20 * 1024 * 1024;
/// Upper bound for anything the tests wait for.
const WAIT: Duration = Duration::from_secs(30);

fn doc(id: i64, name: &str, files: &str) -> IndexDoc {
    IndexDoc {
        id,
        name: name.into(),
        files: files.into(),
        file_count: 1,
        ..IndexDoc::default()
    }
}

fn add_docs(index: &SearchIndex, docs: &[IndexDoc], checkpoint: i64) {
    let mut w = index.writer(HEAP).unwrap();
    for d in docs {
        w.upsert(d).unwrap();
    }
    w.commit(checkpoint).unwrap();
}

fn ids(index: &SearchIndex, text: &str) -> Vec<i64> {
    let mut v: Vec<i64> = index
        .search(&SearchQuery::new(text))
        .unwrap()
        .hits
        .iter()
        .map(|h| h.id)
        .collect();
    v.sort_unstable();
    v
}

async fn hit_ids(handle: &SearchHandle, text: &str) -> Vec<i64> {
    let mut v: Vec<i64> = handle
        .search(SearchQuery::new(text), SEARCH_TIMEOUT)
        .await
        .unwrap()
        .hits
        .iter()
        .map(|h| h.id)
        .collect();
    v.sort_unstable();
    v
}

async fn wait_for(mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < WAIT,
            "condition not met within {WAIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn write_current(root: &Path, contents: &[u8]) {
    fs::write(root.join(CURRENT_FILE), contents).unwrap();
}

fn read_current(root: &Path) -> Vec<u8> {
    fs::read(root.join(CURRENT_FILE)).unwrap()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Starts `handle.watch` on the runtime; send on the returned channel to stop it.
fn spawn_watch(
    handle: &SearchHandle,
    interval: Duration,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(handle.clone().watch(interval, async move {
        let _ = stopped.await;
    }));
    (stop, task)
}

async fn stop_watch(stop: oneshot::Sender<()>, task: tokio::task::JoinHandle<()>) {
    stop.send(()).unwrap();
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
}

// ---------------------------------------------------------------- the root

#[test]
fn generation_directory_names() {
    assert_eq!(generation_dir_name(1), "gen-000001");
    assert_eq!(generation_dir_name(999_999), "gen-999999");
    assert_eq!(generation_dir_name(1_234_567), "gen-1234567");
    for (name, want) in [
        ("gen-000001", Some(1)),
        ("gen-000042", Some(42)),
        ("gen-1234567", Some(1_234_567)),
        ("gen-000000", None),
        ("gen-1", None),
        ("gen-00001", None),
        ("gen-0000001", None),
        ("gen-+00001", None),
        ("gen--00001", None),
        ("gen-00000a", None),
        ("gen-", None),
        ("xgen-000001", None),
        ("gen-000001 ", None),
        ("GEN-000001", None),
        ("gen-99999999999999999999999", None),
    ] {
        assert_eq!(parse_generation_dir_name(name), want, "{name}");
    }
}

#[test]
fn root_initialises_first_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index");
    let root = IndexRoot::open(&path).unwrap();
    assert_eq!(root.path(), path);
    assert_eq!(read_current(&path), b"1\n");
    let (generation, gen_dir) = root.current().unwrap();
    assert_eq!(generation, FIRST_GENERATION);
    assert_eq!(gen_dir, path.join("gen-000001"));
    assert_eq!(gen_dir, root.generation_dir(1));
    assert!(gen_dir.join("meta.json").is_file());
    assert_eq!(root.generations().unwrap(), [1]);

    let index = root.open_current().unwrap();
    assert_eq!(index.generation(), Some(1));
    assert_eq!(index.doc_count(), 0);
    assert_eq!(index.checkpoint().unwrap(), 0);
    add_docs(&index, &[doc(1, "kept", "")], 5);
    drop(index);

    // Reopening keeps the generation and its data.
    let root = IndexRoot::open(&path).unwrap();
    assert_eq!(root.current().unwrap().0, 1);
    let index = root.open_current().unwrap();
    assert_eq!(index.checkpoint().unwrap(), 5);
    assert_eq!(ids(&index, "kept"), [1]);

    // Other entries in the root are not generations.
    fs::create_dir(path.join("lost+found")).unwrap();
    fs::write(path.join("notes.txt"), b"x").unwrap();
    assert_eq!(root.generations().unwrap(), [1]);
}

#[test]
fn corrupt_current_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let (spare, _) = root.create_next().unwrap();
    let bad: [&[u8]; 16] = [
        b"",
        b"1",
        b"\n",
        b"0\n",
        b"01\n",
        b" 1\n",
        b"1 \n",
        b"1\r\n",
        b"1\n\n",
        b"1\n2\n",
        b"-1\n",
        b"+1\n",
        b"one\n",
        b"18446744073709551616\n",
        b"123456789012345678901234\n",
        b"\xff\n",
    ];
    for contents in bad {
        write_current(dir.path(), contents);
        let shown = String::from_utf8_lossy(contents);
        assert!(
            matches!(root.current(), Err(SearchError::InvalidCurrent(_))),
            "{shown:?}"
        );
        assert!(
            matches!(
                IndexRoot::open(dir.path()),
                Err(SearchError::InvalidCurrent(_))
            ),
            "{shown:?}"
        );
        assert!(matches!(
            SearchHandle::open(dir.path()),
            Err(SearchError::InvalidCurrent(_))
        ));
        assert!(root.open_current().is_err());
        assert!(root.create_next().is_err());
        assert!(root.cleanup(&[], Duration::ZERO).is_err());
        // Failing closed: nothing was rewritten or removed.
        assert_eq!(read_current(dir.path()), contents, "{shown:?}");
        assert_eq!(root.generations().unwrap(), [1, spare]);
    }

    // The largest number is valid, but there is no such generation.
    write_current(dir.path(), b"18446744073709551615\n");
    assert_eq!(root.current().unwrap().0, u64::MAX);
    assert!(matches!(
        root.open_current(),
        Err(SearchError::MissingGeneration(u64::MAX))
    ));
    assert!(matches!(
        root.create_next(),
        Err(SearchError::InvalidGeneration(u64::MAX))
    ));

    // A CURRENT naming a missing generation fails closed as well.
    write_current(dir.path(), b"7\n");
    assert!(matches!(
        root.open_current(),
        Err(SearchError::MissingGeneration(7))
    ));
    assert!(matches!(
        SearchHandle::open(dir.path()),
        Err(SearchError::MissingGeneration(7))
    ));
    assert!(matches!(
        root.cleanup(&[], Duration::ZERO),
        Err(SearchError::MissingGeneration(7))
    ));
    assert_eq!(root.generations().unwrap(), [1, spare]);

    // An explicit promote repairs it.
    root.promote(1).unwrap();
    assert_eq!(read_current(dir.path()), b"1\n");
    assert_eq!(root.open_current().unwrap().generation(), Some(1));
}

#[test]
fn missing_current_beside_other_generations_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    assert_eq!(root.create_next().unwrap().0, 2);
    fs::remove_file(dir.path().join(CURRENT_FILE)).unwrap();
    assert!(matches!(root.current(), Err(SearchError::MissingCurrent)));
    assert!(matches!(
        IndexRoot::open(dir.path()),
        Err(SearchError::MissingCurrent)
    ));
    assert!(matches!(
        SearchHandle::open(dir.path()),
        Err(SearchError::MissingCurrent)
    ));
    assert!(!dir.path().join(CURRENT_FILE).exists());

    // With only the first generation left, the root is initialised again.
    fs::remove_dir_all(root.generation_dir(2)).unwrap();
    IndexRoot::open(dir.path()).unwrap();
    assert_eq!(read_current(dir.path()), b"1\n");
}

#[test]
fn concurrent_opens_initialise_the_root_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index");
    // Each thread takes the root lock through its own file descriptor, as
    // separate processes would.
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                let root = IndexRoot::open(&path).unwrap();
                root.open_current().unwrap().generation()
            })
        })
        .collect();
    for t in threads {
        assert_eq!(t.join().unwrap(), Some(1));
    }
    assert_eq!(read_current(&path), b"1\n");
    let root = IndexRoot::open(&path).unwrap();
    assert_eq!(root.generations().unwrap(), [1]);
    let index = root.open_current().unwrap();
    assert_eq!(index.doc_count(), 0);
    add_docs(&index, &[doc(1, "after init", "")], 1);
    assert_eq!(ids(&index, "init"), [1]);
}

#[test]
fn concurrent_create_next_hands_out_distinct_generations() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || root.create_next().unwrap().0)
        })
        .collect();
    let mut got: Vec<u64> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    got.sort_unstable();
    assert_eq!(got, [2, 3, 4, 5]);
    assert_eq!(root.generations().unwrap(), [1, 2, 3, 4, 5]);
    assert_eq!(root.current().unwrap().0, 1);
}

#[test]
fn create_next_and_promote() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let (two, index) = root.create_next().unwrap();
    assert_eq!(two, 2);
    assert_eq!(index.generation(), Some(2));
    assert_eq!(index.doc_count(), 0);
    assert_eq!(index.checkpoint().unwrap(), 0);
    drop(index);
    // Not live until promoted.
    assert_eq!(root.current().unwrap().0, 1);
    assert_eq!(root.create_next().unwrap().0, 3);
    assert_eq!(root.generations().unwrap(), [1, 2, 3]);

    root.promote(2).unwrap();
    assert_eq!(read_current(dir.path()), b"2\n");
    assert_eq!(root.open_current().unwrap().generation(), Some(2));
    // Promoting the live generation again is harmless.
    root.promote(2).unwrap();
    assert_eq!(read_current(dir.path()), b"2\n");

    // Numbers go past every name in use, even one that is not a directory.
    fs::write(dir.path().join("gen-000009"), b"not a directory").unwrap();
    assert_eq!(root.create_next().unwrap().0, 10);
    assert_eq!(root.generations().unwrap(), [1, 2, 3, 10]);

    // Only real generations with an index can be promoted.
    assert!(matches!(
        root.promote(0),
        Err(SearchError::InvalidGeneration(0))
    ));
    assert!(matches!(
        root.promote(4),
        Err(SearchError::MissingGeneration(4))
    ));
    assert!(matches!(
        root.promote(9),
        Err(SearchError::MissingGeneration(9))
    ));
    fs::create_dir(dir.path().join("gen-000011")).unwrap();
    assert!(matches!(
        root.promote(11),
        Err(SearchError::MissingGeneration(11))
    ));
    assert!(matches!(
        root.open_generation(11),
        Err(SearchError::MissingGeneration(11))
    ));
    assert_eq!(read_current(dir.path()), b"2\n");

    // No temporary files are left behind.
    let names = entries(dir.path());
    assert!(!names.iter().any(|n| n.contains("tmp")), "{names:?}");
}

#[test]
fn schema_mismatch_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let other = root.generation_dir(2);
    fs::create_dir(&other).unwrap();
    let mut b = tantivy::schema::Schema::builder();
    b.add_text_field("title", tantivy::schema::TEXT);
    drop(tantivy::Index::create_in_dir(&other, b.build()).unwrap());

    assert!(matches!(root.promote(2), Err(SearchError::SchemaMismatch)));
    assert!(matches!(
        root.open_generation(2),
        Err(SearchError::SchemaMismatch)
    ));
    assert!(matches!(
        SearchIndex::open(&other),
        Err(SearchError::SchemaMismatch)
    ));
    assert!(matches!(
        SearchIndex::open_or_create(&other),
        Err(SearchError::SchemaMismatch)
    ));
    assert_eq!(read_current(dir.path()), b"1\n");
    // An index-less directory is not silently created by `open`.
    let empty = dir.path().join("empty");
    fs::create_dir(&empty).unwrap();
    assert!(SearchIndex::open(&empty).is_err());
    assert!(!empty.join("meta.json").exists());
}

#[test]
fn cleanup_keeps_live_recent_excepted_and_locked_generations() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    for want in 2..=5 {
        assert_eq!(root.create_next().unwrap().0, want);
    }
    root.promote(3).unwrap();

    // Everything is younger than an hour.
    let hour = Duration::from_secs(3600);
    assert!(root.cleanup(&[], hour).unwrap().is_empty());
    assert_eq!(root.generations().unwrap(), [1, 2, 3, 4, 5]);

    // 3 is live, 5 is excepted, and 4 has a writer.
    let four = root.open_generation(4).unwrap();
    let writer = four.writer(HEAP).unwrap();
    assert_eq!(root.cleanup(&[5], Duration::ZERO).unwrap(), [1, 2]);
    assert_eq!(root.generations().unwrap(), [3, 4, 5]);
    drop(writer);
    drop(four);
    // Leftovers of an interrupted cleanup or promote neither get in the way
    // nor survive.
    fs::create_dir_all(dir.path().join(".trash-gen-000004/sub")).unwrap();
    fs::write(dir.path().join(".trash-gen-000004/sub/file"), b"x").unwrap();
    fs::write(dir.path().join(".CURRENT.tmp"), b"9\n").unwrap();
    assert_eq!(root.cleanup(&[], Duration::ZERO).unwrap(), [4, 5]);
    assert_eq!(root.generations().unwrap(), [3]);
    assert!(root.cleanup(&[], Duration::ZERO).unwrap().is_empty());

    assert_eq!(
        entries(dir.path()),
        [".dc3-root.lock", "CURRENT", "gen-000003"]
    );
    assert_eq!(root.open_current().unwrap().generation(), Some(3));
}

#[test]
fn cleanup_survives_concurrent_generation_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    // A crowd of stale generations widens the sweep loop so a concurrent
    // deleter reliably lands inside it.
    for _ in 0..30 {
        let _ = root.create_next().unwrap();
    }
    let hour = Duration::from_secs(3600);
    assert!(root.cleanup(&[], hour).unwrap().is_empty());

    let stop = Arc::new(AtomicBool::new(false));
    let remover = {
        let probe = root.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                for generation in 2..=31 {
                    let path = probe.generation_dir(generation);
                    let _ = fs::remove_dir_all(&path);
                    let _ = fs::create_dir(&path);
                }
            }
        })
    };
    // Every sweep must succeed: a generation that vanishes mid-sweep is
    // skipped, never an error. Nothing is old enough to remove.
    for _ in 0..100 {
        assert!(root.cleanup(&[], hour).unwrap().is_empty());
    }
    stop.store(true, Ordering::Relaxed);
    remover.join().unwrap();
    // The live generation and its pointer survived the whole thing.
    assert_eq!(root.current().unwrap().0, 1);
    assert!(root.generation_dir(1).is_dir());
}

#[test]
fn promote_restarts_the_age_of_the_retired_generation() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    assert_eq!(root.create_next().unwrap().0, 2);
    assert_eq!(root.create_next().unwrap().0, 3);
    root.promote(2).unwrap();
    let long_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
    for generation in [1, 2, 3] {
        File::open(root.generation_dir(generation))
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
    }
    // Retiring 2 now gives it a fresh age; 1 retired long ago.
    root.promote(3).unwrap();
    let hour = Duration::from_secs(3600);
    assert_eq!(root.cleanup(&[], hour).unwrap(), [1]);
    assert_eq!(root.generations().unwrap(), [2, 3]);
}

#[test]
fn promote_of_a_vanished_target_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let (two, index) = root.create_next().unwrap();
    drop(index);
    // The target vanishes between creation and promotion: the under-lock
    // check must fail without touching CURRENT.
    fs::remove_dir_all(root.generation_dir(two)).unwrap();
    assert!(matches!(
        root.promote(two),
        Err(SearchError::MissingGeneration(2))
    ));
    assert_eq!(root.current().unwrap().0, 1);
}

#[test]
fn writer_locked_tracks_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    assert!(!root.writer_locked(1).unwrap());

    let index = root.open_current().unwrap();
    let writer = index.writer(HEAP).unwrap();
    assert!(root.writer_locked(1).unwrap());
    // A second writer, even through another index instance, is refused.
    let other = root.open_current().unwrap();
    assert!(matches!(
        other.writer(HEAP),
        Err(SearchError::Index(tantivy::TantivyError::LockFailure(..)))
    ));
    drop(writer);
    assert!(!root.writer_locked(1).unwrap());

    // The probe released the lock again.
    let writer = other.writer(HEAP).unwrap();
    assert!(root.writer_locked(1).unwrap());
    writer.wait_merging_threads().unwrap();
    assert!(!root.writer_locked(1).unwrap());

    let (two, _) = root.create_next().unwrap();
    assert!(!root.writer_locked(two).unwrap());
    assert!(matches!(
        root.writer_locked(0),
        Err(SearchError::InvalidGeneration(0))
    ));
    assert!(matches!(
        root.writer_locked(9),
        Err(SearchError::MissingGeneration(9))
    ));
    // A generation directory without an index has no writer.
    fs::create_dir(root.generation_dir(9)).unwrap();
    assert!(!root.writer_locked(9).unwrap());
}

// ---------------------------------------------------------- the web handle

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_switches_the_handle_after_watch() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    add_docs(
        &root.open_current().unwrap(),
        &[doc(1, "old release", "")],
        1,
    );
    let handle = SearchHandle::open(dir.path()).unwrap();
    assert_eq!(handle.current_generation(), 1);
    assert_eq!(hit_ids(&handle, "release").await, [1]);

    let (generation, next) = root.create_next().unwrap();
    add_docs(
        &next,
        &[doc(2, "new release", ""), doc(3, "new extra", "")],
        1,
    );
    drop(next);
    // Built but not promoted: the handle stays where it is.
    assert_eq!(handle.refresh().unwrap(), 1);
    assert_eq!(handle.current_generation(), 1);

    let clone = handle.clone();
    let (stop, task) = spawn_watch(&handle, Duration::from_millis(20));
    root.promote(generation).unwrap();
    wait_for(|| handle.current_generation() == generation).await;
    assert_eq!(handle.doc_count(), 2);
    assert_eq!(hit_ids(&handle, "release").await, [2]);
    assert!(hit_ids(&handle, "old").await.is_empty());
    // Every clone follows.
    assert_eq!(clone.current_generation(), generation);
    assert_eq!(hit_ids(&clone, "new").await, [2, 3]);
    stop_watch(stop, task).await;

    // Old generations can now be removed; the handle keeps working.
    assert_eq!(root.cleanup(&[], Duration::ZERO).unwrap(), [1]);
    assert_eq!(hit_ids(&handle, "extra").await, [3]);
    assert!(format!("{handle:?}").contains(&generation.to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_survives_a_broken_current_and_stops_on_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    add_docs(&root.open_current().unwrap(), &[doc(1, "steady", "")], 1);
    let (generation, next) = root.create_next().unwrap();
    add_docs(&next, &[doc(2, "recovered", "")], 1);
    drop(next);
    let handle = SearchHandle::open(dir.path()).unwrap();

    // A zero interval is clamped, not a busy loop or a panic.
    let (stop, task) = spawn_watch(&handle, Duration::ZERO);
    write_current(dir.path(), b"garbage");
    assert!(matches!(
        handle.refresh(),
        Err(SearchError::InvalidCurrent(_))
    ));
    tokio::time::sleep(MIN_WATCH_INTERVAL * 10).await;
    assert_eq!(handle.current_generation(), 1);
    assert_eq!(hit_ids(&handle, "steady").await, [1]);

    root.promote(generation).unwrap();
    wait_for(|| handle.current_generation() == generation).await;
    assert_eq!(hit_ids(&handle, "recovered").await, [2]);
    stop_watch(stop, task).await;
}

#[test]
fn handle_types_work_across_threads_and_tasks() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>(_: &T) {}
    send_sync::<SearchHandle>();
    send_sync::<IndexRoot>();
    send_sync::<SearchIndex>();
    let dir = tempfile::tempdir().unwrap();
    let handle = SearchHandle::open(dir.path()).unwrap();
    // axum handlers and `tokio::spawn` need these futures to be `Send`.
    let search = handle.search(SearchQuery::new("x"), SEARCH_TIMEOUT);
    send(&search);
    let watch = handle
        .clone()
        .watch(Duration::from_secs(1), std::future::pending::<()>());
    send(&watch);
}

#[test]
fn handle_keeps_one_search_limit_across_generations() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let handle = SearchHandle::open(dir.path()).unwrap();
    let before = Arc::clone(handle.index().permits());
    let (generation, _) = root.create_next().unwrap();
    root.promote(generation).unwrap();
    assert_eq!(handle.refresh().unwrap(), generation);
    assert_eq!(handle.current_generation(), generation);
    assert!(Arc::ptr_eq(&before, handle.index().permits()));
    assert_eq!(before.available_permits(), MAX_CONCURRENT_SEARCHES);
    // Separately opened indexes have their own limit.
    let separate = root.open_current().unwrap();
    assert!(!Arc::ptr_eq(&before, separate.permits()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_limit_carries_over_a_generation_switch() {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let handle = SearchHandle::open(dir.path()).unwrap();
    let (generation, next) = root.create_next().unwrap();
    add_docs(&next, &[doc(1, "limited", "")], 1);
    drop(next);

    let permits = Arc::clone(handle.index().permits());
    let held = permits
        .acquire_many(MAX_CONCURRENT_SEARCHES as u32)
        .await
        .unwrap();
    root.promote(generation).unwrap();
    assert_eq!(handle.refresh().unwrap(), generation);
    let e = handle
        .search(SearchQuery::new("limited"), Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(matches!(e, SearchError::Timeout), "{e}");
    drop(held);
    assert_eq!(hit_ids(&handle, "limited").await, [1]);
}

// ------------------------------------------ two instances, one directory

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_in_one_instance_is_visible_in_another() {
    let dir = tempfile::tempdir().unwrap();
    // The index process: the only writer.
    let root = IndexRoot::open(dir.path()).unwrap();
    let writer_side = root.open_current().unwrap();
    let mut writer = writer_side.writer(HEAP).unwrap();
    // The web process: separate `Index` instances on the same directory.
    let reader_side = IndexRoot::open(dir.path()).unwrap().open_current().unwrap();
    let handle = SearchHandle::open(dir.path()).unwrap();
    assert_eq!(reader_side.doc_count(), 0);
    assert_eq!(handle.doc_count(), 0);

    writer.upsert(&doc(1, "fresh upload", "")).unwrap();
    writer.commit(1).unwrap();
    // Visible after an explicit reload ...
    reader_side.reload().unwrap();
    assert_eq!(reader_side.doc_count(), 1);
    assert!(!reader_side.reload_if_changed().unwrap());
    assert_eq!(reader_side.checkpoint().unwrap(), 1);
    assert_eq!(ids(&reader_side, "fresh"), [1]);

    // ... and through a running watch.
    let (stop, task) = spawn_watch(&handle, Duration::from_millis(20));
    wait_for(|| handle.doc_count() == 1).await;
    assert_eq!(hit_ids(&handle, "fresh").await, [1]);

    writer.delete(1).unwrap();
    writer.upsert(&doc(2, "second upload", "")).unwrap();
    writer.commit(2).unwrap();
    let second_visible = || {
        let index = handle.index();
        index
            .search(&SearchQuery::new("second"))
            .is_ok_and(|r| r.total == 1)
    };
    wait_for(second_visible).await;
    assert_eq!(hit_ids(&handle, "upload").await, [2]);
    assert_eq!(handle.index().checkpoint().unwrap(), 2);
    stop_watch(stop, task).await;

    // The reload check picks up a commit without Tantivy's own watcher.
    writer.upsert(&doc(3, "third upload", "")).unwrap();
    writer.commit(3).unwrap();
    reader_side.reload_if_changed().unwrap();
    assert_eq!(ids(&reader_side, "upload"), [2, 3]);
    assert!(!reader_side.reload_if_changed().unwrap());
}

#[test]
fn reader_instance_survives_commits_merges_and_gc() {
    const ROUNDS: i64 = 50;
    const DOCS_PER_ROUND: i64 = 20;

    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let writer_side = root.open_current().unwrap();
    let mut writer = writer_side.writer(HEAP).unwrap();
    let handle = SearchHandle::open(dir.path()).unwrap();

    // Every document count the index side commits, recorded before the
    // commit, so the reader can check that it only sees committed states.
    let committed = Arc::new(Mutex::new(BTreeSet::from([0u64])));

    // The web side refreshes, reloads and searches as fast as it can; any
    // error ends the test.
    let stop = Arc::new(AtomicBool::new(false));
    let passes = Arc::new(AtomicUsize::new(0));
    let reader = {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        let passes = Arc::clone(&passes);
        let committed = Arc::clone(&committed);
        std::thread::spawn(move || -> Result<(), String> {
            let query = SearchQuery::new("common ");
            loop {
                handle.refresh().map_err(|e| e.to_string())?;
                let index = handle.index();
                for reload in [false, true] {
                    if reload {
                        index.reload().map_err(|e| e.to_string())?;
                    }
                    let total = index.search(&query).map_err(|e| e.to_string())?.total;
                    if !committed.lock().unwrap().contains(&total) {
                        return Err(format!("saw {total} documents, never committed"));
                    }
                }
                passes.fetch_add(1, Ordering::Relaxed);
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
            }
        })
    };
    let started = Instant::now();
    while passes.load(Ordering::Relaxed) == 0 {
        assert!(!reader.is_finished(), "reader stopped early");
        assert!(started.elapsed() < WAIT, "reader did not start");
        std::thread::yield_now();
    }

    // The index side commits 50 times. Each round deletes a quarter of the
    // previous round's documents and the oldest live one, so every segment
    // keeps documents and carries deletes, the default merge policy has to
    // merge, and the garbage collector removes files the reader may have
    // open.
    let mut live = BTreeSet::new();
    let mut next_id = 0i64;
    let mut most_segments = 0;
    for round in 0..ROUNDS {
        let mut doomed: Vec<i64> = (next_id - DOCS_PER_ROUND + 1..=next_id)
            .filter(|id| id % 4 == 0 && live.contains(id))
            .collect();
        doomed.extend(live.first().copied());
        for id in doomed {
            writer.delete(id).unwrap();
            live.remove(&id);
        }
        for _ in 0..DOCS_PER_ROUND {
            next_id += 1;
            let name = format!("common item{next_id}");
            writer
                .upsert(&doc(next_id, &name, "folder/file.txt"))
                .unwrap();
            live.insert(next_id);
        }
        committed.lock().unwrap().insert(live.len() as u64);
        writer.commit(round + 1).unwrap();
        most_segments = most_segments.max(writer_side.segment_count());
    }
    writer.wait_merging_threads().unwrap();
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap().unwrap();
    let passes = passes.load(Ordering::Relaxed);
    assert!(passes > 1, "{passes} reader passes");
    // Without merges there would be one segment per commit.
    assert!(
        most_segments < ROUNDS as usize / 2,
        "{most_segments} segments during the run"
    );

    // The final state is visible through the other instance.
    handle.refresh().unwrap();
    let index = handle.index();
    let expected = live.len() as u64;
    assert_eq!(expected, 1000 - 49 * 6);
    assert_eq!(index.doc_count(), expected);
    assert_eq!(index.checkpoint().unwrap(), ROUNDS);
    assert_eq!(
        index.search(&SearchQuery::new("common ")).unwrap().total,
        expected
    );
    let mut q = SearchQuery::new("common ");
    q.sort = Sort::Newest;
    q.per_page = MAX_PER_PAGE;
    let newest = index.search(&q).unwrap();
    assert_eq!(newest.hits.len(), MAX_PER_PAGE as usize);
    assert!(newest.hits.iter().all(|h| live.contains(&h.id)));

    // Merges ran, and the garbage collector left only live segments' files.
    let segments = index.segment_count();
    assert!(segments < ROUNDS as usize / 2, "{segments} segments");
    let on_disk: BTreeSet<String> = entries(&root.generation_dir(1))
        .into_iter()
        .filter_map(|name| {
            let stem = name.split('.').next()?.to_owned();
            (stem.len() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit())).then_some(stem)
        })
        .collect();
    assert_eq!(on_disk.len(), segments, "{on_disk:?}");
}
