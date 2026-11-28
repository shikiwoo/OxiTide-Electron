## 1.2.1 beta 2

This is a beta. It is a prerelease: installed copies are not offered it as an update, so download it from this page to try it. Everything in beta 1 is included.

### Memory cache mode

- **Settings → Storage → Cache → Cache storage** has a new choice: **Memory**. Cached tracks, downloads in progress and the downloads after a seek are then kept in RAM only, so no audio is written to your drive. **Disk** stays the default.
- The change takes effect after you restart OxiTide; the Settings page says so until then.
- Memory has its own size limit, 256 MB to 4 GB (512 MB by default), separate from the disk cache's. The track that is playing and the next one are never cut off for it: they may take the total over the limit while they play.
- Replays and skipping back to a recent track still start without downloading again, as long as the track is still in memory.
- Offline playback needs the disk cache: in Memory mode, nothing cached survives a restart. Your existing disk cache folder is left alone and is used again if you switch back to Disk.

### Fixes

- Skipping a track while it was still loading could leave its cache entry protected, so it was never evicted. Fixed in both cache modes.

### Known gaps

- The new Settings text is machine-translated in all languages except English and has not been reviewed yet.
