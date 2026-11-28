import { describe, expect, it } from "vitest";
import type { CacheBackend, CacheStatus } from "./api";
import { cacheControls, clampMemoryMb, MEMORY_MAX_MB, MEMORY_MIN_MB } from "./cacheSettings";

const status = (backend: CacheBackend): CacheStatus => ({
  dir: backend === "disk" ? "C:/c" : "",
  used_bytes: 0,
  pending_bytes: 0,
  max_bytes: 1,
  usable: true,
  configured_dir: "",
  backend,
  configured_backend: backend,
});

describe("cacheControls", () => {
  it("disk running and configured shows location and clear-on-exit, no note", () => {
    expect(cacheControls(status("disk"), "disk")).toEqual({
      showLocation: true,
      showClearOnExit: true,
      restartNote: false,
      sizeField: "disk",
    });
  });

  it("memory running hides location and clear-on-exit", () => {
    expect(cacheControls(status("memory"), "memory")).toEqual({
      showLocation: false,
      showClearOnExit: false,
      restartNote: false,
      sizeField: "memory",
    });
  });

  it("switching to memory shows the restart note and the memory size field", () => {
    const c = cacheControls(status("disk"), "memory");
    expect(c.restartNote).toBe(true);
    expect(c.sizeField).toBe("memory");
    // The running disk cache still has a folder and an exit sweep.
    expect(c.showLocation).toBe(true);
    expect(c.showClearOnExit).toBe(true);
  });

  it("switching back to disk before a restart shows the note", () => {
    const c = cacheControls(status("memory"), "disk");
    expect(c.restartNote).toBe(true);
    expect(c.sizeField).toBe("disk");
    expect(c.showLocation).toBe(false);
  });
});

describe("clampMemoryMb", () => {
  it("clamps to 256..4096 and floors", () => {
    expect(clampMemoryMb(10)).toBe(256);
    expect(clampMemoryMb(9000)).toBe(4096);
    expect(clampMemoryMb(700.9)).toBe(700);
  });

  it("matches the backend's range", () => {
    expect(MEMORY_MIN_MB).toBe(256);
    expect(MEMORY_MAX_MB).toBe(4096);
  });
});
