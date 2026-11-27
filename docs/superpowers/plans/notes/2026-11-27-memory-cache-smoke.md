# Memory cache mode — owner smoke

**Status:** open. Built on `ram-cache` (plan `2026-11-27-oxitide-memory-cache.md`, spec
`2026-11-27-oxitide-memory-cache-design.md`); not yet run.

## What changed

- Settings → Storage → Cache has a **Cache storage** switch (Disk / Memory), applied at the next
  start. In Memory mode no audio bytes are written to disk: cached tracks, in-flight downloads and
  the seek scratch all live in RAM, within a budget of 256–4096 MB (default 512).
- Offline playback across restarts needs the disk cache; Memory mode has none.
- The disk cache folder is left alone while Memory mode runs, and is used again after switching
  back.

## Setup

1. Settings → Storage → Cache → Cache storage → **Memory**. The "Takes effect after you restart
   OxiTide" note shows.
2. Restart the app.
3. Windows, in separate terminals:
   - `scripts/smoke/watch-log.ps1` (network fetches and errors);
   - `scripts/smoke/watch-cache.ps1 -Mode new -Seconds 600 -CacheDir "$env:APPDATA\OxiTide\config\cache","$env:TEMP"`;
   - `scripts/smoke/watch-cache.ps1 -Mode spill -Seconds 600 -CacheDir "$env:APPDATA\OxiTide\config\cache","$env:TEMP"`;
   - an elevated `scripts/smoke/net-toggle.ps1` for the network row.

   `-CacheDir` taking several folders is new in this change and has not been run yet: if the
   watcher errors, note it here and check the two folders by hand.

## Checks (Windows)

- [ ] Settings shows "Memory" and "In memory", the usage row reads "Memory usage", and the usage
      readout moves while a track plays. The clear-on-exit toggle is not shown.
- [ ] Play a track, seek within it, and skip back to the previous track: the previous track starts
      with no network fetch (`watch-log.ps1`).
- [ ] Once the playing track has fully downloaded (the usage readout stops growing), seek within it
      and back: neither seek makes a network fetch. A seek before that point cancels the running
      download, so it is expected to fetch.
- [ ] A network toggle mid-track (`net-toggle.ps1`: `down`, wait, `up`) is ridden out in place.
- [ ] A hi-res track longer than the limit (e.g. set the limit to 256 MB) plays to the end, and
      once it has fully downloaded, seeking back within it makes no network fetch.
- [ ] During all of the above, the cache folder and `%TEMP%` gain no `.part`, `.fmp4` or `spill-*`
      files (both watchers stay quiet).
- [ ] Lowering the Memory limit while a track plays keeps it playing.
- [ ] Clear cache while a track plays: playback continues, and the usage readout drops to the
      in-flight bytes.
- [ ] Switching back to Disk and restarting brings the old disk entries back (a track cached before
      the switch replays with no fetch).

## Linux

The same checks, set up as in `docs/linux-build.md`. The `scripts/smoke/` watchers are
PowerShell, so check the files with a shell command instead. Before starting the app:

```bash
touch /tmp/oxitide-stamp
```

After each check:

```bash
find ~/.config/oxitide/cache "${TMPDIR:-/tmp}" -maxdepth 1 -newer /tmp/oxitide-stamp \( -name '*.part' -o -name '*.fmp4' -o -name 'spill-*' \)
```

Expected: no output. (`~/.config/oxitide/cache` is the default cache folder, from
`config.rs::default_cache_dir`; use `cache.dir` if it is set.) `docs/linux-build.md` names no
network toggle, so toggle the link with `nmcli networking off` / `nmcli networking on`.

- [ ] Linux: all of the above pass.

## Machine checks (2026-11-27, `oxitide-dev` distrobox, Ubuntu 24.04)

At `4546a8e` (the last code commit before the final review). After the review fixes (seek hold, scratch accounting) the Rust gates were re-run: fmt and clippy clean, `cargo test --workspace` 1337 passed, 0 failed, `--doc` exit 0; no UI code changed. The `4546a8e` results:

- `cargo fmt --all -- --check`: exit 0.
- `cargo clippy --workspace --all-targets -- -D warnings`: exit 0.
- `cargo test --workspace`: 1335 passed, 0 failed (app 545, audio 518 + 2, tidal 270); `--doc`: exit 0.
- `npm --prefix ui run check`: 0 errors, 0 warnings.
- `npm --prefix ui test`: 189 files, 1562 tests passed.
- `node scripts/i18n/check.mjs`: 0 errors, 210 warnings (unchanged by this work). The 8 new
  messages in the 23 shipped languages are machine drafts (status `machine`), not yet reviewed.

Not run here: the Windows build and its gates (`cargo test`/clippy on Windows compile the
`cfg(windows)` code), CI's nextest profile, and the updated `watch-cache.ps1` (no PowerShell
on this machine).
