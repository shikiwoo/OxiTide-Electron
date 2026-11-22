//! Electron shell mode (`OXITIDE_SHELL=electron`).
//!
//! The Rust backend runs unchanged, but its own WebKitGTK window stays hidden
//! on a blank page and never renders the UI. The UI runs in an Electron
//! (Chromium) window instead, and talks to this process over a Unix socket that
//! Electron creates and passes in `OXITIDE_BRIDGE_SOCK`.
//!
//! Wire format: one JSON object per line, in both directions.
//!
//! Electron → backend:
//! - `{"t":"invoke","id":N,"cmd":"name","args":{…}}`: runs the command through
//!   Tauri's normal IPC path (`Webview::on_message`), so ACL, argument parsing
//!   and sync/async dispatch behave exactly as they do for the webview.
//! - `{"t":"listen","event":"name"}`: forward that event from now on.
//! - `{"t":"deeplink","url":"tidal://…"}`: a link Electron received (second
//!   launch); handled like the deep-link plugin's.
//! - `{"t":"close"}`: the user closed the window (close-to-tray or quit).
//! - `{"t":"quit"}`: exit the app.
//!
//! Backend → Electron:
//! - `{"t":"res","id":N,"ok":<json>}`, `{"t":"res","id":N,"raw":"<base64>"}`,
//!   or `{"t":"res","id":N,"err":<json>}`
//! - `{"t":"ev","event":"name","payload":<json>}`
//! - `{"t":"shell","kind":"show"|"hide"|"toggle"|"mini","data":<json>}`: window
//!   requests that used to act on the Tauri window (tray, second launch,
//!   MPRIS Raise, mini player).

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;
use tauri::ipc::{CallbackFn, InvokeBody, InvokeResponse, InvokeResponseBody};
use tauri::webview::InvokeRequest;
use tauri::{AppHandle, Listener, Manager};

/// The blank page the hidden Tauri window loads in shell mode.
pub const BLANK_PAGE: &str = "shell-blank.html";

/// Whether this process runs as the Electron shell's backend.
pub fn electron() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OXITIDE_SHELL").is_ok_and(|v| v == "electron"))
}

struct Bridge {
    out: Mutex<UnixStream>,
    listening: Mutex<HashSet<String>>,
}

static BRIDGE: OnceLock<Arc<Bridge>> = OnceLock::new();

impl Bridge {
    fn send_line(&self, line: &str) {
        let mut out = match self.out.lock() {
            Ok(out) => out,
            Err(poisoned) => poisoned.into_inner(),
        };
        if out
            .write_all(line.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .is_err()
        {
            // Electron is gone; the reader thread exits the app.
            tracing::debug!("shell bridge write failed");
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Incoming {
    Invoke {
        id: u64,
        cmd: String,
        #[serde(default)]
        args: Value,
    },
    Listen {
        event: String,
    },
    Deeplink {
        url: String,
    },
    Close,
    Quit,
}

/// Connect to Electron's socket and start serving it. Call from `setup`.
pub fn start(app: &AppHandle) {
    let Ok(path) = std::env::var("OXITIDE_BRIDGE_SOCK") else {
        tracing::error!("OXITIDE_SHELL=electron without OXITIDE_BRIDGE_SOCK");
        app.exit(2);
        return;
    };
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!("cannot connect to the Electron shell at {path}: {e}");
            app.exit(2);
            return;
        }
    };
    let reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(e) => {
            tracing::error!("shell bridge clone failed: {e}");
            app.exit(2);
            return;
        }
    };
    let bridge = Arc::new(Bridge {
        out: Mutex::new(stream),
        listening: Mutex::new(HashSet::new()),
    });
    let _ = BRIDGE.set(bridge.clone());
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("oxitide-shell-bridge".into())
        .spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Incoming>(&line) {
                    Ok(msg) => handle(&app, &bridge, msg),
                    Err(e) => tracing::warn!("bad shell bridge message: {e}"),
                }
            }
            tracing::info!("Electron shell disconnected; exiting");
            app.exit(0);
        });
    if let Err(e) = spawned {
        tracing::error!("shell bridge thread failed: {e}");
    }
}

fn handle(app: &AppHandle, bridge: &Arc<Bridge>, msg: Incoming) {
    match msg {
        Incoming::Invoke { id, cmd, args } => invoke(app, bridge.clone(), id, cmd, args),
        Incoming::Listen { event } => {
            let fresh = bridge
                .listening
                .lock()
                .map(|mut set| set.insert(event.clone()))
                .unwrap_or(false);
            if fresh {
                let bridge = bridge.clone();
                let name = event.clone();
                app.listen_any(event, move |ev| {
                    let name_json = serde_json::to_string(&name).unwrap_or_default();
                    let payload = if ev.payload().is_empty() {
                        "null"
                    } else {
                        ev.payload()
                    };
                    bridge.send_line(&format!(
                        "{{\"t\":\"ev\",\"event\":{name_json},\"payload\":{payload}}}"
                    ));
                });
            }
        }
        Incoming::Deeplink { url } => crate::handle_deep_link(app, &url),
        Incoming::Close => {
            if app
                .state::<crate::state::AppState>()
                .config()
                .ui
                .close_to_tray
            {
                notify(app, "hide", Value::Null);
            } else {
                app.exit(0);
            }
        }
        Incoming::Quit => app.exit(0),
    }
}

fn invoke(app: &AppHandle, bridge: Arc<Bridge>, id: u64, cmd: String, args: Value) {
    let app2 = app.clone();
    // The main thread, like a real webview message: sync commands keep their
    // arrival order and main-thread affinity (see AGENTS.md, SYNC_COMMANDS).
    let queued = app.run_on_main_thread(move || {
        let Some(webview) = app2.get_webview_window("main") else {
            bridge.send_line(&format!(
                "{{\"t\":\"res\",\"id\":{id},\"err\":\"no main window\"}}"
            ));
            return;
        };
        let url = match webview.url() {
            Ok(url) => url,
            Err(e) => {
                let msg = serde_json::to_string(&e.to_string()).unwrap_or_default();
                bridge.send_line(&format!("{{\"t\":\"res\",\"id\":{id},\"err\":{msg}}}"));
                return;
            }
        };
        let request = InvokeRequest {
            cmd,
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url,
            body: InvokeBody::Json(if args.is_null() {
                Value::Object(Default::default())
            } else {
                args
            }),
            headers: Default::default(),
            invoke_key: app2.invoke_key().to_string(),
        };
        let out = bridge.clone();
        webview.on_message(
            request,
            Box::new(move |_webview, _cmd, response, _ok, _err| {
                let line = match response {
                    InvokeResponse::Ok(InvokeResponseBody::Json(json)) => {
                        format!("{{\"t\":\"res\",\"id\":{id},\"ok\":{json}}}")
                    }
                    InvokeResponse::Ok(InvokeResponseBody::Raw(bytes)) => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                        format!("{{\"t\":\"res\",\"id\":{id},\"raw\":\"{b64}\"}}")
                    }
                    InvokeResponse::Err(error) => {
                        let json =
                            serde_json::to_string(&error.0).unwrap_or_else(|_| "null".into());
                        format!("{{\"t\":\"res\",\"id\":{id},\"err\":{json}}}")
                    }
                };
                out.send_line(&line);
            }),
        );
    });
    if let Err(e) = queued {
        tracing::warn!("shell invoke {id} could not reach the main thread: {e}");
    }
}

/// Ask Electron to do something with its window. A no-op outside shell mode.
pub fn notify(_app: &AppHandle, kind: &str, data: Value) {
    let Some(bridge) = BRIDGE.get() else { return };
    let kind = serde_json::to_string(kind).unwrap_or_default();
    bridge.send_line(&format!(
        "{{\"t\":\"shell\",\"kind\":{kind},\"data\":{data}}}"
    ));
}
