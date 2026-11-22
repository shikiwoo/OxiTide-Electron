// Shim for @tauri-apps/plugin-opener in the Electron shell.
import { shell } from "./bridge";

export function openUrl(url: string | URL): Promise<void> {
  return shell.openUrl(String(url));
}
