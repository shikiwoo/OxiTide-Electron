// Exposes the narrow bridge the UI shims use (ui/src/shell/*). Nothing else
// from Node or Electron reaches the page.
"use strict";

const { contextBridge, ipcRenderer } = require("electron");

function unraw(value) {
  // automix_render returns raw bytes: base64 over the socket, ArrayBuffer here
  // (what Tauri's invoke resolves for an ipc::Response).
  if (value && typeof value === "object" && typeof value.__raw === "string") {
    const bin = atob(value.__raw);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return bytes.buffer;
  }
  return value;
}

contextBridge.exposeInMainWorld("oxiShell", {
  invoke: (cmd, args) =>
    ipcRenderer.invoke("oxi:invoke", cmd, args).then((value) => {
      if (value && typeof value === "object" && "__err" in value) throw value.__err;
      return unraw(value);
    }),
  listen: (event) => ipcRenderer.send("oxi:listen", event),
  onEvent: (cb) => {
    ipcRenderer.on("oxi:event", (_e, event, payload) => cb(event, payload));
  },
  openUrl: (url) => ipcRenderer.invoke("oxi:open-url", url),
  win: (op) => ipcRenderer.invoke("oxi:win", op),
  onResized: (cb) => {
    const handler = () => cb();
    ipcRenderer.on("oxi:resized", handler);
    return () => ipcRenderer.removeListener("oxi:resized", handler);
  },
});
