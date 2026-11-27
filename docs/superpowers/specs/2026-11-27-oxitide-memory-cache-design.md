# OxiTide — Memory cache mode (design)

**Status:** draft, 2026-11-27. Branch `ram-cache` (cut from `linux-nv-explicit-sync` at `release: 1.2.0`).

## Intent

The owner wants to cut SSD writes during normal listening. Today every played track is written to disk in full: `fetch::download_ahead` lands the whole stream in a cache `.part` (fsynced, renamed, plus a `.meta` sidecar) or, when no cache entry can be started (a seek, a second writer, cache disabled), in a `spill-*.part` scratch file. Lowering the cache cap or disabling it does not stop the writes, because the scratch file still goes to disk.

The fix is a **Memory** cache mode: a second backend next to the existing disk cache, chosen in Settings, in which no audio bytes touch the disk.

Offline playback across restarts (`DiskCache::find_offline`, the offline startup restore from `config\tidal_session.json`) depends on cached files surviving a restart. The owner said they would hardly miss it, so Memory mode gives it up.

## Goals

- In Memory mode, no audio bytes are written to disk: in-flight downloads, committed entries and the seek/second-writer scratch data all live in RAM.
- Memory mode keeps what the disk cache gives during a session: replay and skip-back of recent tracks without a re-download, the eviction that drops the least recently used entry first, a size limit, protection of the playing and prefetched entries, and the single-writer rule per key.
- Network resilience is unchanged: the download still runs ahead of the decoder, and a lost network is ridden out in place (`fetch_recovering`, `buffering`).
- Disk stays the default; nothing changes for anyone who doesn't switch.
- Works the same on Windows and Linux.

## Non-goals

- Offline playback in Memory mode. `find_offline` returns `None`, and an unreachable TIDAL behaves like an uncached track does today.
- The small JSON caches beside the audio cache (`artwork.rs` palettes, `lyrics.rs`, `album_dates.rs`, `track_flags.rs`) and the state files (session, history, flags). They write KBs, and they stay on disk.
- WebView2 / WebKitGTK caches. They are outside the app's code.
- Switching backends live while the app runs.
- A hybrid mode (some tracks to disk).

## Anti-goals

- Memory mode that quietly still writes audio to disk anywhere (the scratch fallback to `temp_dir()` included). That defeats the point even if everything else works.
- Unbounded RAM growth. In Memory mode the limit is a RAM budget, so in-flight and scratch bytes count against it (unlike Disk, where `pending` is only reported; see "Memory budget" below). A finished session must not keep entries alive after they are evicted.
- Regressions in Disk mode's behaviour or its tests.

## Approaches considered

- **A. Full RAM cache backend (picked).** Memory becomes a second backend behind the cache API, holding entries, in-flight writes and scratch data in RAM, with eviction and a size limit. It is the only option that guarantees no audio writes, and it keeps replay without a re-download. Cost: RAM use up to the limit, and offline across restarts is lost.
- **B. Leave it to the OS.** Keep the files but mark them temporary and drop the fsync, so Windows tends to keep them in RAM. Very little code, but there is no guarantee (Windows writes them out under memory pressure or on long-lived handles), and Linux has no equivalent outside a tmpfs. Rejected: it doesn't reliably do what's asked.
- **C. Only the playing track in RAM.** No cache: the playing and prefetched tracks are held in RAM and dropped after play. Smallest RAM footprint, but every replay downloads again. Rejected as a separate design: A with a small limit gives the same behaviour.

The owner picked A.

## How it works today (what the change touches)

- `crates/oxitide-audio/src/cache.rs`, `DiskCache`. The API is path-based: `lookup(&key) -> Option<PathBuf>` (it touches mtime for the LRU order), `begin_write(&key) -> Option<CacheWriter>`, and `CacheWriter::{write_all, set_meta, commit, abort, part_path}`. Also `find_offline`, `protect`/`unprotect`, `remove`, `clear`, `cleanup`, `set_max_bytes`, `size_bytes`/`pending_bytes`, `usable`, `dir`.
- `crates/oxitide-audio/src/fetch.rs`. `AheadSink::{Cache(CacheWriter), Scratch(ScratchFile)}`. `download_ahead` reopens `sink.path()` with `File::open` for the feeder. `feed(buffer, reader: File, progress)` reads back only up to `progress.downloaded`, so the reader never runs past what the network thread has written. A memory reader therefore needs only positioned reads under a lock, with no new waiting logic.
- `crates/oxitide-audio/src/engine.rs`.
  - Around line 1444: a cache hit calls `decode_from_file(.., &path, ..)`, which builds a `demux::FileSource` and calls the generic `decode_source(.., source, ..)`.
  - Around line 1624: the writer comes from `begin_write`; otherwise a `ScratchFile` goes in `cache.dir()` or `temp_dir()`.
  - Around line 1530: `play_cached_offline` uses `find_offline`.
- `crates/oxitide-app`.
  - `player.rs::build_cache` builds the cache from `config.cache` (`CacheConfig { dir, max_size_mb = 2048, clear_on_exit }`), and `state.rs` holds the `Arc<DiskCache>`.
  - `main.rs` runs `cache.cleanup()` on the startup background thread and `cache.clear()` when `clear_on_exit` is on.
  - `commands.rs` has `get_cache_status` (`CacheStatusDto`), `clear_cache`, `set_cache_max_size_mb` and the clear-on-exit setter.
  - The UI is the Storage → Cache section of `ui/src/views/Settings.svelte`, with entries in `ui/src/lib/settingsIndex.ts`.

## Design

### Backend

- The audio crate's cache becomes one type with two backends, Disk (today's code, unchanged in behaviour) and Memory. Callers keep a single `Arc<…>` handle and don't branch on the mode. Whether that is an enum inside `DiskCache` renamed to something like `TrackCache`, or a trait object, is the builder's choice; keep the public surface close to today's so `engine.rs` and `fetch.rs` change little.
- The path-returning API turns into a reader-returning one:
  - `lookup` returns a cache-hit source that implements `Read + Seek` plus a length. For Disk it wraps `FileSource`; for Memory it reads over the entry's bytes.
  - The writer gives the feeder a reader for its in-progress bytes, instead of `part_path()` plus `File::open`.
  - `decode_from_file` becomes "decode from cache source" and keeps passing a length closure (`total_bytes`).
- **Memory entries are stored in fixed-size chunks** (e.g. 1 MiB `Vec`s behind an `Arc`), not one growing `Vec`. Growth then never copies the track, peak RAM is never twice the track, and readers can hold the entry while the writer appends. (Builder: the exact chunk size is your call.)
- **Commit in Memory mode** moves the in-flight buffer into the committed map under its key, with no copy. Abort or drop frees it. The single-writer rule (`in_flight`) and `protect`/`unprotect` work as on disk, keyed by `CacheKey` instead of path.
- **LRU order in Memory mode** uses a last-used timestamp or counter, updated on `lookup` and commit. Nothing is touched on disk.
- **Accounting** keeps committed and pending (in-flight plus scratch) bytes separately, as Disk does, and protected entries are skipped. Unlike Disk, both count against the limit (see Memory budget). An evicted entry that a reader still holds stays alive only through that reader's `Arc` and is freed when the reader drops. It no longer counts against the limit once evicted.
- **Memory budget (designer's call, after the builder check).** In Memory mode, committed + pending bytes are kept at or under the limit:
  - Whenever a write (cache writer or memory scratch) would push the total over the limit, unprotected committed entries are evicted, least recently used first, before the bytes are added. So eviction runs on writes too, not only on commit and `set_max_bytes`.
  - **Playback is never refused or degraded for the budget.** When only protected and in-flight data is left (the playing track, the prefetched one, an AutoMix next), the write goes ahead and the total may exceed the limit, by at most the data that is actually in use. The run-ahead download and network-drop recovery stay as they are.
  - When such a download commits while the total is still over the limit, it is kept only if eviction can bring the total back under. Otherwise it is dropped from the cache once it is unprotected; it is not kept as an over-limit entry.
  - The Settings readout shows used + pending against max and may briefly read over 100% in that case. That's fine.
  - The Memory range starts at **256 MB**, so a typical lossless track plus a prefetch fits; the default is 512 MB.
- **The stream sidecar** (`set_meta`) is accepted and ignored in Memory mode. It only serves `find_offline`, which returns `None` there.
- **Scratch data in Memory mode.** `AheadSink::Scratch` gets a memory variant: the same chunked buffer, not registered in the cache map, freed on drop. In Memory mode the engine never creates a `ScratchFile` and never falls back to `temp_dir()`. The pending scratch bytes are counted in `pending_bytes` so Settings shows the real RAM in use.
- **`cleanup`, `dir`, `usable`.** `cleanup()` is a no-op in Memory mode, and it doesn't touch the disk cache folder. `usable()` is always true. `dir()` is not used for anything in Memory mode; the status DTO reports the backend instead.

### Configuration

- `CacheConfig` gains `backend: "disk" | "memory"` (default `"disk"`) and `memory_max_size_mb` (default **512**).
  - A separate limit, so switching to Memory never reuses a 2 GB disk limit as a RAM budget.
  - Older config files load with the defaults (`#[serde(default)]` already covers it).
- **The backend is chosen at startup** in `build_cache`. Changing it in Settings saves the config and shows a "takes effect after restart" note; the running cache keeps its backend until then. There is no live migration.
- **`set_cache_max_size_mb`** keeps setting the disk limit; a new setter covers the memory limit. Each applies live to the running cache only when it matches the running backend. (Builder: one command with a backend argument is also fine.)
- **`clear_on_exit`** has no meaning in Memory mode. RAM is gone on exit anyway, so the toggle is hidden or disabled there.
- **Switching to Memory leaves the existing disk cache folder alone.** Its files stay, unread, and are used again if the owner switches back. "Clear cache" clears the running backend only.

### Commands, DTOs, UI

- **`CacheStatusDto`** gains `backend` (the running one) and `configured_backend` (what starts next time), so the UI can show the restart note. In Memory mode `dir` is empty, and the UI shows "In memory" instead of a path. Keep `api.ts` in sync.
- **New commands.**
  - A backend setter and a memory limit setter, both sync config writers like the existing cache setters.
  - Each is listed in `generate_handler!`, in `SYNC_COMMANDS` with a reason, and classified in the phone remote's `ALLOWLIST`/`NOT_EXPOSED` the same way `set_cache_max_size_mb` is.
- **Settings → Storage → Cache.**
  - A two-option control (Disk / Memory) at the top of the section.
  - The size control follows the configured backend, with a Memory range of 256 MB to 4 GB. The location control is hidden in Memory mode, and so is clear on exit.
  - A short line explains that offline playback needs the disk cache.
  - The usage readout works for both modes.
  - New `settingsIndex.ts` entries for search.
- **Text.** All new text goes through `t()` with keys in `ui/src/locales/en/app.json` and translator notes, and `node scripts/i18n/check.mjs` passes. Other locales follow the project's existing process for new keys.
- **Docs.** Update `ui/design/DESIGN.md` for the Settings change, and add a Memory-mode paragraph to `docs/agents/audio.md` (cache) and `docs/agents/app-shell.md` (config).

### Tests

- Memory-backend unit tests in `cache.rs`, mirroring the disk ones that apply:
  - commit and lookup
  - abort and drop free the bytes
  - single writer per key
  - eviction respects the limit and skips protected entries
  - `set_max_bytes(0)`
  - `clear`
  - pending accounting
  - plus new tests for:
    - a reader that outlives eviction stays valid, and the bytes are freed afterwards;
    - a write evicts unprotected entries to stay under the limit;
    - when only protected data is left, the write still succeeds and goes over the limit;
    - an over-limit commit is dropped once it is unprotected.
- `fetch.rs` download-ahead tests run against the memory sink as well as the file sink, including the network-drop recovery case.
- One engine-level test plays a track twice in Memory mode and checks that the second play is a cache hit with no fetch, and that no files appear in the cache dir or the temp dir during either play.
- Config round-trip with the new fields; `commands.rs` and `remote/rpc.rs` classification tests pass.
- Settings component test: the mode switch shows the restart note, and the location and clear-on-exit controls are hidden in Memory mode.

## Constraints

- AGENTS.md rules apply throughout: no new `QueueItem` literals expected; DSP untouched; `t()` for text; semantic tokens only; no width-driven JS layout.
- Engine teardown: whatever the engine owns must still drop cleanly inside an async command (`auth_restore` rebuilds the player). The memory cache holds only plain data and `Arc`s, so there is nothing to shut down.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `npm --prefix ui run check` and `npm --prefix ui test` are the gates, plus the Linux clippy and test pass from `docs/linux-build.md` before merge.
- Smoke check (human): Memory mode plays, seeks, skips back without a refetch, rides out a network toggle (`scripts/smoke/`), and the cache folder and `%TEMP%` gain no `.part` or `.fmp4` files. Record it as a checklist in `docs/superpowers/plans/notes/`.

## Left to the builder

- Enum versus trait for the two backends, the type and module names, and the chunk size.
- Whether the memory-limit setter is a separate command or shares one with the disk limit.
- The exact Settings layout within the existing section style, and the Memory size range and step.
- How the engine-level "no files written" check is made (temp dirs per test are fine).
