# Memory Cache Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Memory cache backend, chosen in Settings and applied at the next start, in which no audio bytes are written to disk.

**Architecture:** A new `mem_cache.rs` in `oxitide-audio` holds a chunked shared byte buffer, a RAM cache with LRU eviction, a budget, protection and a single-writer rule, plus RAM scratch buffers. `cache.rs` gains a `TrackCache` facade over `DiskCache` (unchanged behaviour) and `MemoryCache`, and hands out readers (`CacheSource`, `SpillReader`) instead of paths. The engine and `fetch.rs` switch to the facade. The app picks the backend from `CacheConfig` at startup.

**Tech Stack:** Rust 1.99.1 (std only, no new crates), symphonia `MediaSource`, Tauri 2 commands, Svelte 5 runes, vitest.

**Spec:** `docs/superpowers/specs/2026-11-27-oxitide-memory-cache-design.md`

## Global Constraints

- In Memory mode no audio bytes are written to disk: no `.part`, no `.fmp4`, no `.meta`, no `spill-*.part`, and no fallback to `std::env::temp_dir()`.
- Disk stays the default (`backend = "disk"`); Disk-mode behaviour and its existing tests are unchanged. The one exception is the protection leak fixed in Task 4 (Review Focus 7), which applies to both backends.
- Prerequisite: `git` on `PATH` in the shell that runs the plan. The commit steps need it, and on 2026-11-27 an agent shell in this checkout could not find it.
- `CacheConfig` gains `backend` (default `"disk"`) and `memory_max_size_mb` (default **512**). The Memory range in Settings is **256 MB to 4 GB**.
- In Memory mode, committed + pending bytes count against the limit. Playback is never refused or degraded for the budget; going over the limit is allowed only by protected or in-flight data.
- The backend is chosen at startup; changing it saves the config and shows a "takes effect after restart" note. There is no live switch.
- Switching to Memory leaves the disk cache folder untouched: `cleanup()` is a no-op in Memory mode. "Clear cache" clears the running backend only.
- AGENTS.md rules: new commands go in `generate_handler!`, sync ones go in `SYNC_COMMANDS` with a reason, every command is classified in `remote/rpc.rs` `ALLOWLIST`/`NOT_EXPOSED`, all text goes through `t()` with an en key and a translator note, the UI uses semantic tokens only, and `api.ts` is kept in sync by hand.
- Gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `npm --prefix ui run check`, `npm --prefix ui test`, `node scripts/i18n/check.mjs`.

## Review Focus

1. **Seek mid-track in Memory mode.** That takes the scratch path, and a seek must still create no file in the cache dir or `%TEMP%`. Test: Task 4, `memory_mode_seek_writes_no_files`.
2. **A track bigger than the whole limit** (a 300 MB hi-res track with a 256 MB limit). It plays to the end, seeking back works, and it is dropped from the cache once it is unprotected. Test: Task 2, `oversized_protected_commit_is_dropped_on_unprotect`.
3. **Lowering the Memory limit live while a track plays.** The protected entry and its readers survive; the others are evicted. Test: Task 2, `set_max_bytes_keeps_protected_and_live_readers`.
4. **A typo in `cache.backend` in config.toml** (e.g. `"ram"`). It must fall back to Disk with a warning, not reset the whole config through the malformed-config backup path. Test: Task 5, `unknown_cache_backend_falls_back_to_disk`.
5. **"Clear cache" while a track plays in Memory mode.** Playback continues from the live reader, and the usage readout drops to the in-flight bytes. Test: Task 2, `clear_keeps_live_readers_valid`.
6. **Two holders of one key** (repeat-one prefetches the playing track). *Correction after the final review (2026-11-27): a seek does not overlap two holders. It ends the session, so the old stream unprotects before the new one protects again, and an over-budget entry was evicted in that gap. The fix is a seek hold (`TrackCache::hold_track`, released by the new session); test `memory_mode_seek_keeps_an_over_budget_playing_entry`.* Disk protection is a plain set, so the first `unprotect` clears it for both. Disk mode rarely evicts, so it never mattered there. Memory mode evicts on every write and on every unprotect, so the playing entry would drop out of the cache and a seek back would download it again. Memory protection is therefore a count per key. Test: Task 2, `protection_is_counted_per_holder`.
7. **A stream dropped while its track is still resolving.** The decode thread protects the key only after `resolve` returns (`engine.rs` line 1440, line 1549 offline). If the `TrackStream` dropped first, its `Drop` found an empty slot, and the late `protect` was never undone. With counted protection that leak is permanent, and in Memory mode a leaked entry can never be evicted and may hold RAM over the limit. Task 4 closes the slot on drop. Test: Task 4, `stream_dropped_during_resolve_leaves_no_protection`.

---

### Task 1: Chunked shared buffer and reader

**Files:**
- Create: `crates/oxitide-audio/src/mem_cache.rs` (buffer part)
- Modify: `crates/oxitide-audio/src/lib.rs` (add `pub mod mem_cache;`)
- Test: unit tests in `mem_cache.rs`

**Interfaces:**
- Produces:
  - `pub const CHUNK_BYTES: usize = 1 << 20;`
  - `pub struct ChunkBuf`, which is `Send + Sync`, has interior mutability, and stores `Vec<Box<[u8]>>` chunks of `CHUNK_BYTES` plus `len`:
    - `ChunkBuf::new() -> Arc<ChunkBuf>`
    - `fn append(&self, data: &[u8])`
    - `fn len(&self) -> u64`
    - `fn read_at(&self, pos: u64, out: &mut [u8]) -> usize` (copies at most `len - pos` bytes and never blocks)
  - `pub struct ChunkReader { buf: Arc<ChunkBuf>, pos: u64 }`:
    - `ChunkReader::new(buf: Arc<ChunkBuf>) -> Self`
    - `impl Read`
    - `impl Seek` (`SeekFrom::End` is relative to the current `len`; a seek before 0 is an `InvalidInput` error)
    - `impl symphonia::core::io::MediaSource` (`is_seekable` is true and `byte_len` is `Some(len)`)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn append_across_chunk_boundaries_reads_back_exactly() {
    let buf = ChunkBuf::new();
    let data: Vec<u8> = (0..(CHUNK_BYTES * 2 + 17)).map(|i| (i % 251) as u8).collect();
    for piece in data.chunks(CHUNK_BYTES / 3 + 5) { buf.append(piece); }
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
```

- [ ] **Step 2: Run the tests and check they fail**

Run: `cargo test -p oxitide-audio mem_cache`
Expected: fails to compile (`ChunkBuf` is not defined).

- [ ] **Step 3: Implement `ChunkBuf` and `ChunkReader`**

Guard the chunks and `len` with an `RwLock` (many readers, one appender). Fill the last chunk before allocating a new one, so growing never copies existing bytes.

- [ ] **Step 4: Run the tests and check they pass**

Run: `cargo test -p oxitide-audio mem_cache`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/oxitide-audio/src/mem_cache.rs crates/oxitide-audio/src/lib.rs
git commit -m "audio: chunked shared buffer for the memory cache"
```

---

### Task 2: MemoryCache (entries, writer, scratch, budget)

**Files:**
- Modify: `crates/oxitide-audio/src/mem_cache.rs`
- Test: unit tests in `mem_cache.rs`

**Interfaces:**
- Consumes: Task 1 (`ChunkBuf`, `ChunkReader`); `crate::cache::CacheKey`.
- Produces:
  - `pub struct MemoryCache`. Its fields: `max_bytes: AtomicU64`, `committed_bytes: AtomicU64`, `pending: AtomicU64`, `entries: Mutex<HashMap<CacheKey, MemEntry>>` (`MemEntry { buf: Arc<ChunkBuf>, last_used: u64 }`), `protected: Mutex<HashMap<CacheKey, u32>>` (a holder count per key; see Review Focus 6), `in_flight: Mutex<HashSet<CacheKey>>`, and `clock: AtomicU64`.
  - `MemoryCache::new(max_bytes: u64) -> Arc<MemoryCache>`
  - `fn lookup(&self, key: &CacheKey) -> Option<ChunkReader>` (bumps `last_used`)
  - `fn begin_write(self: &Arc<Self>, key: &CacheKey) -> Option<MemWriter>` (`None` if a writer is already in flight for that key)
  - `fn scratch(self: &Arc<Self>) -> MemScratch`
  - `fn remove(&self, key: &CacheKey)`
  - `fn clear(&self)`
  - `fn set_max_bytes(&self, max: u64)`
  - `fn max_bytes(&self) -> u64`
  - `fn size_bytes(&self) -> u64` (committed bytes)
  - `fn pending_bytes(&self) -> u64` (in-flight plus scratch bytes)
  - `fn protect(&self, key: &CacheKey)` adds one holder.
  - `fn unprotect(&self, key: &CacheKey)` removes one holder. The count saturates at zero, so unprotecting an unprotected key is a no-op. When the count reaches zero the key leaves the map and `evict_to_fit` runs.
  - `fn is_protected(&self, key: &CacheKey) -> bool` is true while the count is above zero.
  - This refines the spec's "`protect`/`unprotect` work as on disk": callers still pair one `unprotect` with each `protect`, but two holders of one key no longer cancel each other. `DiskCache` keeps its set.
  - `pub struct MemWriter`:
    - `fn write_all(&mut self, data: &[u8]) -> io::Result<()>`
    - `fn bytes(&self) -> u64`
    - `fn buf(&self) -> Arc<ChunkBuf>` (for the feeder)
    - `fn commit(self)`, `fn abort(self)`
    - `impl Drop` (drop without commit means abort)
  - `pub struct MemScratch`:
    - `fn write_all(&mut self, data: &[u8]) -> io::Result<()>`
    - `fn buf(&self) -> Arc<ChunkBuf>`
    - `impl Drop` (subtracts its bytes from `pending`)

**Budget algorithm (pinned by the spec):**

```text
reserve(n):  // called by MemWriter::write_all and MemScratch::write_all before appending
  while committed + pending + n > max:
    evict the unprotected committed entry with the smallest last_used; stop if none
  pending += n          // always: playback is never refused

commit(writer):
  move writer.buf into entries[key] (no copy); pending -= len; committed += len
  evict_to_fit()        // unprotected entries, LRU first

evict_to_fit():         // also run by set_max_bytes and unprotect
  while committed + pending > max: evict the LRU unprotected entry; stop if none
```

Evicting only drops the map's `Arc`; a live `ChunkReader` keeps the bytes until it drops. The sidecar is not stored.

An entry bigger than the whole limit: by the time it commits, `reserve` has already evicted every unprotected entry while it downloaded, so `commit`'s `evict_to_fit` finds nothing more to evict. The entry then stays only while it is protected. The spec's "kept only if eviction can bring the total back under" needs no extra rule.

- [ ] **Step 1: Write the failing tests**

Use 1 KiB entries and small limits. Each test's name and assertions:

- `commit_then_lookup_reads_the_bytes`: write `b"hello"`, commit; `lookup` reads `b"hello"`; `size_bytes() == 5`, `pending_bytes() == 0`.
- `abort_and_drop_free_pending`: a writer writes 10 bytes and `pending_bytes() == 10`; after abort, `0` and `lookup` is `None`. The same holds after dropping a writer that was never committed.
- `second_writer_for_the_same_key_is_refused`: `begin_write(&k)` twice, the second is `None`; after the first commits, `begin_write(&k)` is `Some` again.
- `write_evicts_lru_unprotected_entries_to_stay_under_the_limit`: limit 3000; commit `a`, `b` (1000 bytes each), look up `a` (now `b` is LRU), then write 1500 bytes for `c` and commit. `b` is evicted, `a` and `c` remain, and `size_bytes() + pending_bytes() <= 3000`.
- `protected_only_write_goes_over_the_limit`: limit 1000; commit and protect `a` (800 bytes); write 600 bytes for `b`. `write_all` returns `Ok`, `pending_bytes() == 600`, and `a` is still present.
- `oversized_protected_commit_is_dropped_on_unprotect`: limit 1000; protect `big`, write 3000 bytes, commit. `lookup(big)` is `Some` while protected. After `unprotect(big)`, `lookup(big)` is `None` and `size_bytes() == 0`.
- `set_max_bytes_keeps_protected_and_live_readers`: commit `a`, `b`, `c` at 1000 bytes each with limit 10_000; protect `b`; hold a `ChunkReader` from `lookup(a)`; `set_max_bytes(1000)`. Then `lookup(a)` and `lookup(c)` are `None`, `lookup(b)` is `Some`, and the held reader still reads all 1000 bytes of `a`.
- `clear_keeps_live_readers_valid`: hold a reader from `lookup(a)`, call `clear()`. `size_bytes() == 0`, `lookup(a)` is `None`, and the reader reads all bytes.
- `scratch_counts_as_pending_and_frees_on_drop`: a scratch writes 700 bytes, so `pending_bytes() == 700`; after the drop, `0`. A scratch never shows up in `lookup`.
- `scratch_write_evicts_like_a_cache_write`: limit 1500; commit `a` (1000 bytes, unprotected); a scratch writes 1000 bytes, so `a` is evicted.
- `protection_is_counted_per_holder`: limit 1000; commit `a` (800 bytes); `protect(a)` twice, then `unprotect(a)` once. `is_protected(a)` is still true, and a 600-byte write for `b` leaves `a` in place. After the second `unprotect(a)`, `is_protected(a)` is false and `a` is evicted (800 + 600 > 1000).
- `unprotect_without_protect_is_a_no_op`: `unprotect(a)` on a key that was never protected, then `protect(a)` once. `is_protected(a)` is true, so the count did not go below zero.

- [ ] **Step 2: Run the tests and check they fail**

Run: `cargo test -p oxitide-audio mem_cache`
Expected: fails to compile (`MemoryCache` is not defined).

- [ ] **Step 3: Implement `MemoryCache`, `MemWriter` and `MemScratch` following the budget algorithm above**

`last_used` comes from `clock.fetch_add(1)`, a logical clock, never wall time. Take the locks in a fixed order (`entries`, then `protected`), and never hold a lock across `ChunkBuf::append`.

- [ ] **Step 4: Run the tests and check they pass**

Run: `cargo test -p oxitide-audio mem_cache`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/oxitide-audio/src/mem_cache.rs
git commit -m "audio: in-memory track cache with a RAM budget"
```

---

### Task 3: TrackCache facade; engine and fetch read through it (Disk behaviour unchanged)

**Files:**
- Modify: `crates/oxitide-audio/src/cache.rs` (add the facade and enums; `DiskCache` and `CacheWriter` keep their behaviour)
- Modify: `crates/oxitide-audio/src/fetch.rs` (`AheadSink`, `download`, `download_urls`, `download_ahead`, `feed`)
- Modify: `crates/oxitide-audio/src/engine.rs`: the field types at lines 243, 489 and 1150; `decode_from_file` (line 1723) becomes `decode_from_cache`; the cache-hit path at line 1444; `play_cached_offline` at line 1530; the writer and scratch setup at lines 1624–1652; and `test_cache()` at line 4113
- Test: the existing suites in `cache.rs`, `fetch.rs` and `engine.rs`

**Interfaces:**
- Consumes: Task 2 (`MemoryCache`, `MemWriter`, `MemScratch`, `ChunkReader`, `ChunkBuf`).
- Produces (in `cache.rs`):
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)] #[serde(rename_all = "lowercase")] pub enum CacheBackend { Disk, Memory }`
  - `pub enum TrackCache { Disk(Arc<DiskCache>), Memory(Arc<MemoryCache>) }`:
    - `TrackCache::disk(dir: PathBuf, max_bytes: u64) -> Arc<TrackCache>`
    - `TrackCache::memory(max_bytes: u64) -> Arc<TrackCache>`
    - `fn backend(&self) -> CacheBackend`
    - `fn usable(&self) -> bool` (always true for Memory)
    - `fn dir(&self) -> Option<&Path>` (`None` for Memory)
    - `fn lookup(&self, key: &CacheKey) -> Option<CacheSource>`
    - `fn find_offline(&self, track_id: u64, quality: Quality) -> Option<(CacheKey, PathBuf, Option<Vec<u8>>)>` (Memory returns `None`)
    - `fn begin_write(&self, key: &CacheKey) -> Option<TrackWriter>`
    - `remove`, `clear`, `cleanup` (a no-op for Memory), `set_max_bytes`, `max_bytes`, `size_bytes`, `pending_bytes`, `protect`, `unprotect` and `is_protected`, with the same signatures as on `DiskCache`
  - `pub enum CacheSource { File(crate::demux::FileSource), Memory(ChunkReader) }`:
    - `impl Read + Seek + MediaSource`
    - `fn len(&self) -> u64`
    - `CacheSource::open_file(path: &Path) -> io::Result<CacheSource>`
  - `pub enum TrackWriter { Disk(CacheWriter), Memory(MemWriter) }`:
    - `fn set_meta(&mut self, meta: Vec<u8>)` (ignored for Memory)
    - `fn write_all(&mut self, data: &[u8]) -> io::Result<()>`
    - `fn bytes(&self) -> u64`
    - `fn commit(self)`, `fn abort(self)`
    - `fn spill_reader(&self) -> io::Result<SpillReader>` (Disk opens `part_path()`; Memory gives a `ChunkReader` over `buf()`)
  - `pub enum SpillReader { File(std::fs::File), Memory(ChunkReader) }`, `impl Read + Send`
- Produces (in `fetch.rs`):
  - `pub enum AheadSink { Cache(TrackWriter), Scratch(ScratchFile), MemScratch(MemScratch) }`
  - `fn reader(&self) -> io::Result<SpillReader>` replaces `fn path()`.
  - `download(.., cache_writer: Option<TrackWriter>)`
  - `feed(buffer, reader: SpillReader, progress)`
- `DiskCache::new` stays public (for tests). The app and engine hold `Arc<TrackCache>`.

- [ ] **Step 0: Record the audio test count before touching anything**

Run: `cargo test -p oxitide-audio 2>&1 | grep "test result"` and note the passed count for each suite. Step 2 compares against it.

- [ ] **Step 1: Switch the types and call sites**

Point the existing `fetch.rs` and `engine.rs` tests at `TrackCache::disk(..)` and `AheadSink::Cache(TrackWriter::Disk(..))`, so they compile against the new API. They stay Disk-only for now. Then change the production types as listed under Interfaces.

`decode_from_cache(.., source: CacheSource, ..)` builds `total_bytes` from `source.len()`. `play_cached_offline` keeps its path: it calls `plan_from_file(&path, ..)`, then `CacheSource::open_file(&path)`.

- [ ] **Step 2: Run the whole audio suite and check it passes unchanged**

Run: `cargo test -p oxitide-audio`
Expected: PASS, with the same counts as recorded in Step 0.

- [ ] **Step 3: Switch the app crate and run clippy on the workspace**

The app crate names `DiskCache` and won't compile until it is switched. Change these to `TrackCache` and `TrackCache::disk(..)` in this task, leaving the behaviour as it is:
- `state.rs`: lines 6, 46, 177 and 181.
- `commands.rs`: the import at line 3; `require_cache` at line 89; and the `cache` parameters of `replace_player` (line 422) and `build_player` (line 437).
- `player.rs`: the import at line 3 and `build_cache` at line 19. The `build_cache` test at line 570 compares `cache.dir()` with a path; it becomes `Some(dir.as_path())`.
- `commands.rs` `get_cache_status` at line 2909: `cache.dir()` is now an `Option`, so report `""` for `None` (Task 5 relies on this for Memory mode).

`main.rs` needs no edit; its calls (`set_cache(build_cache(..))`, `cleanup`, `clear`) pick up the new type.

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/oxitide-audio crates/oxitide-app/src/state.rs crates/oxitide-app/src/commands.rs crates/oxitide-app/src/player.rsgit commit -m "audio: TrackCache facade; cache hits and spill read through readers, not paths"
```

---

### Task 4: Memory-mode scratch in the engine; protection slot; end-to-end Memory tests

**Files:**
- Modify: `crates/oxitide-audio/src/engine.rs`: scratch selection at lines 1636–1652; the cache-key slot (`TrackStream.cache_key` at line 1152, its `Drop` at line 1277, and the two places that fill it, lines 1440 and 1549)
- Test: `crates/oxitide-audio/src/fetch.rs` tests, `crates/oxitide-audio/src/engine.rs` tests

**Interfaces:**
- Consumes: Task 3 (`TrackCache::memory`, `AheadSink::MemScratch`, `TrackCache::backend`). Also `MemoryCache::scratch` via `TrackCache::Memory(m) => m.scratch()`; add `pub fn memory_scratch(&self) -> Option<MemScratch>` on `TrackCache` (`None` for Disk).
- Produces: no new public API. Inside `engine.rs`, `Arc<Mutex<Option<CacheKey>>>` becomes a private `ProtectSlot`:
  - `struct ProtectSlot { state: Mutex<SlotState> }`, where `enum SlotState { Empty, Held(CacheKey), Closed }`.
  - `fn hold(&self, cache: &TrackCache, key: CacheKey)`: under the lock, `Empty` becomes `Held(key)` and calls `cache.protect(&key)`. On `Closed` it does nothing, because the stream is already gone.
  - `fn close(&self, cache: &TrackCache)`: under the lock, `Held(key)` calls `cache.unprotect(&key)`, and any state becomes `Closed`. `TrackStream::drop` calls it.
  - Lines 1440–1441 and 1549–1550 call `slot.hold(..)` in place of setting the slot and calling `protect`.
  - This applies to both backends. In Disk mode it only changes the leak case (Review Focus 7), where a key used to stay protected after its stream was gone.

- [ ] **Step 1: Write the failing tests**

- **Fixtures for these tests.** `fixture_plan()` (line 3982) has one segment and `manifest_hash: None`, so it yields no cache key. Use these instead:
  - `cached_plan(url, hash)` (line 4139) for tests that need a key. Pair it with `CountingFetcher` (line 4158) for the fetch count.
  - `he_aac_fixture(..)` (line 4007, `manifest_hash: Some(0x96)`, init plus two media segments) for the seek test, which needs a second segment to seek into.
- In `engine.rs`, `stream_dropped_during_resolve_leaves_no_protection`:
  - Build the engine on a `TrackCache::memory(..)` with `GatedResolver` (line 6082) serving `cached_plan(..)`.
  - Start a track with `PlayTracks` and wait until it is resolving (`wait_for_order`).
  - Send `EngineCmd::Clear` so its `TrackStream` drops, then `gate.release(..)`.
  - Wait for the decode thread to finish. Assert `!cache.is_protected(&key)`, where `key` is built from the plan's `manifest_hash`, the item's track id and the engine's quality.
  - Without `ProtectSlot` the late `protect` sticks and the test fails.

- In `fetch.rs`, `download_ahead_into_memory_publishes_the_whole_track`: this mirrors `download_ahead_fetches_the_whole_track_without_a_consumer` (line 1121), using `TrackCache::memory(1 << 30)`. Assert that `cache.lookup(&key)` reads back exactly the fixture bytes.
- In `fetch.rs`, `memory_scratch_rides_out_a_network_drop`: this mirrors the existing Scratch network-drop test (the one using `ScratchFile` at line 1228), using `AheadSink::MemScratch(..)`. Assert the same buffer output, and that `pending_bytes() == 0` after the sink drops.
- In `engine.rs`, add a helper `fn own_spill_files() -> usize`. It counts the files directly in `std::env::temp_dir()`, not recursively, whose name starts with `spill-{std::process::id()}-`. That is the only place Memory mode could leak to, since there is no cache dir. Disk-mode tests spill into their own cache subdirectories, so they don't affect the count.
- In `engine.rs`, `memory_mode_replay_is_a_cache_hit_and_writes_no_files`: build the engine with `TrackCache::memory(64 << 20)`, `cached_plan(..)` and `CountingFetcher`. Play a track to the end, then play it again. Assert that the second play fires `AudioPerfEvent::CacheHit` with no fetch (fetch counter unchanged), and that `own_spill_files()` is the same before and after.
- In `engine.rs`, `memory_mode_seek_writes_no_files`: in Memory mode, start a track with `seek_ms = Some(..)`. Don't sample `pending_bytes()` while the download runs: the download can finish before the test looks, so such an assertion fails at random (AGENTS.md treats a flaky test as a bug). Instead, hold the download at a known point:
  - Use `he_aac_fixture(..)` and seek to a position inside its first media segment (`mem://head`). `seek_ms` being `Some` is what routes the download to scratch (`engine.rs` line 1623), so the plan stays init, head and tail.
  - Use a fetcher that serves `mem://init` and `mem://head`, then blocks on `mem://tail` until a `Gate` is released. That could be a new `HoldTailFetcher`, or `GatedFetcher` (line 7684) gating only one URL.
  - While it is held, wait for PCM. Assert that `cache.pending_bytes()` equals the init plus head lengths (scratch bytes count as pending) and that `own_spill_files()` is unchanged.
  - Release the gate and let the track end. Then assert `pending_bytes() == 0`, `size_bytes() == 0` (a seek-truncated plan is never cached) and that `own_spill_files()` is still unchanged.

- [ ] **Step 2: Run the tests and check they fail**

Run: `cargo test -p oxitide-audio memory_` and `cargo test -p oxitide-audio stream_dropped_during_resolve`
Expected: the Memory-mode engine tests fail (a `spill-*.part` file is created), and the resolve test fails (the key is still protected).

- [ ] **Step 3: Choose `AheadSink::MemScratch(scratch)` when `inner.cache.memory_scratch()` is `Some`, else today's `ScratchFile` path**

Memory mode never reaches the `temp_dir()` fallback.

- [ ] **Step 3b: Add `ProtectSlot` and switch `TrackStream` and the two fill sites to it**

- [ ] **Step 4: Run the tests and check they pass**

Run: `cargo test -p oxitide-audio`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/oxitide-audio/src/engine.rs crates/oxitide-audio/src/fetch.rs
git commit -m "audio: memory mode keeps scratch downloads in RAM; close the protection slot on drop"
```

---

### Task 5: Config, startup wiring and commands

**Files:**
- Modify: `crates/oxitide-app/src/config.rs` (`CacheConfig`, around line 721)
- Modify: `crates/oxitide-app/src/player.rs` (`build_cache`)
- Modify: `crates/oxitide-app/src/main.rs` (`generate_handler!`, around line 620; startup `cleanup` around line 423 and `clear_on_exit` around line 696 go unchanged through the facade)
- Modify: `crates/oxitide-app/src/commands.rs` (`CacheStatusDto`, `get_cache_status` at line 2906, the new setters next to line 3371, `SYNC_COMMANDS` at line 3648)
- Modify: `crates/oxitide-app/src/remote/rpc.rs` (`NOT_EXPOSED` at line 740, next to `set_cache_max_size_mb`)
- Test: `config.rs` tests, `player.rs` tests (add if absent), and the existing `commands.rs`/`rpc.rs` classification tests

**Interfaces:**
- Consumes: Task 3 (`TrackCache`, `CacheBackend`).
- Produces:
  - In `config.rs`, `CacheConfig { dir, max_size_mb, clear_on_exit, backend: CacheBackendSetting, memory_max_size_mb: u64 }`. The defaults are `backend = Disk` and `memory_max_size_mb = 512`.
    - `CacheBackendSetting` is an app-side enum `{ Disk, Memory }`, serialized lowercase.
    - Its `Deserialize` is lenient: an unknown string logs `warn!` and becomes `Disk`.
    - It has `fn to_backend(self) -> oxitide_audio::cache::CacheBackend`.
  - `pub const MEMORY_CACHE_MIN_MB: u64 = 256; pub const MEMORY_CACHE_MAX_MB: u64 = 4096;`
  - `build_cache(config: &Config) -> Arc<TrackCache>`: Memory gives `TrackCache::memory(clamped memory_max_size_mb * MiB)`; Disk is today's logic.
  - `CacheStatusDto` gains `backend: CacheBackend` and `configured_backend: CacheBackend`. `dir` is `""` in Memory mode.
  - `#[tauri::command] pub fn set_cache_backend(state, backend: CacheBackendSetting) -> Result<(), String>` saves the config only and doesn't touch the running cache.
  - `#[tauri::command] pub fn set_cache_memory_max_size_mb(state, mb: u64) -> Result<(), String>` clamps to 256..=4096, saves, and calls `set_max_bytes` only when the running backend is Memory.
  - `set_cache_max_size_mb` calls `set_max_bytes` only when the running backend is Disk.
  - Both new commands go in `SYNC_COMMANDS` with the reason "config writer: settings read-modify-write the whole config", and in `NOT_EXPOSED`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn cache_config_defaults_to_disk_with_a_512_mb_memory_limit() {
    let c = CacheConfig::default();
    assert_eq!(c.backend, CacheBackendSetting::Disk);
    assert_eq!(c.memory_max_size_mb, 512);
}

#[test]
fn cache_backend_round_trips_through_toml() {
    let mut config = Config::default();
    config.cache.backend = CacheBackendSetting::Memory;
    let text = toml::to_string(&config).unwrap();
    assert!(text.contains("backend = \"memory\""));
    let back: Config = toml::from_str(&text).unwrap();
    assert_eq!(back.cache.backend, CacheBackendSetting::Memory);
}

#[test]
fn unknown_cache_backend_falls_back_to_disk() {
    let back: Config = toml::from_str("[cache]\nbackend = \"ram\"\nmax_size_mb = 1024\n").unwrap();
    assert_eq!(back.cache.backend, CacheBackendSetting::Disk);
    assert_eq!(back.cache.max_size_mb, 1024); // the rest of the section survives
}
```

In `player.rs`, `build_cache_memory_backend_is_memory_and_clamped`: with backend Memory and `memory_max_size_mb = 10`, `build_cache(&config)` returns `backend() == CacheBackend::Memory`, `max_bytes() == 256 * 1024 * 1024` and `dir() == None`.

- [ ] **Step 2: Run the tests and check they fail**

Run: `cargo test -p oxitide-app cache_`
Expected: fails to compile (`CacheBackendSetting` is not defined).

- [ ] **Step 3: Implement the config fields, `build_cache`, the DTO fields and the two commands, and register them**

Register them in `generate_handler!`, `SYNC_COMMANDS` and `NOT_EXPOSED`.

- [ ] **Step 4: Run the app tests and check they pass**

Run: `cargo test -p oxitide-app`
Expected: PASS, including the `SYNC_COMMANDS` and remote-classification tests.

- [ ] **Step 5: Commit**

```bash
git add crates/oxitide-app
git commit -m "app: cache.backend and cache.memory_max_size_mb; memory cache at startup"
```

---

### Task 6: Settings UI, text and design doc

**Files:**
- Modify: `ui/src/lib/api.ts`:
  - `CacheStatus` (line 496) gains `backend: "disk" | "memory"` and `configured_backend: "disk" | "memory"`.
  - The config `cache` type (line 576) gains `backend` and `memory_max_size_mb`.
  - `api` (near line 1033) gains `setCacheBackend(backend)` and `setCacheMemoryMaxSizeMb(mb)`.
- Create: `ui/src/lib/cacheSettings.ts` and `ui/src/lib/cacheSettings.test.ts` (node)
- Modify: `ui/src/views/Settings.svelte` (the cache section at lines 1731–1795; the handlers next to `setCacheMaxMb` at line 865)
- Modify: `ui/src/lib/settingsIndex.ts` (add a `cache-backend` entry next to line 127)
- Modify: `ui/src/locales/en/app.json`, `ui/src/locales/notes/app.json`
- Modify: `ui/design/DESIGN.md` (the Settings → Storage description)

**Interfaces:**
- Consumes: Task 5 (the command names `set_cache_backend` and `set_cache_memory_max_size_mb`, args `{ backend }` and `{ mb }`, and the `CacheStatus` fields).
- Produces: `cacheSettings.ts`:
  - `export const MEMORY_MIN_MB = 256`, `export const MEMORY_MAX_MB = 4096`
  - `export function cacheControls(status: CacheStatus, configured: "disk" | "memory"): { showLocation: boolean; showClearOnExit: boolean; restartNote: boolean; sizeField: "disk" | "memory" }`
  - `export function clampMemoryMb(mb: number): number`

- [ ] **Step 1: Write the failing tests**

```ts
import { describe, expect, it } from "vitest";
import { cacheControls, clampMemoryMb } from "./cacheSettings";

const status = (backend: "disk" | "memory") => ({
  dir: backend === "disk" ? "C:/c" : "", used_bytes: 0, pending_bytes: 0, max_bytes: 1,
  usable: true, configured_dir: "", backend, configured_backend: backend,
});

describe("cacheControls", () => {
  it("disk running and configured shows location and clear-on-exit, no note", () => {
    expect(cacheControls(status("disk"), "disk"))
      .toEqual({ showLocation: true, showClearOnExit: true, restartNote: false, sizeField: "disk" });
  });
  it("memory running hides location and clear-on-exit", () => {
    expect(cacheControls(status("memory"), "memory"))
      .toEqual({ showLocation: false, showClearOnExit: false, restartNote: false, sizeField: "memory" });
  });
  it("switching to memory shows the restart note and the memory size field", () => {
    const c = cacheControls(status("disk"), "memory");
    expect(c.restartNote).toBe(true);
    expect(c.sizeField).toBe("memory");
  });
});

describe("clampMemoryMb", () => {
  it("clamps to 256..4096 and floors", () => {
    expect(clampMemoryMb(10)).toBe(256);
    expect(clampMemoryMb(9000)).toBe(4096);
    expect(clampMemoryMb(700.9)).toBe(700);
  });
});
```

`showLocation` and `showClearOnExit` follow the *running* backend; `sizeField` and `restartNote` follow the *configured* one.

- [ ] **Step 2: Run the tests and check they fail**

Run: `npm --prefix ui test -- src/lib/cacheSettings.test.ts`
Expected: FAIL (the module is not found).

- [ ] **Step 3: Implement `cacheSettings.ts`, the `api.ts` additions and the Settings section**

The Settings section has:
- A `Segmented` row, `id="cache-backend"`, at the top of the cache section, with options Disk / Memory. It calls `api.setCacheBackend` and updates `config.cache.backend`.
- The restart note as an inline notice when `restartNote` is true.
- The size `NumberField` bound to `max_size_mb` or `memory_max_size_mb` per `sizeField`. The memory field gets `min={MEMORY_MIN_MB}` and `max={MEMORY_MAX_MB}`.
- The location row and clear-on-exit row gated by the flags.
- In Memory mode, a location line reading "In memory" and an offline hint line.

New en keys, each with a note in `locales/notes/app.json`:

| Key | Text |
|-----|------|
| `settings.cacheBackend.label` | "Cache storage" |
| `settings.cacheBackend.disk` | "Disk" |
| `settings.cacheBackend.memory` | "Memory" |
| `settings.cacheBackend.description` | "Memory keeps cached tracks in RAM only, so nothing is written to the drive. Offline playback needs the disk cache." |
| `settings.cacheBackend.restart` | "Takes effect after you restart OxiTide." |
| `settings.cacheLocation.memory` | "In memory" |
| `settings.keywords.cacheBackend` | "ram memory ssd disk drive storage writes" |

Update the `DESIGN.md` Settings → Storage text to describe the storage choice and the controls that are hidden in Memory mode.

- [ ] **Step 4: Run the UI gates**

Run: `npm --prefix ui test`, then `npm --prefix ui run check`, then `node scripts/i18n/check.mjs`
Expected: all three exit 0.

- [ ] **Step 5: Commit**

```bash
git add ui
git commit -m "ui: choose disk or memory cache storage in Settings"
```

---

### Task 7: Agent docs, smoke checklist, full gates

**Files:**
- Modify: `docs/agents/audio.md` (add a cache bullet next to the `download_ahead` bullet at line 16)
- Modify: `docs/agents/app-shell.md` (persisted config: the `cache.backend` and `memory_max_size_mb` fields, and that the backend applies at start)
- Create: `docs/superpowers/plans/notes/2026-11-27-memory-cache-smoke.md` with a `**Status:** open` line
- Modify: `scripts/smoke/watch-cache.ps1` (watch several folders), `scripts/smoke/README.md`

**Interfaces:** none.

- [ ] **Step 1: Write the docs**

In `audio.md`, one bullet covering:
- `TrackCache` with Disk and Memory backends;
- Memory keeps entries, in-flight writes and scratch data in `ChunkBuf`s;
- the budget counts pending bytes and only protected data may exceed it;
- `find_offline` is `None` and `cleanup` is a no-op in Memory mode.

In `app-shell.md`, one line on the config fields and that the backend applies at the next start.

- [ ] **Step 2: Write the smoke checklist**

First, let `scripts/smoke/watch-cache.ps1` watch more than one folder. Today its `-CacheDir` parameter takes one folder (default `%APPDATA%\OxiTide\config\cache`), and Memory mode's only possible leak is `%TEMP%`.
- Make `-CacheDir` a `string[]` (keeping the default) and loop over it in both modes.
- In `spill` mode, also report new `.part` files.
- Update the usage lines at the top of the script, and `scripts/smoke/README.md`.

For Windows, with backend Memory and the app restarted:
- [ ] Settings shows "Memory" and "In memory", and the usage readout moves while a track plays.
- [ ] Play, seek and skip back to the previous track: it starts with no network fetch (watch with `scripts/smoke/watch-log.ps1`).
- [ ] Once the playing track has fully downloaded (the usage readout stops growing), seek within it and back: neither seek makes a network fetch. A seek before that point cancels the running download, so it is expected to fetch.
- [ ] A network toggle mid-track (`scripts/smoke/net-toggle.ps1`) is ridden out in place.
- [ ] A hi-res track longer than the limit plays to the end.
- [ ] During all of the above, the cache dir and `%TEMP%` gain no `.part`, `.fmp4` or `spill-*` files. Run `watch-cache.ps1 -Mode new` and `-Mode spill`, both with `-CacheDir "$env:APPDATA\OxiTide\config\cache","$env:TEMP"`.
- [ ] Switching back to Disk and restarting brings the old disk entries back.

The Linux run makes the same checks, set up as in `docs/linux-build.md`. The `scripts/smoke/` watchers are PowerShell, so the checklist gives a shell command for the file check. Run it from a stamp file made before the app starts (`touch /tmp/oxitide-stamp`), over the Linux cache dir (from `config.rs::default_cache_dir`, line 940) and `${TMPDIR:-/tmp}`:

```bash
find "$CACHE_DIR" "${TMPDIR:-/tmp}" -maxdepth 1 -newer /tmp/oxitide-stamp \( -name '*.part' -o -name '*.fmp4' -o -name 'spill-*' \)
```

Expected: no output. `docs/linux-build.md` names no network toggle, so toggle the link with `nmcli networking off` / `on` and record that in the checklist.

- [ ] **Step 3: Run all the gates**

Run:
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `npm --prefix ui run check`
- `npm --prefix ui test`
- `node scripts/i18n/check.mjs`

Expected: all exit 0. Then run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` in WSL2 per `docs/linux-build.md`, since the spec requires a Linux pass before merge. Expected: both exit 0.

- [ ] **Step 4: Commit**

```bash
git add docs scripts/smoke
git commit -m "docs: memory cache mode in the agent guides; smoke checklist"
```
