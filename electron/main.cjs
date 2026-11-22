// OxiTide Electron shell (Linux).
//
// Runs the unchanged Rust backend (crates/oxitide-app) in shell mode
// (OXITIDE_SHELL=electron) and renders the Svelte UI in Chromium instead of
// WebKitGTK. The two talk over a Unix socket in $XDG_RUNTIME_DIR, created
// here with owner-only permissions: one JSON object per line (protocol in
// crates/oxitide-app/src/shell.rs).

"use strict";

const { app, BrowserWindow, ipcMain, net, protocol, shell, nativeTheme } = require("electron");
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const netSock = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const crypto = require("node:crypto");

const ROOT = path.resolve(__dirname, "..");
const UI_DIR = process.env.OXITIDE_UI_DIR || path.join(ROOT, "ui", "dist-electron");
const BACKEND =
  process.env.OXITIDE_BACKEND || path.join(ROOT, "target", "release", "oxitide-app");
const CONFIG_DIR = path.join(process.env.XDG_CONFIG_HOME || path.join(os.homedir(), ".config"), "oxitide");
const GEOMETRY_FILE = path.join(CONFIG_DIR, "electron-window.json");

// Same policy as tauri.conf.json, minus the Tauri IPC origins: the page
// itself never talks to the network except for TIDAL artwork. Inline scripts
// are allowed by hash, as Tauri does (see cspFor).
const CSP_REST = [
  "style-src 'self' 'unsafe-inline'",
  "font-src 'self' data:",
  "img-src 'self' data: blob: https://resources.tidal.com https://images.tidal.com",
  "media-src 'self' blob:",
  "connect-src 'self'",
  "object-src 'none'",
  "base-uri 'self'",
  "frame-ancestors 'none'",
];

function cspFor(html) {
  const hashes = [];
  for (const m of html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g)) {
    hashes.push(`'sha256-${crypto.createHash("sha256").update(m[1]).digest("base64")}'`);
  }
  return ["default-src 'self'", `script-src 'self' ${hashes.join(" ")}`.trim(), ...CSP_REST].join("; ");
}

app.setName("OxiTide");
// The Rust backend owns MPRIS and media keys; keep Chromium out of both.
// GPU flags stay at Chromium's defaults: they give GPU compositing on NVIDIA +
// Wayland here (the "not compatible with Vulkan" log line is harmless).
app.commandLine.appendSwitch("disable-features", "HardwareMediaKeyHandling,MediaSessionService");
app.commandLine.appendSwitch("ozone-platform-hint", "auto");

protocol.registerSchemesAsPrivileged([
  { scheme: "app", privileges: { standard: true, secure: true, supportFetchAPI: true } },
]);

if (!app.requestSingleInstanceLock()) {
  app.quit();
  process.exit(0);
}

/** @type {BrowserWindow | null} */
let win = null;
let backend = null;
let sock = null;
let quitting = false;
let miniStash = null;
let nextId = 1;
const pending = new Map();
let outbox = [];

function send(obj) {
  const line = JSON.stringify(obj) + "\n";
  if (sock) sock.write(line);
  else outbox.push(line);
}

function deepLinksIn(argv) {
  return argv.filter((a) => typeof a === "string" && a.startsWith("tidal://"));
}

// ---- window geometry ------------------------------------------------------

function loadGeometry() {
  try {
    return JSON.parse(fs.readFileSync(GEOMETRY_FILE, "utf8"));
  } catch {
    return { width: 1200, height: 800 };
  }
}

function saveGeometry() {
  if (!win || win.isDestroyed()) return;
  const bounds = miniStash ? miniStash.bounds : win.getNormalBounds();
  const maximized = miniStash ? miniStash.maximized : win.isMaximized();
  try {
    fs.mkdirSync(CONFIG_DIR, { recursive: true });
    fs.writeFileSync(GEOMETRY_FILE, JSON.stringify({ ...bounds, maximized }));
  } catch (e) {
    console.warn("[shell] cannot save window geometry:", e.message);
  }
}

function showWindow() {
  if (!win) return;
  if (win.isMinimized()) win.restore();
  win.show();
  win.focus();
}

// ---- backend bridge -------------------------------------------------------

function onBackendLine(line) {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    console.warn("[shell] bad line from backend:", line.slice(0, 200));
    return;
  }
  if (msg.t === "res") {
    const p = pending.get(msg.id);
    if (!p) return;
    pending.delete(msg.id);
    // Errors resolve as {__err} so the renderer gets Tauri's raw value back
    // (ipcMain.handle would wrap a rejection in its own Error message).
    if ("err" in msg) p.resolve({ __err: msg.err });
    else if ("raw" in msg) p.resolve({ __raw: msg.raw });
    else p.resolve(msg.ok);
  } else if (msg.t === "ev") {
    if (win && !win.isDestroyed()) win.webContents.send("oxi:event", msg.event, msg.payload);
  } else if (msg.t === "shell") {
    onShellRequest(msg.kind, msg.data);
  }
}

function onShellRequest(kind, data) {
  if (!win) return;
  switch (kind) {
    case "show":
      showWindow();
      break;
    case "hide":
      saveGeometry();
      win.hide();
      break;
    case "toggle":
      if (win.isVisible() && !win.isMinimized()) {
        saveGeometry();
        win.hide();
      } else showWindow();
      break;
    case "mini":
      if (data.active && !miniStash) {
        miniStash = { bounds: win.getNormalBounds(), maximized: win.isMaximized() };
        if (miniStash.maximized) win.unmaximize();
        win.setMinimumSize(data.minWidth, data.minHeight);
        win.setSize(data.width, data.height);
      } else if (!data.active && miniStash) {
        const stash = miniStash;
        miniStash = null;
        win.setMinimumSize(data.fullMinWidth, data.fullMinHeight);
        win.setBounds(stash.bounds);
        if (stash.maximized) win.maximize();
      }
      break;
    default:
      console.warn("[shell] unknown shell request:", kind);
  }
}

function startBackend() {
  const runtime = process.env.XDG_RUNTIME_DIR || os.tmpdir();
  const sockPath = path.join(runtime, `oxitide-shell-${process.pid}.sock`);
  try {
    fs.unlinkSync(sockPath);
  } catch {}

  const server = netSock.createServer((conn) => {
    if (sock) {
      conn.destroy(); // one backend only
      return;
    }
    sock = conn;
    server.close();
    fs.rm(sockPath, { force: true }, () => {});
    conn.setEncoding("utf8");
    let buf = "";
    conn.on("data", (chunk) => {
      buf += chunk;
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        if (line) onBackendLine(line);
      }
    });
    conn.on("close", () => {
      sock = null;
      for (const p of pending.values()) p.resolve({ __err: "backend disconnected" });
      pending.clear();
    });
    for (const line of outbox) conn.write(line);
    outbox = [];
  });
  server.listen(sockPath, () => fs.chmodSync(sockPath, 0o600));

  backend = spawn(BACKEND, [], {
    env: { ...process.env, OXITIDE_SHELL: "electron", OXITIDE_BRIDGE_SOCK: sockPath },
    stdio: ["ignore", "inherit", "inherit"],
  });
  backend.on("error", (e) => {
    console.error(`[shell] cannot start backend ${BACKEND}: ${e.message}`);
    quitting = true;
    app.quit();
  });
  backend.on("exit", (code, signal) => {
    console.log(`[shell] backend exited (code ${code}, signal ${signal})`);
    backend = null;
    quitting = true;
    app.quit();
  });
}

// ---- renderer IPC ---------------------------------------------------------

ipcMain.handle("oxi:invoke", (_e, cmd, args) => {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    send({ t: "invoke", id, cmd, args: args ?? {} });
  });
});

ipcMain.on("oxi:listen", (_e, event) => send({ t: "listen", event }));

ipcMain.handle("oxi:open-url", (_e, url) => {
  const u = new URL(url);
  if (u.protocol !== "https:" && u.protocol !== "http:") throw new Error("refused: " + u.protocol);
  return shell.openExternal(u.toString());
});

ipcMain.handle("oxi:win", (_e, op) => {
  if (!win) return null;
  switch (op) {
    case "minimize":
      return win.minimize();
    case "toggleMaximize":
      return win.isMaximized() ? win.unmaximize() : win.maximize();
    case "close":
      return win.close();
    case "isMaximized":
      return win.isMaximized();
  }
  return null;
});

// ---- app lifecycle --------------------------------------------------------

function createWindow() {
  const g = loadGeometry();
  win = new BrowserWindow({
    width: g.width,
    height: g.height,
    x: g.x,
    y: g.y,
    minWidth: 720,
    minHeight: 520,
    show: false,
    title: "OxiTide",
    backgroundColor: nativeTheme.shouldUseDarkColors ? "#0e0e11" : "#f6f6f7",
    autoHideMenuBar: true,
    icon: path.join(ROOT, "crates", "oxitide-app", "icons", "128x128.png"),
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      spellcheck: false,
    },
  });
  win.setMenu(null);
  if (g.maximized) win.maximize();

  win.once("ready-to-show", () => win.show());
  // Renderer warnings/errors land in the shell's stdout (run.sh's log).
  win.webContents.on("console-message", (e) => {
    if (e.level === "warning" || e.level === "error") {
      console.log(`[ui:${e.level}] ${e.message} (${e.sourceId}:${e.lineNumber})`);
    }
  });
  win.on("resize", () => win.webContents.send("oxi:resized"));
  win.on("close", (e) => {
    if (quitting) {
      saveGeometry();
      return;
    }
    e.preventDefault();
    saveGeometry();
    send({ t: "close" }); // backend answers "hide" (close to tray) or exits
  });

  // Never navigate away from the UI or open new windows inside the app.
  win.webContents.on("will-navigate", (e, url) => {
    if (!url.startsWith("app://oxitide/")) e.preventDefault();
  });
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (url.startsWith("https://")) shell.openExternal(url);
    return { action: "deny" };
  });

  win.loadURL("app://oxitide/index.html");
}

app.on("second-instance", (_e, argv) => {
  for (const url of deepLinksIn(argv)) send({ t: "deeplink", url });
  showWindow();
});

app.on("before-quit", () => {
  quitting = true;
  saveGeometry();
  if (sock) send({ t: "quit" });
});

app.on("window-all-closed", () => {
  if (quitting) app.quit();
});

app.on("will-quit", () => {
  if (backend) {
    // Give the backend time to flush its session/history, then make sure.
    const child = backend;
    setTimeout(() => child.kill("SIGTERM"), 3000).unref();
  }
});

app.whenReady().then(() => {
  protocol.handle("app", async (request) => {
    const { pathname } = new URL(request.url);
    const rel = decodeURIComponent(pathname).replace(/^\/+/, "") || "index.html";
    const file = path.resolve(UI_DIR, rel);
    if (!file.startsWith(UI_DIR + path.sep)) return new Response("forbidden", { status: 403 });
    const res = await net.fetch(pathToFileURL(file).toString());
    const headers = new Headers(res.headers);
    if (!rel.endsWith(".html")) return new Response(res.body, { status: res.status, headers });
    const html = await res.text();
    headers.set("Content-Type", "text/html; charset=utf-8");
    headers.set("Content-Security-Policy", cspFor(html));
    return new Response(html, { status: res.status, headers });
  });

  startBackend();
  createWindow();
  if (process.env.OXITIDE_SHELL_DEBUG) {
    // After the page is up: earlier, the GPU process may not be running yet.
    win.webContents.once("did-finish-load", () =>
      setTimeout(async () => {
        const s = app.getGPUFeatureStatus();
        const info = await app.getGPUInfo("complete").catch(() => ({}));
        console.log(
          `[shell] GPU: compositing=${s.gpu_compositing} raster=${s.rasterization} renderer=${info.auxAttributes?.glRenderer ?? "?"}`,
        );
      }, 3000),
    );
  }
  for (const url of deepLinksIn(process.argv)) send({ t: "deeplink", url });
});
