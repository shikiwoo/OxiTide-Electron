//! macOS Now Playing and media keys (fork-only port), through `souvlaki`
//! (MPNowPlayingInfoCenter / MPRemoteCommandCenter). Remote commands arrive
//! on the main run loop, which Tauri drives; updates are pushed from a worker
//! thread like the Windows and Linux sessions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use oxitide_audio::engine::Engine;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

use super::{
    cover_url, to_engine_cmd, ControlCommand, PlayState, SessionSender, SessionUpdate, TrackMeta,
};

const COVER_SIZE: u32 = 640;
const CHANNEL_CAPACITY: usize = 64;
/// Now Playing extrapolates the position from the play state; resend it only
/// when the real position drifts this far from that (a seek).
const POSITION_RESYNC_MS: u64 = 1500;
/// Seek step for the "skip forward/back" commands without an amount.
const SEEK_STEP_MS: u64 = 10_000;

pub struct MediaSession {
    tx: Option<SyncSender<SessionUpdate>>,
    enabled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MediaSession {
    pub fn spawn(engine: Engine, enabled: bool) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel::<SessionUpdate>(CHANNEL_CAPACITY);
        let flag = Arc::new(AtomicBool::new(enabled));
        let worker_flag = flag.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let handle = std::thread::Builder::new()
            .name("oxitide-nowplaying".into())
            .spawn(move || worker(engine, rx, worker_flag, worker_shutdown))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            tx: Some(tx),
            enabled: flag,
            shutdown,
            handle: Some(handle),
        })
    }

    pub fn sender(&self) -> SessionSender {
        SessionSender::new(self.tx.as_ref().expect("live sender").clone())
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// What the session last told the OS, shared with the command handler so
/// Play/Pause (not only Toggle) can map onto the engine's PlayPause.
#[derive(Default)]
struct Shared {
    play: PlayState,
    position_ms: u64,
    duration_ms: u64,
}

fn handle_event(engine: &Engine, shared: &Mutex<Shared>, event: MediaControlEvent) {
    let (play, position, duration) = {
        let s = shared.lock().unwrap();
        (s.play, s.position_ms, s.duration_ms)
    };
    let command = match event {
        MediaControlEvent::Toggle => ControlCommand::PlayPause,
        MediaControlEvent::Play if play != PlayState::Playing => ControlCommand::PlayPause,
        MediaControlEvent::Pause | MediaControlEvent::Stop if play == PlayState::Playing => {
            ControlCommand::PlayPause
        }
        MediaControlEvent::Next => ControlCommand::Next,
        MediaControlEvent::Previous => ControlCommand::Prev,
        MediaControlEvent::SetPosition(MediaPosition(at)) => {
            ControlCommand::Seek(at.as_millis() as u64)
        }
        MediaControlEvent::Seek(direction) => {
            ControlCommand::Seek(step(position, duration, direction, SEEK_STEP_MS))
        }
        MediaControlEvent::SeekBy(direction, by) => {
            ControlCommand::Seek(step(position, duration, direction, by.as_millis() as u64))
        }
        _ => return,
    };
    let _ = engine.handle(to_engine_cmd(command));
}

fn step(position: u64, duration: u64, direction: SeekDirection, by: u64) -> u64 {
    match direction {
        SeekDirection::Forward if duration > 0 => (position + by).min(duration),
        SeekDirection::Forward => position + by,
        SeekDirection::Backward => position.saturating_sub(by),
    }
}

fn playback(play: PlayState, position_ms: u64) -> MediaPlayback {
    let progress = Some(MediaPosition(Duration::from_millis(position_ms)));
    match play {
        PlayState::Playing => MediaPlayback::Playing { progress },
        PlayState::Paused => MediaPlayback::Paused { progress },
        PlayState::Stopped => MediaPlayback::Stopped,
    }
}

fn apply_track(controls: &mut MediaControls, track: Option<&TrackMeta>) {
    let result = match track {
        Some(track) => {
            let cover = track
                .cover
                .as_deref()
                .and_then(|c| cover_url(c, COVER_SIZE));
            controls.set_metadata(MediaMetadata {
                title: Some(&track.title),
                artist: Some(&track.artist),
                album: Some(&track.album),
                cover_url: cover.as_deref(),
                duration: (track.duration_ms > 0).then(|| Duration::from_millis(track.duration_ms)),
            })
        }
        None => controls.set_metadata(MediaMetadata::default()),
    };
    if let Err(e) = result {
        tracing::warn!("Now Playing metadata failed: {e}");
    }
}

fn worker(
    engine: Engine,
    rx: Receiver<SessionUpdate>,
    enabled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let mut controls: Option<MediaControls> = None;
    let mut last_track: Option<TrackMeta> = None;
    // Position last sent to the OS, and when (for extrapolation).
    let mut sent_at: (u64, Instant) = (0, Instant::now());

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let want = enabled.load(Ordering::Relaxed);
        if want && controls.is_none() {
            let config = PlatformConfig {
                display_name: "OxiTide",
                dbus_name: "oxitide",
                hwnd: None,
            };
            match MediaControls::new(config) {
                Ok(mut created) => {
                    let handler_engine = engine.clone();
                    let handler_shared = shared.clone();
                    if let Err(e) = created
                        .attach(move |event| handle_event(&handler_engine, &handler_shared, event))
                    {
                        tracing::warn!("Now Playing commands unavailable: {e}");
                    }
                    apply_track(&mut created, last_track.as_ref());
                    let s = shared.lock().unwrap();
                    let _ = created.set_playback(playback(s.play, s.position_ms));
                    sent_at = (s.position_ms, Instant::now());
                    controls = Some(created);
                }
                Err(e) => {
                    tracing::warn!("Now Playing unavailable: {e}");
                    enabled.store(false, Ordering::Relaxed);
                }
            }
        } else if !want {
            if let Some(mut gone) = controls.take() {
                let _ = gone.set_metadata(MediaMetadata::default());
                let _ = gone.set_playback(MediaPlayback::Stopped);
                let _ = gone.detach();
            }
        }

        let update = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(update) => update,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut s = shared.lock().unwrap();
        match update {
            SessionUpdate::Track(track) => {
                if let Some(c) = controls.as_mut() {
                    apply_track(c, track.as_ref());
                }
                s.duration_ms = track.as_ref().map_or(0, |t| t.duration_ms);
                last_track = track;
            }
            SessionUpdate::Playback(play) => {
                if play != s.play {
                    s.play = play;
                    if let Some(c) = controls.as_mut() {
                        let _ = c.set_playback(playback(play, s.position_ms));
                    }
                    sent_at = (s.position_ms, Instant::now());
                }
            }
            SessionUpdate::Position {
                position_ms,
                duration_ms,
            } => {
                s.position_ms = position_ms;
                s.duration_ms = duration_ms;
                let expected = if s.play == PlayState::Playing {
                    sent_at.0 + sent_at.1.elapsed().as_millis() as u64
                } else {
                    sent_at.0
                };
                if expected.abs_diff(position_ms) > POSITION_RESYNC_MS {
                    if let Some(c) = controls.as_mut() {
                        let _ = c.set_playback(playback(s.play, position_ms));
                    }
                    sent_at = (position_ms, Instant::now());
                }
            }
        }
    }
    if let Some(mut c) = controls.take() {
        let _ = c.detach();
    }
}
