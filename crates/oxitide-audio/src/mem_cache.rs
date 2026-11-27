//! The Memory cache backend: track bytes held in RAM only.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use symphonia::core::io::MediaSource;

use crate::cache::CacheKey;
use oxitide_tidal::stream::Quality;

/// Size of one storage chunk. Large enough that the allocator hands each
/// chunk straight to the OS (and returns it when the entry is freed), small
/// enough that the unused tail of a track's last chunk stays cheap.
pub const CHUNK_BYTES: usize = 1 << 20;

#[derive(Default)]
struct Chunks {
    chunks: Vec<Box<[u8]>>,
    len: u64,
}

/// An append-only byte buffer stored in fixed-size chunks, shared between one
/// writer and any number of readers. Growing it never moves bytes already
/// written, so a track never needs twice its size in RAM.
#[derive(Default)]
pub struct ChunkBuf {
    inner: RwLock<Chunks>,
    /// Set on a scratch buffer: the cache whose pending total its bytes count
    /// toward until the buffer itself drops (its writer and every reader).
    pending_in: Option<Arc<MemoryCache>>,
}

impl Drop for ChunkBuf {
    fn drop(&mut self) {
        if let Some(cache) = self.pending_in.take() {
            let len = self
                .inner
                .get_mut()
                .map(|c| c.len)
                .unwrap_or_else(|e| e.into_inner().len);
            cache.release_scratch(len);
        }
    }
}

impl ChunkBuf {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    // The data is plain bytes, so a panic elsewhere cannot leave it in a
    // state worse than "shorter than expected": recover from poisoning.
    fn read_guard(&self) -> RwLockReadGuard<'_, Chunks> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_guard(&self) -> RwLockWriteGuard<'_, Chunks> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Appends `data`, filling the last chunk before allocating a new one.
    pub fn append(&self, mut data: &[u8]) {
        let mut state = self.write_guard();
        while !data.is_empty() {
            let index = (state.len / CHUNK_BYTES as u64) as usize;
            let offset = (state.len % CHUNK_BYTES as u64) as usize;
            if index == state.chunks.len() {
                state.chunks.push(vec![0u8; CHUNK_BYTES].into_boxed_slice());
            }
            let n = (CHUNK_BYTES - offset).min(data.len());
            state.chunks[index][offset..offset + n].copy_from_slice(&data[..n]);
            state.len += n as u64;
            data = &data[n..];
        }
    }

    pub fn len(&self) -> u64 {
        self.read_guard().len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies the bytes at `pos..` into `out`: at most `len - pos` of them,
    /// and 0 at or past the end. Never waits for more bytes to arrive.
    pub fn read_at(&self, pos: u64, out: &mut [u8]) -> usize {
        let state = self.read_guard();
        if pos >= state.len {
            return 0;
        }
        let total = (state.len - pos).min(out.len() as u64) as usize;
        let mut copied = 0;
        while copied < total {
            let at = pos + copied as u64;
            let index = (at / CHUNK_BYTES as u64) as usize;
            let offset = (at % CHUNK_BYTES as u64) as usize;
            let n = (CHUNK_BYTES - offset).min(total - copied);
            out[copied..copied + n].copy_from_slice(&state.chunks[index][offset..offset + n]);
            copied += n;
        }
        total
    }
}

/// A seekable reader over a [`ChunkBuf`]. It sees bytes appended after it
/// was created, and it keeps the buffer alive after the cache drops it.
pub struct ChunkReader {
    buf: Arc<ChunkBuf>,
    pos: u64,
}

impl ChunkReader {
    pub fn new(buf: Arc<ChunkBuf>) -> Self {
        Self { buf, pos: 0 }
    }
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.buf.read_at(self.pos, out);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for ChunkReader {
    /// `SeekFrom::End` is relative to the buffer's current length.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (base, delta) = match pos {
            SeekFrom::Start(to) => {
                self.pos = to;
                return Ok(to);
            }
            SeekFrom::Current(delta) => (self.pos, delta),
            SeekFrom::End(delta) => (self.buf.len(), delta),
        };
        match base.checked_add_signed(delta) {
            Some(to) => {
                self.pos = to;
                Ok(to)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )),
        }
    }
}

impl MediaSource for ChunkReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.buf.len())
    }
}

struct MemEntry {
    buf: Arc<ChunkBuf>,
    last_used: u64,
}

/// Whole tracks held in RAM, with the disk cache's rules: one writer per
/// key, least recently used first out, and protected (playing or prefetched)
/// entries kept. Unlike the disk cache, in-flight and scratch bytes count
/// against the limit, which is a RAM budget. Playback is never refused for
/// it: when only protected or in-flight data is left, a write goes ahead and
/// the total may exceed the limit by the data actually in use.
///
/// `committed_bytes` and `pending` change only under the `entries` lock, so
/// eviction decisions see a consistent total; the atomics are for lock-free
/// readouts. Locks are taken in the order `entries`, then `protected`.
pub struct MemoryCache {
    max_bytes: AtomicU64,
    committed_bytes: AtomicU64,
    pending: AtomicU64,
    entries: Mutex<HashMap<CacheKey, MemEntry>>,
    /// Holders per key: a seek or a repeated track can hold one key twice.
    protected: Mutex<HashMap<CacheKey, u32>>,
    in_flight: Mutex<HashSet<CacheKey>>,
    /// Logical clock for the LRU order.
    clock: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain bookkeeping: a panic elsewhere leaves nothing half-updated that
    // is worse than a stale total, so recover rather than poison callers.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn sub_saturating(value: &AtomicU64, delta: u64) {
    let mut current = value.load(Ordering::SeqCst);
    loop {
        let next = current.saturating_sub(delta);
        match value.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

impl MemoryCache {
    pub fn new(max_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            max_bytes: AtomicU64::new(max_bytes),
            committed_bytes: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            entries: Mutex::new(HashMap::new()),
            protected: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            clock: AtomicU64::new(0),
        })
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::SeqCst)
    }

    /// A reader over the entry's bytes; bumps it in the LRU order.
    pub fn lookup(&self, key: &CacheKey) -> Option<ChunkReader> {
        let mut entries = lock(&self.entries);
        let entry = entries.get_mut(key)?;
        entry.last_used = self.tick();
        Some(ChunkReader::new(entry.buf.clone()))
    }

    /// `None` when a writer for `key` is already in flight.
    pub fn begin_write(self: &Arc<Self>, key: &CacheKey) -> Option<MemWriter> {
        if !lock(&self.in_flight).insert(*key) {
            tracing::debug!(
                "memory cache entry {} is already being written",
                key.file_name()
            );
            return None;
        }
        Some(MemWriter {
            cache: self.clone(),
            key: *key,
            buf: ChunkBuf::new(),
            bytes: 0,
            done: false,
        })
    }

    /// A RAM buffer for a download that is not cached (a seek, a second
    /// writer). Its bytes count as pending until it drops.
    pub fn scratch(self: &Arc<Self>) -> MemScratch {
        MemScratch {
            cache: self.clone(),
            buf: Arc::new(ChunkBuf {
                inner: RwLock::default(),
                pending_in: Some(self.clone()),
            }),
        }
    }

    pub fn remove(&self, key: &CacheKey) {
        let removed = {
            let mut entries = lock(&self.entries);
            let removed = entries.remove(key);
            if let Some(entry) = &removed {
                sub_saturating(&self.committed_bytes, entry.buf.len());
            }
            removed
        };
        drop(removed); // free outside the lock
    }

    /// Drops every committed entry, protected ones included (as the disk
    /// cache's `clear` does). Live readers keep their bytes until they drop;
    /// in-flight writers and scratch buffers are untouched.
    pub fn clear(&self) {
        let removed: Vec<MemEntry> = {
            let mut entries = lock(&self.entries);
            self.committed_bytes.store(0, Ordering::SeqCst);
            entries.drain().map(|(_, entry)| entry).collect()
        };
        drop(removed);
    }

    pub fn set_max_bytes(&self, max: u64) {
        self.max_bytes.store(max, Ordering::SeqCst);
        self.evict_to_fit();
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(Ordering::SeqCst)
    }

    /// Committed entries.
    pub fn size_bytes(&self) -> u64 {
        self.committed_bytes.load(Ordering::SeqCst)
    }

    /// In-flight writers plus scratch buffers.
    pub fn pending_bytes(&self) -> u64 {
        self.pending.load(Ordering::SeqCst)
    }

    /// Adds one holder; each `protect` is paired with one `unprotect`.
    pub fn protect(&self, key: &CacheKey) {
        *lock(&self.protected).entry(*key).or_insert(0) += 1;
    }

    /// Removes one holder (a no-op when there is none). The last one out
    /// lets the entry be evicted, and eviction runs at once.
    pub fn unprotect(&self, key: &CacheKey) {
        let released = {
            let mut protected = lock(&self.protected);
            match protected.get_mut(key) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    false
                }
                Some(_) => {
                    protected.remove(key);
                    true
                }
                None => false,
            }
        };
        if released {
            self.evict_to_fit();
        }
    }

    /// Adds one holder to every committed entry of `track_id` at `quality`
    /// (any manifest) and returns their keys, for the caller to unprotect.
    /// Taken under the `entries` lock, so no write can evict one in between.
    pub fn protect_track(&self, track_id: u64, quality: Quality) -> Vec<CacheKey> {
        let entries = lock(&self.entries);
        let mut protected = lock(&self.protected);
        let keys: Vec<CacheKey> = entries
            .keys()
            .filter(|key| key.track_id == track_id && key.quality == quality)
            .copied()
            .collect();
        for key in &keys {
            *protected.entry(*key).or_insert(0) += 1;
        }
        keys
    }

    pub fn is_protected(&self, key: &CacheKey) -> bool {
        lock(&self.protected).contains_key(key)
    }

    fn total(&self) -> u64 {
        self.committed_bytes
            .load(Ordering::SeqCst)
            .saturating_add(self.pending.load(Ordering::SeqCst))
    }

    /// Removes the least recently used unprotected entry. Call with the
    /// `entries` lock held; takes `protected` itself (lock order).
    fn evict_one(
        &self,
        entries: &mut HashMap<CacheKey, MemEntry>,
        evicted: &mut Vec<MemEntry>,
    ) -> bool {
        let victim = {
            let protected = lock(&self.protected);
            entries
                .iter()
                .filter(|(key, _)| !protected.contains_key(key))
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
        };
        let Some(key) = victim else {
            return false;
        };
        if let Some(entry) = entries.remove(&key) {
            tracing::debug!("memory cache: evicting {}", key.file_name());
            sub_saturating(&self.committed_bytes, entry.buf.len());
            evicted.push(entry);
        }
        true
    }

    /// Makes room for `n` more pending bytes, then counts them. Never
    /// refuses: with only protected or in-flight data left, the total goes
    /// over the limit.
    fn reserve(&self, n: u64) {
        let mut evicted = Vec::new();
        {
            let mut entries = lock(&self.entries);
            while self.total().saturating_add(n) > self.max_bytes()
                && self.evict_one(&mut entries, &mut evicted)
            {}
            self.pending.fetch_add(n, Ordering::SeqCst);
        }
        drop(evicted);
    }

    fn evict_to_fit(&self) {
        let mut evicted = Vec::new();
        {
            let mut entries = lock(&self.entries);
            while self.total() > self.max_bytes() && self.evict_one(&mut entries, &mut evicted) {}
        }
        drop(evicted);
    }

    /// Publishes a finished writer's buffer under its key, with no copy.
    fn commit_writer(&self, key: CacheKey, buf: Arc<ChunkBuf>, bytes: u64) {
        let replaced = {
            let mut entries = lock(&self.entries);
            let entry = MemEntry {
                buf,
                last_used: self.tick(),
            };
            let replaced = entries.insert(key, entry);
            if let Some(old) = &replaced {
                sub_saturating(&self.committed_bytes, old.buf.len());
            }
            sub_saturating(&self.pending, bytes);
            self.committed_bytes.fetch_add(bytes, Ordering::SeqCst);
            replaced
        };
        drop(replaced);
        lock(&self.in_flight).remove(&key);
        self.evict_to_fit();
    }

    fn release_writer(&self, key: &CacheKey, bytes: u64) {
        {
            let _entries = lock(&self.entries);
            sub_saturating(&self.pending, bytes);
        }
        lock(&self.in_flight).remove(key);
    }

    fn release_scratch(&self, bytes: u64) {
        let _entries = lock(&self.entries);
        sub_saturating(&self.pending, bytes);
    }
}

/// An in-flight whole-track download into the memory cache. Dropping it
/// without `commit` aborts it and frees its bytes.
pub struct MemWriter {
    cache: Arc<MemoryCache>,
    key: CacheKey,
    buf: Arc<ChunkBuf>,
    bytes: u64,
    done: bool,
}

impl MemWriter {
    pub fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.cache.reserve(data.len() as u64);
        self.buf.append(data);
        self.bytes += data.len() as u64;
        Ok(())
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The buffer being written, for the feeder to read back.
    pub fn buf(&self) -> Arc<ChunkBuf> {
        self.buf.clone()
    }

    pub fn commit(mut self) {
        self.done = true;
        self.cache
            .commit_writer(self.key, self.buf.clone(), self.bytes);
    }

    pub fn abort(self) {
        // `Drop` releases the bytes and the key.
    }
}

impl Drop for MemWriter {
    fn drop(&mut self) {
        if !self.done {
            self.cache.release_writer(&self.key, self.bytes);
        }
    }
}

/// A RAM scratch buffer for a run-ahead download that is not cached. Never
/// published. Its bytes count as pending for as long as the buffer lives:
/// the feeder reads a finished download until playback reaches its end, so
/// the release happens when the last holder drops (see `ChunkBuf`'s `Drop`).
pub struct MemScratch {
    cache: Arc<MemoryCache>,
    buf: Arc<ChunkBuf>,
}

impl MemScratch {
    pub fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.cache.reserve(data.len() as u64);
        self.buf.append(data);
        Ok(())
    }

    /// The buffer being written, for the feeder to read back.
    pub fn buf(&self) -> Arc<ChunkBuf> {
        self.buf.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_across_chunk_boundaries_reads_back_exactly() {
        let buf = ChunkBuf::new();
        let data: Vec<u8> = (0..(CHUNK_BYTES * 2 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        for piece in data.chunks(CHUNK_BYTES / 3 + 5) {
            buf.append(piece);
        }
        assert_eq!(buf.len(), data.len() as u64);
        let mut out = Vec::new();
        ChunkReader::new(buf.clone()).read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn reader_sees_bytes_appended_after_it_was_created() {
        let buf = ChunkBuf::new();
        buf.append(b"abc");
        let mut r = ChunkReader::new(buf.clone());
        let mut out = [0u8; 8];
        assert_eq!(r.read(&mut out).unwrap(), 3);
        assert_eq!(r.read(&mut out).unwrap(), 0);
        buf.append(b"de");
        assert_eq!(r.read(&mut out).unwrap(), 2);
        assert_eq!(&out[..2], b"de");
    }

    #[test]
    fn seek_from_start_current_and_end() {
        let buf = ChunkBuf::new();
        buf.append(b"0123456789");
        let mut r = ChunkReader::new(buf);
        assert_eq!(r.seek(SeekFrom::End(-3)).unwrap(), 7);
        assert_eq!(r.seek(SeekFrom::Current(-2)).unwrap(), 5);
        let mut b = [0u8; 1];
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"5");
        assert!(r.seek(SeekFrom::Current(-100)).is_err());
    }

    #[test]
    fn media_source_is_seekable_and_reports_the_current_length() {
        let buf = ChunkBuf::new();
        buf.append(b"abcd");
        let r = ChunkReader::new(buf.clone());
        assert!(r.is_seekable());
        assert_eq!(r.byte_len(), Some(4));
        buf.append(b"ef");
        assert_eq!(r.byte_len(), Some(6));
    }

    #[test]
    fn read_at_copies_at_most_the_remaining_bytes() {
        let buf = ChunkBuf::new();
        assert!(buf.is_empty());
        buf.append(b"hello");
        assert!(!buf.is_empty());
        let mut out = [0u8; 8];
        assert_eq!(buf.read_at(3, &mut out), 2);
        assert_eq!(&out[..2], b"lo");
        assert_eq!(buf.read_at(5, &mut out), 0);
        assert_eq!(buf.read_at(99, &mut out), 0);
    }

    // ---- MemoryCache ----

    use oxitide_tidal::stream::Quality;

    fn key(id: u64) -> CacheKey {
        CacheKey {
            track_id: id,
            quality: Quality::Lossless,
            manifest_hash: 0xfeed,
        }
    }

    fn commit_entry(cache: &Arc<MemoryCache>, k: &CacheKey, n: usize) {
        let mut w = cache.begin_write(k).expect("no writer in flight");
        w.write_all(&vec![k.track_id as u8; n]).unwrap();
        w.commit();
    }

    fn read_all(mut r: ChunkReader) -> Vec<u8> {
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn commit_then_lookup_reads_the_bytes() {
        let cache = MemoryCache::new(1 << 20);
        let mut w = cache.begin_write(&key(1)).unwrap();
        w.write_all(b"hello").unwrap();
        assert_eq!(w.bytes(), 5);
        assert_eq!(w.buf().len(), 5);
        w.commit();
        assert_eq!(read_all(cache.lookup(&key(1)).unwrap()), b"hello");
        assert_eq!(cache.size_bytes(), 5);
        assert_eq!(cache.pending_bytes(), 0);
    }

    #[test]
    fn abort_and_drop_free_pending() {
        let cache = MemoryCache::new(1 << 20);
        let mut w = cache.begin_write(&key(1)).unwrap();
        w.write_all(&[0; 10]).unwrap();
        assert_eq!(cache.pending_bytes(), 10);
        w.abort();
        assert_eq!(cache.pending_bytes(), 0);
        assert!(cache.lookup(&key(1)).is_none());

        let mut w = cache.begin_write(&key(1)).unwrap();
        w.write_all(&[0; 10]).unwrap();
        assert_eq!(cache.pending_bytes(), 10);
        drop(w);
        assert_eq!(cache.pending_bytes(), 0);
        assert!(cache.lookup(&key(1)).is_none());
        assert_eq!(cache.size_bytes(), 0);
    }

    #[test]
    fn second_writer_for_the_same_key_is_refused() {
        let cache = MemoryCache::new(1 << 20);
        let first = cache.begin_write(&key(1)).unwrap();
        assert!(cache.begin_write(&key(1)).is_none());
        first.commit();
        assert!(cache.begin_write(&key(1)).is_some());
    }

    #[test]
    fn recommitting_a_key_replaces_the_entry_without_double_counting() {
        let cache = MemoryCache::new(1 << 20);
        commit_entry(&cache, &key(1), 300);
        commit_entry(&cache, &key(1), 200);
        assert_eq!(cache.size_bytes(), 200);
        assert_eq!(read_all(cache.lookup(&key(1)).unwrap()).len(), 200);
    }

    #[test]
    fn write_evicts_lru_unprotected_entries_to_stay_under_the_limit() {
        let cache = MemoryCache::new(3000);
        commit_entry(&cache, &key(1), 1000); // a
        commit_entry(&cache, &key(2), 1000); // b
        assert!(cache.lookup(&key(1)).is_some()); // now b is LRU
        let mut c = cache.begin_write(&key(3)).unwrap();
        c.write_all(&[3; 1500]).unwrap();
        assert!(cache.lookup(&key(2)).is_none());
        assert!(cache.size_bytes() + cache.pending_bytes() <= 3000);
        c.commit();
        assert!(cache.lookup(&key(1)).is_some());
        assert!(cache.lookup(&key(3)).is_some());
        assert!(cache.size_bytes() + cache.pending_bytes() <= 3000);
    }

    #[test]
    fn protected_only_write_goes_over_the_limit() {
        let cache = MemoryCache::new(1000);
        commit_entry(&cache, &key(1), 800);
        cache.protect(&key(1));
        let mut b = cache.begin_write(&key(2)).unwrap();
        assert!(b.write_all(&[2; 600]).is_ok());
        assert_eq!(cache.pending_bytes(), 600);
        assert!(cache.lookup(&key(1)).is_some());
    }

    #[test]
    fn oversized_protected_commit_is_dropped_on_unprotect() {
        let cache = MemoryCache::new(1000);
        let big = key(9);
        cache.protect(&big);
        let mut w = cache.begin_write(&big).unwrap();
        w.write_all(&[9; 3000]).unwrap();
        w.commit();
        assert!(cache.lookup(&big).is_some());
        cache.unprotect(&big);
        assert!(cache.lookup(&big).is_none());
        assert_eq!(cache.size_bytes(), 0);
    }

    #[test]
    fn set_max_bytes_keeps_protected_and_live_readers() {
        let cache = MemoryCache::new(10_000);
        commit_entry(&cache, &key(1), 1000); // a
        commit_entry(&cache, &key(2), 1000); // b
        commit_entry(&cache, &key(3), 1000); // c
        cache.protect(&key(2));
        let held = cache.lookup(&key(1)).unwrap();
        cache.set_max_bytes(1000);
        assert_eq!(cache.max_bytes(), 1000);
        assert!(cache.lookup(&key(1)).is_none());
        assert!(cache.lookup(&key(3)).is_none());
        assert!(cache.lookup(&key(2)).is_some());
        assert_eq!(read_all(held), vec![1u8; 1000]);
    }

    #[test]
    fn clear_keeps_live_readers_valid() {
        let cache = MemoryCache::new(1 << 20);
        commit_entry(&cache, &key(1), 1000);
        let held = cache.lookup(&key(1)).unwrap();
        cache.clear();
        assert_eq!(cache.size_bytes(), 0);
        assert!(cache.lookup(&key(1)).is_none());
        assert_eq!(read_all(held), vec![1u8; 1000]);
    }

    #[test]
    fn remove_drops_the_entry_and_its_bytes() {
        let cache = MemoryCache::new(1 << 20);
        commit_entry(&cache, &key(1), 400);
        commit_entry(&cache, &key(2), 100);
        cache.remove(&key(1));
        assert!(cache.lookup(&key(1)).is_none());
        assert_eq!(cache.size_bytes(), 100);
    }

    #[test]
    fn scratch_counts_as_pending_and_frees_on_drop() {
        let cache = MemoryCache::new(1 << 20);
        let mut s = cache.scratch();
        s.write_all(&[7; 700]).unwrap();
        assert_eq!(cache.pending_bytes(), 700);
        assert_eq!(read_all(ChunkReader::new(s.buf())), vec![7u8; 700]);
        assert_eq!(cache.size_bytes(), 0);
        drop(s);
        assert_eq!(cache.pending_bytes(), 0);
        assert_eq!(cache.size_bytes(), 0);
    }

    /// The feeder keeps reading a finished download's scratch until playback
    /// reaches its end: those bytes are still RAM in use, so they count until
    /// the buffer's last holder drops, not when the writer does.
    #[test]
    fn scratch_bytes_count_until_the_last_reader_drops() {
        let cache = MemoryCache::new(1 << 20);
        let mut s = cache.scratch();
        s.write_all(&[7; 700]).unwrap();
        let reader = ChunkReader::new(s.buf());
        drop(s); // the download finished
        assert_eq!(
            cache.pending_bytes(),
            700,
            "a finished scratch still in use stopped counting"
        );
        assert_eq!(read_all(reader), vec![7u8; 700]);
        assert_eq!(cache.pending_bytes(), 0);
    }

    #[test]
    fn scratch_write_evicts_like_a_cache_write() {
        let cache = MemoryCache::new(1500);
        commit_entry(&cache, &key(1), 1000);
        let mut s = cache.scratch();
        s.write_all(&[0; 1000]).unwrap();
        assert!(cache.lookup(&key(1)).is_none());
    }

    #[test]
    fn protection_is_counted_per_holder() {
        let cache = MemoryCache::new(1000);
        let a = key(1);
        commit_entry(&cache, &a, 800);
        cache.protect(&a);
        cache.protect(&a);
        cache.unprotect(&a);
        assert!(cache.is_protected(&a));
        let mut b = cache.begin_write(&key(2)).unwrap();
        b.write_all(&[2; 600]).unwrap();
        assert!(cache.lookup(&a).is_some());
        cache.unprotect(&a);
        assert!(!cache.is_protected(&a));
        assert!(cache.lookup(&a).is_none());
        drop(b);
    }

    #[test]
    fn unprotect_without_protect_is_a_no_op() {
        let cache = MemoryCache::new(1 << 20);
        let a = key(1);
        cache.unprotect(&a);
        cache.protect(&a);
        assert!(cache.is_protected(&a));
    }
}
