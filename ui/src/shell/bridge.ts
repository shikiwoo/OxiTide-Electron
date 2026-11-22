// Electron shell bridge: the preload script exposes `window.oxiShell`
// (electron/preload.cjs). These shims stand in for @tauri-apps/* in
// `vite build --mode electron` (see vite.config.ts).

export interface OxiShell {
  invoke(cmd: string, args?: unknown): Promise<unknown>;
  listen(event: string): void;
  onEvent(cb: (event: string, payload: unknown) => void): void;
  openUrl(url: string): Promise<void>;
  win(op: "minimize" | "toggleMaximize" | "close" | "isMaximized"): Promise<unknown>;
  onResized(cb: () => void): () => void;
}

declare global {
  interface Window {
    oxiShell: OxiShell;
  }
}

export const shell: OxiShell = window.oxiShell;

type Handler = (event: { event: string; id: number; payload: unknown }) => void;
const handlers = new Map<string, Map<number, Handler>>();
let nextId = 1;

shell.onEvent((event, payload) => {
  const set = handlers.get(event);
  if (!set) return;
  for (const [id, handler] of [...set]) handler({ event, id, payload });
});

export function addListener(event: string, handler: Handler): () => void {
  let set = handlers.get(event);
  if (!set) {
    set = new Map();
    handlers.set(event, set);
    shell.listen(event);
  }
  const id = nextId++;
  set.set(id, handler);
  return () => {
    handlers.get(event)?.delete(id);
  };
}
