// Which Settings → Storage → Cache controls apply. The running backend
// decides what exists now (a folder, an exit sweep); the configured one,
// which a switch changes at once, decides the size field and whether the
// restart note shows (the backend applies at the next start).
import type { CacheBackend, CacheStatus } from "./api";

/// The Memory cache's range in MB (`MEMORY_CACHE_MIN_MB`/`_MAX_MB` in config.rs).
export const MEMORY_MIN_MB = 256;
export const MEMORY_MAX_MB = 4096;

export interface CacheControls {
  showLocation: boolean;
  showClearOnExit: boolean;
  restartNote: boolean;
  sizeField: CacheBackend;
}

export function cacheControls(status: CacheStatus, configured: CacheBackend): CacheControls {
  const onDisk = status.backend === "disk";
  return {
    showLocation: onDisk,
    showClearOnExit: onDisk,
    restartNote: configured !== status.backend,
    sizeField: configured,
  };
}

export function clampMemoryMb(mb: number): number {
  return Math.min(MEMORY_MAX_MB, Math.max(MEMORY_MIN_MB, Math.floor(mb)));
}
