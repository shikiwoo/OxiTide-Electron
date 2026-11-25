//! macOS output (fork-only port): Core Audio through `cpal`, shared mode.
//!
//! The playback worker pushes f32 sample words into an `rtrb` ring
//! (`ring::push_samples`); Core Audio's render callback drains it, applies the
//! app's volume as a short ramp (`gain::GainRamp`) and pads underruns with
//! silence. The HAL output unit converts the stream's rate to the device's
//! nominal rate itself, so any track plays on any device.
//!
//! Shared mode only: macOS has no exclusive (hog) mode in this fork, so an
//! exclusive setting carried over from another platform opens shared and
//! `status` never claims bit-perfect. Not yet: following a change of the
//! system default device mid-stream.

use std::sync::atomic::{AtomicU64, Ordering};

use rtrb::Consumer;

use crate::gain::GainRamp;
use crate::ring::Starvation;

/// Ring length: 200 ms of audio, like WASAPI.
pub fn ring_samples(sample_rate: u32, channels: u16) -> usize {
    (sample_rate as usize * channels as usize / 5).max(4096)
}

/// A flush handshake between the writer and the render callback: the writer
/// bumps `requested`; the callback empties the ring and stores the value in
/// `completed` at its next period.
#[derive(Debug, Default)]
pub(crate) struct FlushRequest {
    requested: AtomicU64,
    completed: AtomicU64,
}

impl FlushRequest {
    pub fn request(&self) -> u64 {
        self.requested.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn is_done(&self, seq: u64) -> bool {
        self.completed.load(Ordering::SeqCst) >= seq
    }

    /// Callback side: the sequence to acknowledge, when a flush is pending.
    fn pending(&self) -> Option<u64> {
        let requested = self.requested.load(Ordering::SeqCst);
        (requested > self.completed.load(Ordering::SeqCst)).then_some(requested)
    }

    fn complete(&self, seq: u64) {
        self.completed.store(seq, Ordering::SeqCst);
    }
}

/// Scale f32 samples in place.
pub(crate) fn scale_f32(frame: &mut [f32], gain: f32) {
    for sample in frame {
        *sample *= gain;
    }
}

/// One render period: honour a pending flush, copy whole frames of f32 words
/// from the ring into `out`, pad with silence, count a dry-out, and apply the
/// volume ramp. Returns the frames taken from the ring.
pub(crate) fn render(
    consumer: &mut Consumer<u32>,
    out: &mut [f32],
    channels: usize,
    ramp: &mut GainRamp,
    target_gain: f32,
    flush: &FlushRequest,
    starvation: &Starvation,
) -> usize {
    let channels = channels.max(1);
    if let Some(seq) = flush.pending() {
        let all = consumer.slots();
        if let Ok(chunk) = consumer.read_chunk(all) {
            chunk.commit_all();
        }
        flush.complete(seq);
    }
    let wanted = out.len() - out.len() % channels;
    let available = consumer.slots().min(wanted);
    let count = available - available % channels;
    if count > 0 {
        if let Ok(chunk) = consumer.read_chunk(count) {
            let (first, second) = chunk.as_slices();
            for (slot, word) in out.iter_mut().zip(first.iter().chain(second)) {
                *slot = f32::from_bits(*word);
            }
            chunk.commit_all();
        }
    }
    out[count..].fill(0.0);
    starvation.observe(count / channels, wanted / channels);
    ramp.retarget(target_gain);
    ramp.apply_frames(&mut out[..count], channels, scale_f32);
    count / channels
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use rtrb::{Producer, RingBuffer};

    use super::{render, ring_samples, FlushRequest};
    use crate::decode::PcmBuffer;
    use crate::devices::{CapsSource, DeviceCaps, DeviceGroup, DeviceKind, Transport};
    use crate::error::{Error, Result};
    use crate::gain::GainRamp;
    use crate::output::{
        volume_gain, AudioOutput, AudioOutputSpec, OutputDevice, OutputMode, OutputStatus,
        VolumeControl,
    };
    use crate::ring::{self, push_samples, Starvation};

    const DISCONNECTED: &str = "output device disconnected";
    const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

    fn host() -> cpal::Host {
        cpal::default_host()
    }

    fn device_name(device: &cpal::Device) -> Option<String> {
        device.name().ok().filter(|name| !name.is_empty())
    }

    /// A guess at the device type from its Core Audio name; the transport
    /// type property is not exposed through `cpal`.
    fn kind_of(name: &str) -> (DeviceKind, Transport) {
        let lower = name.to_lowercase();
        if lower.contains("airpods") || lower.contains("bluetooth") || lower.contains("beats") {
            (DeviceKind::Bluetooth, Transport::Bluetooth)
        } else if lower.contains("headphone") {
            (DeviceKind::Headphones, Transport::Internal)
        } else if lower.contains("macbook") || lower.contains("built-in") || lower.contains("imac")
        {
            (DeviceKind::Speakers, Transport::Internal)
        } else if lower.contains("hdmi") || lower.contains("display") || lower.contains("lg ") {
            (DeviceKind::Hdmi, Transport::Hdmi)
        } else if lower.contains("blackhole")
            || lower.contains("loopback")
            || lower.contains("zoom")
        {
            (DeviceKind::Virtual, Transport::Virtual)
        } else if lower.contains("usb") || lower.contains("dac") {
            (DeviceKind::Usb, Transport::Usb)
        } else {
            (DeviceKind::Unknown, Transport::Unknown)
        }
    }

    /// Output devices, by Core Audio name (the id the settings save).
    pub fn enumerate() -> Result<Vec<OutputDevice>> {
        let host = host();
        let default = host.default_output_device().and_then(|d| device_name(&d));
        let devices = host
            .output_devices()
            .map_err(|e| Error::Output(format!("listing Core Audio devices: {e}")))?;
        let mut out = Vec::new();
        for device in devices {
            let Some(name) = device_name(&device) else {
                continue;
            };
            if out.iter().any(|d: &OutputDevice| d.id == name) {
                continue;
            }
            let (kind, transport) = kind_of(&name);
            let mut row = OutputDevice::new(name.clone(), name.clone(), DeviceGroup::CoreAudio);
            row.kind = kind;
            row.transport = transport;
            row.is_default = default.as_deref() == Some(name.as_str());
            out.push(row);
        }
        Ok(out)
    }

    fn find_device(id: Option<&str>) -> Option<cpal::Device> {
        let host = host();
        match id {
            None => host.default_output_device(),
            Some(id) => host
                .output_devices()
                .ok()?
                .find(|d| device_name(d).as_deref() == Some(id)),
        }
    }

    /// What the shared mixer runs at; exclusive is never offered on macOS.
    pub fn probe_caps(id: &str) -> Option<DeviceCaps> {
        let config = find_device(Some(id))?.default_output_config().ok()?;
        Some(DeviceCaps {
            exclusive: false,
            shared_rate: Some(config.sample_rate().0),
            shared_bits: Some((config.sample_format().sample_size() * 8) as u16),
            source: CapsSource::Probed,
            ..DeviceCaps::default()
        })
    }

    /// Output through Core Audio's HAL unit, on a thread that owns the stream.
    pub struct CoreAudioOutput {
        control: Arc<VolumeControl>,
        status: Arc<Mutex<OutputStatus>>,
        producer: Option<Producer<u32>>,
        worker: Option<JoinHandle<()>>,
        stop: Option<mpsc::Sender<()>>,
        flush: Arc<FlushRequest>,
        lost: Arc<AtomicBool>,
        starvation: Arc<Starvation>,
        samples_per_sec: usize,
        device_ms: u32,
    }

    impl Default for CoreAudioOutput {
        fn default() -> Self {
            Self::new()
        }
    }

    impl CoreAudioOutput {
        pub fn new() -> Self {
            Self {
                control: Arc::new(VolumeControl::default()),
                status: Arc::new(Mutex::new(OutputStatus::default())),
                producer: None,
                worker: None,
                stop: None,
                flush: Arc::new(FlushRequest::default()),
                lost: Arc::new(AtomicBool::new(false)),
                starvation: Arc::new(Starvation::default()),
                samples_per_sec: 0,
                device_ms: 0,
            }
        }

        fn shutdown_worker(&mut self) {
            // Dropping the sender wakes the stream thread, which drops the
            // stream (stopping the audio unit) and exits.
            self.stop.take();
            self.producer = None;
            if let Some(handle) = self.worker.take() {
                let _ = handle.join();
            }
        }
    }

    struct Ready {
        device_ms: u32,
    }

    #[allow(clippy::too_many_arguments)]
    fn run_stream(
        spec: AudioOutputSpec,
        mut consumer: rtrb::Consumer<u32>,
        control: Arc<VolumeControl>,
        flush: Arc<FlushRequest>,
        lost: Arc<AtomicBool>,
        starvation: Arc<Starvation>,
        ready: mpsc::Sender<std::result::Result<Ready, String>>,
        stop: mpsc::Receiver<()>,
    ) {
        let Some(device) = find_device(spec.device_id.as_deref()) else {
            let _ = ready.send(Err(match &spec.device_id {
                Some(id) => format!("output device not found: {id}"),
                None => "no default output device".into(),
            }));
            return;
        };
        let channels = spec.info.channels as usize;
        let rate = spec.info.sample_rate;
        let config = cpal::StreamConfig {
            channels: spec.info.channels,
            sample_rate: cpal::SampleRate(rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let mut ramp = GainRamp::new(volume_gain(control.position()), rate);
        let lost_cb = lost.clone();
        let stream = device.build_output_stream(
            &config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                render(
                    &mut consumer,
                    out,
                    channels,
                    &mut ramp,
                    volume_gain(control.position()),
                    &flush,
                    &starvation,
                );
            },
            move |err| {
                tracing::error!("core audio stream error: {err}");
                if matches!(err, cpal::StreamError::DeviceNotAvailable) {
                    lost_cb.store(true, Ordering::SeqCst);
                }
            },
            None,
        );
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                let _ = ready.send(Err(format!("opening Core Audio stream: {e}")));
                return;
            }
        };
        if let Err(e) = stream.play() {
            let _ = ready.send(Err(format!("starting Core Audio stream: {e}")));
            return;
        }
        // The HAL's default I/O buffer is 512 frames; count it plus the
        // safety offset as roughly one more period.
        let device_ms = (1024u64 * 1000 / rate.max(1) as u64) as u32;
        let _ = ready.send(Ok(Ready { device_ms }));
        // Park until the output closes (the sender is dropped).
        let _ = stop.recv();
        drop(stream);
    }

    impl AudioOutput for CoreAudioOutput {
        fn open(&mut self, spec: &AudioOutputSpec) -> Result<()> {
            self.shutdown_worker();
            self.lost = Arc::new(AtomicBool::new(false));
            self.flush = Arc::new(FlushRequest::default());
            self.starvation.disarm();
            let rate = spec.info.sample_rate;
            let channels = spec.info.channels;
            self.samples_per_sec = rate as usize * channels as usize;
            let (producer, consumer) = RingBuffer::<u32>::new(ring_samples(rate, channels));
            let (ready_tx, ready_rx) = mpsc::channel();
            let (stop_tx, stop_rx) = mpsc::channel();
            let spec_thread = spec.clone();
            let control = self.control.clone();
            let flush = self.flush.clone();
            let lost = self.lost.clone();
            let starvation = self.starvation.clone();
            let handle = std::thread::Builder::new()
                .name("oxitide-coreaudio".into())
                .spawn(move || {
                    run_stream(
                        spec_thread,
                        consumer,
                        control,
                        flush,
                        lost,
                        starvation,
                        ready_tx,
                        stop_rx,
                    )
                })
                .map_err(|e| Error::Output(e.to_string()))?;
            match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(ready)) => {
                    self.producer = Some(producer);
                    self.worker = Some(handle);
                    self.stop = Some(stop_tx);
                    self.device_ms = ready.device_ms;
                    *self.status.lock().unwrap() = OutputStatus {
                        mode: OutputMode::Shared,
                        bit_perfect: false,
                        notice: None,
                        volume_fixed: false,
                    };
                    Ok(())
                }
                Ok(Err(message)) => {
                    drop(stop_tx);
                    let _ = handle.join();
                    Err(Error::Output(message))
                }
                Err(_) => {
                    drop(stop_tx);
                    let _ = handle.join();
                    Err(Error::Output(
                        "audio device did not initialize in time".into(),
                    ))
                }
            }
        }

        fn start(&mut self) -> Result<()> {
            Ok(())
        }

        fn write(&mut self, buf: &PcmBuffer) -> Result<()> {
            if self.lost.load(Ordering::SeqCst) {
                return Err(Error::Output("output device unavailable".into()));
            }
            let rate = self.samples_per_sec;
            let producer = self
                .producer
                .as_mut()
                .ok_or_else(|| Error::Output("output not open".into()))?;
            let pushed = match buf {
                PcmBuffer::F32(v) => push_samples(producer, v, rate, |s| s.to_bits()),
                // Left-aligned i32 -> f32 is a pure divide (exact for 16/24 bit).
                PcmBuffer::I32(v) => push_samples(producer, v, rate, |s| {
                    (s as f32 / 2_147_483_648.0).to_bits()
                }),
            };
            self.starvation.arm();
            pushed
        }

        fn set_volume(&mut self, volume: f32) -> Result<()> {
            self.control.set(volume);
            Ok(())
        }

        fn attach_volume(&mut self, control: Arc<VolumeControl>) {
            self.control = control;
        }

        fn flush(&mut self) -> Result<()> {
            if self.producer.is_none() {
                return Ok(());
            }
            self.starvation.disarm();
            let seq = self.flush.request();
            let deadline = Instant::now() + FLUSH_TIMEOUT;
            while !self.flush.is_done(seq) {
                if Instant::now() >= deadline || self.lost.load(Ordering::SeqCst) {
                    return Err(Error::Output("audio device did not flush in time".into()));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            self.shutdown_worker();
            Ok(())
        }

        fn status(&self) -> OutputStatus {
            let mut status = self.status.lock().unwrap().clone();
            if self.lost.load(Ordering::SeqCst) {
                status.notice = Some(DISCONNECTED.into());
            }
            status
        }

        fn queued_ms(&self) -> Option<u32> {
            let producer = self.producer.as_ref()?;
            let queued = ring::queued_ms(
                producer.buffer().capacity(),
                producer.slots(),
                self.samples_per_sec,
            );
            Some(queued + self.device_ms)
        }

        fn underruns(&self) -> Option<u64> {
            Some(self.starvation.count())
        }

        fn drain(&mut self, cancel: &dyn Fn() -> bool) -> Result<()> {
            if let Some(producer) = self.producer.as_ref() {
                let lost = &self.lost;
                ring::wait_drained(producer, self.samples_per_sec, self.device_ms, &|| {
                    cancel() || lost.load(Ordering::SeqCst)
                });
            }
            Ok(())
        }
    }

    impl Drop for CoreAudioOutput {
        fn drop(&mut self) {
            self.shutdown_worker();
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::{enumerate, probe_caps, CoreAudioOutput};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::push_samples;
    use rtrb::RingBuffer;

    fn words(samples: &[f32]) -> Vec<u32> {
        samples.iter().map(|s| s.to_bits()).collect()
    }

    #[test]
    fn renders_whole_frames_and_pads_with_silence() {
        let (mut producer, mut consumer) = RingBuffer::<u32>::new(16);
        push_samples(&mut producer, &words(&[0.1, 0.2, 0.3]), 1000, |w| w).unwrap();
        let mut ramp = GainRamp::new(1.0, 48_000);
        let flush = FlushRequest::default();
        let starvation = Starvation::default();
        starvation.arm();
        let mut out = [9.0f32; 4];
        let frames = render(
            &mut consumer,
            &mut out,
            2,
            &mut ramp,
            1.0,
            &flush,
            &starvation,
        );
        assert_eq!(frames, 1, "the odd sample waits for its pair");
        assert_eq!(out, [0.1, 0.2, 0.0, 0.0]);
        assert_eq!(
            starvation.count(),
            1,
            "a short period after a write is a dry-out"
        );
    }

    #[test]
    fn unity_gain_passes_samples_through_untouched() {
        let (mut producer, mut consumer) = RingBuffer::<u32>::new(16);
        let input = [0.123_456_79f32, -0.987_654_3, 0.5, -0.5];
        push_samples(&mut producer, &words(&input), 1000, |w| w).unwrap();
        let mut ramp = GainRamp::new(1.0, 48_000);
        let mut out = [0.0f32; 4];
        render(
            &mut consumer,
            &mut out,
            2,
            &mut ramp,
            1.0,
            &FlushRequest::default(),
            &Starvation::default(),
        );
        assert_eq!(out, input);
    }

    #[test]
    fn a_flush_empties_the_ring_before_the_period() {
        let (mut producer, mut consumer) = RingBuffer::<u32>::new(16);
        push_samples(&mut producer, &words(&[0.5; 8]), 1000, |w| w).unwrap();
        let flush = FlushRequest::default();
        let seq = flush.request();
        assert!(!flush.is_done(seq));
        let mut out = [1.0f32; 4];
        let frames = render(
            &mut consumer,
            &mut out,
            2,
            &mut GainRamp::new(1.0, 48_000),
            1.0,
            &flush,
            &Starvation::default(),
        );
        assert_eq!(frames, 0);
        assert_eq!(out, [0.0; 4]);
        assert!(flush.is_done(seq));
        assert_eq!(consumer.slots(), 0);
    }

    #[test]
    fn volume_glides_instead_of_stepping() {
        // 2000 stereo frames: past the 1440-frame (30 ms at 48 kHz) ramp.
        let (mut producer, mut consumer) = RingBuffer::<u32>::new(4096);
        push_samples(&mut producer, &words(&[1.0; 4000]), 1000, |w| w).unwrap();
        let mut ramp = GainRamp::new(1.0, 48_000);
        let mut out = vec![0.0f32; 4000];
        render(
            &mut consumer,
            &mut out,
            2,
            &mut ramp,
            0.0,
            &FlushRequest::default(),
            &Starvation::default(),
        );
        assert!(out[0] > 0.9, "the ramp starts at the old gain");
        assert_eq!(out[0], out[1], "both channels of a frame share a gain");
        assert!(out[1440] > 0.0 && out[1440] < out[0], "mid-ramp is between");
        assert!(out[3999].abs() < 1e-6, "settled at the new gain after the ramp");
    }

    #[test]
    fn ring_is_about_200_ms() {
        assert_eq!(ring_samples(48_000, 2), 19_200);
        assert_eq!(ring_samples(8_000, 1), 4096);
    }
}
