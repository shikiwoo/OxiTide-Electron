// Shim for @tauri-apps/api/window in the Electron shell. Linux uses the
// native frame, so these only back WindowControls/drag code paths if reached.
import { shell } from "./bridge";

const current = {
  minimize: () => shell.win("minimize").then(() => {}),
  toggleMaximize: () => shell.win("toggleMaximize").then(() => {}),
  close: () => shell.win("close").then(() => {}),
  isMaximized: () => shell.win("isMaximized") as Promise<boolean>,
  startDragging: async () => {},
  onResized: async (cb: () => void) => shell.onResized(cb),
};

export function getCurrentWindow() {
  return current;
}
