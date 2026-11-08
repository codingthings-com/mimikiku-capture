use base64::Engine;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use tokio::sync::mpsc;

/// State shared between the realtime audio callback, the network task and the UI.
#[derive(Default)]
pub struct Shared {
    pub connected: AtomicBool,
    pub paused: AtomicBool,
    /// Last callback RMS, as f32 bits.
    pub rms: AtomicU32,
    /// Peak |sample| since the UI last took it, as f32 bits (non-negative floats order like u32).
    pub peak: AtomicU32,
    pub sent: AtomicU64,
    pub dropped: AtomicU64,
}

impl Shared {
    pub fn rms(&self) -> f32 {
        f32::from_bits(self.rms.load(Ordering::Relaxed))
    }

    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }
}

pub fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".into())
}

pub fn list_devices() -> Result<Vec<String>, Box<dyn Error>> {
    Ok(cpal::default_host().input_devices()?.map(|d| device_name(&d)).collect())
}

pub fn pick_device(wanted: Option<&str>) -> Result<cpal::Device, Box<dyn Error>> {
    let host = cpal::default_host();
    let wanted = wanted.map(str::to_lowercase);
    let matches = |d: &cpal::Device| {
        let name = device_name(d).to_lowercase();
        match &wanted {
            Some(w) => name.contains(w.as_str()),
            None => name.contains("monitor") || name.contains("pipewire"),
        }
    };
    if let Some(d) = host.input_devices()?.find(|d| matches(d)) {
        return Ok(d);
    }
    if let Some(w) = &wanted {
        return Err(format!("no input device matching {w:?} (try --list-devices)").into());
    }
    host.default_input_device().ok_or_else(|| "no input device found".into())
}

/// Little-endian f32 bytes, base64-encoded (the wire format the server expects).
fn encode_chunk(data: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for s in data {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Start capturing. Mono-downmixed, base64 chunks go to `tx` while connected and not paused.
/// Errors from the stream callback are reported through `on_error`.
pub struct Capture {
    pub channels: u16,
    pub rate: u32,
    pub chunk: u32,
    pub threshold: f32,
}

pub fn start(
    device: &cpal::Device,
    cap: Capture,
    shared: Arc<Shared>,
    tx: mpsc::Sender<String>,
    on_error: impl Fn(String) + Send + 'static,
) -> Result<cpal::Stream, Box<dyn Error>> {
    let Capture { channels, rate, chunk, threshold } = cap;
    let config = cpal::StreamConfig {
        channels,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Fixed(chunk),
    };
    let ch = channels.max(1) as usize;
    let mut mono: Vec<f32> = Vec::new();

    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _| {
            let samples: &[f32] = if ch == 1 {
                data
            } else {
                mono.clear();
                mono.extend(data.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
                &mono
            };
            if samples.is_empty() {
                return;
            }
            let mean_sq = samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32;
            let rms = mean_sq.sqrt();
            let peak = samples.iter().fold(0f32, |m, x| m.max(x.abs()));
            shared.rms.store(rms.to_bits(), Ordering::Relaxed);
            shared.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);

            if rms <= threshold
                || shared.paused.load(Ordering::Relaxed)
                || !shared.connected.load(Ordering::Relaxed)
            {
                return;
            }
            // Never block the realtime callback: drop the chunk if the socket lags.
            match tx.try_send(encode_chunk(samples)) {
                Ok(()) => shared.sent.fetch_add(1, Ordering::Relaxed),
                Err(_) => shared.dropped.fetch_add(1, Ordering::Relaxed),
            };
        },
        move |err| on_error(format!("audio stream: {err}")),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}
