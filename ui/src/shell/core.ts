// Shim for @tauri-apps/api/core in the Electron shell.
import { shell } from "./bridge";

export type InvokeArgs = Record<string, unknown>;

export function invoke<T>(cmd: string, args?: InvokeArgs): Promise<T> {
  // Tauri serializes args as JSON; do the same, so Svelte $state proxies (which
  // Electron's structured clone rejects) arrive as plain data.
  const plain = args === undefined ? {} : JSON.parse(JSON.stringify(args));
  return shell.invoke(cmd, plain) as Promise<T>;
}

export function isTauri(): boolean {
  return true;
}
