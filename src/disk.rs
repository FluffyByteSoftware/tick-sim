// disk.rs -- a cut-down copy of the game server's disk writer, for tick-sim.
// Author: Jacob Chacko
//
// Same technique as the server:
//
//   - Files are only ever replaced whole. Each one is written as `path.tmp`,
//     forced onto the disk, then renamed over `path`, so a crash never leaves
//     half a file.
//   - write_later() drops a file into a cache and returns straight away. The
//     cache keeps one copy per path, the newest.
//   - A background writer thread takes the whole cache at once, writes the
//     temp files with several threads, renames them, then syncs each folder
//     once for the whole batch.
//   - When the cache is full (too many files or too many bytes),
//     write_later() waits until the writer has made room.
//
// Left out: reading files back, deleting, logging, and the "write right now"
// path. tick-sim only needs the saving side.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How big the cache can get, and how many threads write a batch.
pub struct Limits {
    pub max_files: usize,
    pub max_bytes: u64,
    pub threads: usize,
}

/// What the writer did over the whole run.
#[derive(Default, Clone, Copy)]
pub struct DiskStats {
    pub batches: u64,
    pub files_written: u64,
    pub files_failed: u64,
    pub bytes_written: u64,
    pub busy: Duration,          // total time spent writing batches
    pub longest_batch: Duration, // the slowest single batch
    pub biggest_batch: usize,    // the most files in one batch
    pub waits: u64,              // write_later() calls that had to wait for room
    pub waited: Duration,        // total time spent waiting for room
    pub replaced: u64,           // saves that replaced a copy still waiting
}

/// One file waiting in the cache.
struct Pending {
    // Arc: a shared pointer, so the writer can take the whole cache without
    // copying the bytes.
    contents: Arc<Vec<u8>>,
    /// Goes up every time anything is saved, so the writer can tell whether
    /// the copy it just wrote is still the newest.
    version: u64,
}

struct Cache {
    files: HashMap<PathBuf, Pending>,
    bytes: u64,
    next_version: u64,
    stopping: bool,
    stats: DiskStats,
}

/// What the writer thread and write_later() share: the cache, behind a lock,
/// and a Condvar, which is how one thread sleeps until another says
/// "something changed".
struct Shared {
    cache: Mutex<Cache>,
    changed: Condvar,
}

pub struct DiskWriter {
    shared: Arc<Shared>,
    max_files: usize,
    max_bytes: u64,
    writer: thread::JoinHandle<()>,
}

impl DiskWriter {
    /// Starts the writer thread.
    pub fn start(limits: Limits) -> DiskWriter {
        let shared = Arc::new(Shared {
            cache: Mutex::new(Cache {
                files: HashMap::new(),
                bytes: 0,
                next_version: 0,
                stopping: false,
                stats: DiskStats::default(),
            }),
            changed: Condvar::new(),
        });
        let for_writer = Arc::clone(&shared);
        let threads = limits.threads.max(1);
        let writer = thread::spawn(move || writer_loop(&for_writer, threads));
        DiskWriter {
            shared,
            max_files: limits.max_files,
            max_bytes: limits.max_bytes,
            writer,
        }
    }

    /// Hands a whole file to the cache and returns once it is in. Waits
    /// first if the cache is full.
    pub fn write_later(&self, path: PathBuf, contents: Vec<u8>) {
        let size = contents.len() as u64;
        let mut cache = self.shared.cache.lock().unwrap();

        let mut waiting_since: Option<Instant> = None;
        while !has_room(&cache, &path, size, self.max_files, self.max_bytes) {
            if waiting_since.is_none() {
                waiting_since = Some(Instant::now());
                cache.stats.waits += 1;
            }
            // wait() lets go of the lock, sleeps until notify_all(), and
            // takes the lock back before returning.
            cache = self.shared.changed.wait(cache).unwrap();
        }
        if let Some(since) = waiting_since {
            cache.stats.waited += since.elapsed();
        }

        let version = cache.next_version;
        cache.next_version += 1;
        let pending = Pending {
            contents: Arc::new(contents),
            version,
        };
        // insert() hands back the old entry if this path was already waiting.
        if let Some(old) = cache.files.insert(path, pending) {
            cache.bytes -= old.contents.len() as u64;
            cache.stats.replaced += 1;
        }
        cache.bytes += size;

        drop(cache); // let go of the lock before waking the writer
        self.shared.changed.notify_all();
    }

    /// Writes everything still waiting, stops the writer thread, and hands
    /// back its stats.
    pub fn stop(self) -> DiskStats {
        self.shared.cache.lock().unwrap().stopping = true;
        self.shared.changed.notify_all();
        self.writer.join().expect("disk writer thread panicked");
        let stats = self.shared.cache.lock().unwrap().stats;
        stats
    }
}

/// Whether a file of `size` bytes for `path` fits right now. A file for a
/// path that is already waiting replaces the old copy, so it doesn't add to
/// the count. An empty cache always has room.
fn has_room(cache: &Cache, path: &Path, size: u64, max_files: usize, max_bytes: u64) -> bool {
    if cache.files.is_empty() {
        return true;
    }
    let (files_after, bytes_after) = match cache.files.get(path) {
        Some(old) => (cache.files.len(), cache.bytes - old.contents.len() as u64 + size),
        None => (cache.files.len() + 1, cache.bytes + size),
    };
    files_after <= max_files && bytes_after <= max_bytes
}

/// One file the writer has taken to write. It stays in the cache while it is
/// being written.
struct BatchItem {
    path: PathBuf,
    contents: Arc<Vec<u8>>,
    version: u64,
}

/// The writer thread: sleep until the cache has something in it, take all
/// of it, write it, take out whatever was written, repeat. Returns once
/// stop() has been called and the cache is empty.
fn writer_loop(shared: &Shared, threads: usize) {
    loop {
        let batch: Vec<BatchItem> = {
            let mut cache = shared.cache.lock().unwrap();
            loop {
                if !cache.files.is_empty() {
                    break take_batch(&cache);
                }
                if cache.stopping {
                    return;
                }
                cache = shared.changed.wait(cache).unwrap();
            }
        };

        let started = Instant::now();
        let (written, bytes) = write_batch(&batch, threads);
        let took = started.elapsed();

        let mut cache = shared.cache.lock().unwrap();
        finish_batch(&mut cache, &batch);
        cache.stats.batches += 1;
        cache.stats.files_written += written;
        cache.stats.files_failed += batch.len() as u64 - written;
        cache.stats.bytes_written += bytes;
        cache.stats.busy += took;
        cache.stats.longest_batch = cache.stats.longest_batch.max(took);
        cache.stats.biggest_batch = cache.stats.biggest_batch.max(batch.len());
        drop(cache);
        shared.changed.notify_all();
    }
}

/// Everything in the cache, ready to write. The cache keeps it all until
/// finish_batch().
fn take_batch(cache: &Cache) -> Vec<BatchItem> {
    cache
        .files
        .iter()
        .map(|(path, pending)| BatchItem {
            path: path.clone(),
            contents: Arc::clone(&pending.contents),
            version: pending.version,
        })
        .collect()
}

/// The batch is done. Each file leaves the cache, unless a newer copy was
/// saved while it was being written; that one stays for the next batch.
fn finish_batch(cache: &mut Cache, batch: &[BatchItem]) {
    for item in batch {
        let still_newest = match cache.files.get(&item.path) {
            Some(pending) => pending.version == item.version,
            None => false,
        };
        if still_newest {
            if let Some(old) = cache.files.remove(&item.path) {
                cache.bytes -= old.contents.len() as u64;
            }
        }
    }
}

/// Writes a batch: every temp file written and synced, `threads` at a time,
/// then the renames, then each folder synced once. Hands back how many files
/// made it, and their bytes.
fn write_batch(batch: &[BatchItem], threads: usize) -> (u64, u64) {
    if batch.is_empty() {
        return (0, 0);
    }
    let per_thread = batch.len().div_ceil(threads);

    // Step 1: the temp files, several threads at once. thread::scope makes
    // sure every thread has finished before it returns, which is what lets
    // the threads borrow `batch` directly.
    let mut temp_ok: Vec<bool> = Vec::new();
    thread::scope(|scope| {
        let handles: Vec<_> = batch
            .chunks(per_thread)
            .map(|part| scope.spawn(move || part.iter().map(write_temp).collect::<Vec<bool>>()))
            .collect();
        for handle in handles {
            match handle.join() {
                Ok(results) => temp_ok.extend(results),
                Err(_) => temp_ok.extend(vec![false; per_thread]),
            }
        }
    });

    // Step 2: the renames, one at a time. They are quick.
    let mut written = 0;
    let mut bytes = 0;
    let mut folders: Vec<PathBuf> = Vec::new();
    for (item, ok) in batch.iter().zip(&temp_ok) {
        if !*ok {
            continue;
        }
        let temp = temp_path_for(&item.path);
        if fs::rename(&temp, &item.path).is_ok() {
            written += 1;
            bytes += item.contents.len() as u64;
            folders.push(folder_of(&item.path));
        } else {
            let _ = fs::remove_file(&temp);
        }
    }

    // Step 3: each folder once, however many files went into it. This is
    // what makes the renames permanent on the disk.
    folders.sort();
    folders.dedup();
    for folder in &folders {
        let _ = File::open(folder).and_then(|f| f.sync_all());
    }

    (written, bytes)
}

/// Step 1 for one file: write its temp file and force it onto the disk.
fn write_temp(item: &BatchItem) -> bool {
    let temp = temp_path_for(&item.path);
    let result = File::create(&temp).and_then(|mut file| {
        file.write_all(&item.contents)?;
        file.sync_all() // don't return until the bytes are really on the disk
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.is_ok()
}

fn temp_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

fn folder_of(path: &Path) -> PathBuf {
    path.parent().unwrap_or(Path::new(".")).to_path_buf()
}