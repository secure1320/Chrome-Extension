//! In-memory PCM conversion: interleaved f32 -> mono -> PCM16 little-endian chunks.
//! No resampling: chunks keep the Windows mix sample rate, which is passed to Deepgram.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};

use tokio::sync::mpsc::UnboundedSender;

use crate::audio::{AudioError, CaptureSink, DeviceInfo, MixFormat};
use crate::log_warn;

/// Duration of each binary frame sent downstream.
pub const CHUNK_MS: u32 = 100;
/// Capacity of the bounded audio channel (~5 s of audio at `CHUNK_MS`).
pub const AUDIO_CHANNEL_CAPACITY: usize = 50;

/// Bounded channel carrying PCM16 mono chunks from the capture thread to Deepgram.
pub fn audio_channel() -> (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) {
    sync_channel(AUDIO_CHANNEL_CAPACITY)
}

/// Averages a stereo interleaved buffer into mono.
pub fn downmix_stereo_to_mono(interleaved: &[f32], out: &mut Vec<f32>) {
    out.extend(interleaved.chunks_exact(2).map(|lr| (lr[0] + lr[1]) * 0.5));
}

/// Averages any channel count into mono (fast path for mono/stereo).
pub fn downmix_to_mono(interleaved: &[f32], channels: u16, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend_from_slice(interleaved),
        2 => downmix_stereo_to_mono(interleaved, out),
        n => {
            let n = n as usize;
            let scale = 1.0 / n as f32;
            out.extend(
                interleaved
                    .chunks_exact(n)
                    .map(|frame| frame.iter().sum::<f32>() * scale),
            );
        }
    }
}

/// Converts f32 samples in [-1, 1] to signed 16-bit, clamping out-of-range input.
pub fn float32_to_pcm16(samples: &[f32], out: &mut Vec<i16>) {
    out.extend(samples.iter().map(|&s| {
        let s = if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) };
        (s * 32767.0).round() as i16
    }));
}

/// Accumulates converted audio and emits fixed-size PCM16 mono LE chunks.
/// All internal buffers are reused and bounded by one chunk plus one packet.
pub struct Pcm16Encoder {
    chunk_bytes: usize,
    mono: Vec<f32>,
    pcm: Vec<i16>,
    pending: Vec<u8>,
}

impl Pcm16Encoder {
    pub fn new(sample_rate: u32) -> Self {
        let chunk_samples = (sample_rate as usize * CHUNK_MS as usize / 1000).max(1);
        let chunk_bytes = chunk_samples * 2;
        Self {
            chunk_bytes,
            mono: Vec::new(),
            pcm: Vec::new(),
            pending: Vec::with_capacity(chunk_bytes * 2),
        }
    }

    pub fn push(&mut self, interleaved: &[f32], channels: u16, mut emit: impl FnMut(Vec<u8>)) {
        self.mono.clear();
        downmix_to_mono(interleaved, channels, &mut self.mono);
        self.pcm.clear();
        float32_to_pcm16(&self.mono, &mut self.pcm);
        for s in &self.pcm {
            self.pending.extend_from_slice(&s.to_le_bytes());
        }
        while self.pending.len() >= self.chunk_bytes {
            let rest = self.pending.split_off(self.chunk_bytes);
            let chunk = std::mem::replace(&mut self.pending, rest);
            emit(chunk);
        }
    }
}

/// Non-audio events from the capture thread.
#[derive(Debug)]
pub enum CaptureEvent {
    DeviceChanged(DeviceInfo),
    Fatal(AudioError),
}

/// Capture sink that converts to PCM16 mono and hands chunks to a bounded channel.
/// If the consumer falls behind, chunks are dropped rather than buffered unboundedly.
pub struct PcmChunkSink {
    encoder: Pcm16Encoder,
    audio_tx: SyncSender<Vec<u8>>,
    events_tx: UnboundedSender<CaptureEvent>,
    dropped: u64,
}

impl PcmChunkSink {
    pub fn new(
        sample_rate: u32,
        audio_tx: SyncSender<Vec<u8>>,
        events_tx: UnboundedSender<CaptureEvent>,
    ) -> Self {
        Self {
            encoder: Pcm16Encoder::new(sample_rate),
            audio_tx,
            events_tx,
            dropped: 0,
        }
    }
}

impl CaptureSink for PcmChunkSink {
    fn on_audio(&mut self, interleaved: &[f32], channels: u16) {
        let tx = &self.audio_tx;
        let dropped = &mut self.dropped;
        self.encoder.push(interleaved, channels, |chunk| {
            if let Err(TrySendError::Full(_)) = tx.try_send(chunk) {
                *dropped += 1;
                if *dropped % 50 == 1 {
                    log_warn!("Audio consumer is behind; dropped {} chunk(s) so far", dropped);
                }
            }
        });
    }

    fn on_device_changed(&mut self, device: &DeviceInfo, _format: &MixFormat) {
        let _ = self.events_tx.send(CaptureEvent::DeviceChanged(device.clone()));
    }

    fn on_fatal(&mut self, error: &AudioError) {
        let _ = self.events_tx.send(CaptureEvent::Fatal(error.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_downmix_averages_pairs() {
        let mut out = Vec::new();
        downmix_stereo_to_mono(&[1.0, 0.0, -0.5, -0.5, 0.25, 0.75], &mut out);
        assert_eq!(out, vec![0.5, -0.5, 0.5]);
    }

    #[test]
    fn multichannel_downmix() {
        let mut out = Vec::new();
        downmix_to_mono(&[0.6, 0.0, 0.0, 0.6, 0.0, 0.0], 3, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.2).abs() < 1e-6);
        assert!((out[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn mono_passthrough() {
        let mut out = Vec::new();
        downmix_to_mono(&[0.1, 0.2], 1, &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
    }

    #[test]
    fn pcm16_clamps_and_scales() {
        let mut out = Vec::new();
        float32_to_pcm16(&[0.0, 1.0, -1.0, 2.0, -3.0, 0.5, f32::NAN], &mut out);
        assert_eq!(out, vec![0, 32767, -32767, 32767, -32767, 16384, 0]);
    }

    #[test]
    fn encoder_emits_fixed_100ms_chunks() {
        let mut enc = Pcm16Encoder::new(48_000);
        let mut chunks = Vec::new();
        // 250 ms of stereo audio in uneven packets.
        let packet = vec![0.25f32; 480 * 2 * 5]; // 50 ms stereo
        for _ in 0..5 {
            enc.push(&packet, 2, |c| chunks.push(c));
        }
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.len() == 4800 * 2));
        let first = i16::from_le_bytes([chunks[0][0], chunks[0][1]]);
        assert_eq!(first, (0.25f32 * 32767.0).round() as i16);
        assert_eq!(enc.pending.len(), 2400 * 2);
    }

    #[test]
    fn sink_drops_when_channel_full() {
        let (tx, rx) = sync_channel(1);
        let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = PcmChunkSink::new(48_000, tx, etx);
        let packet = vec![0.0f32; 4800 * 2];
        for _ in 0..3 {
            sink.on_audio(&packet, 2);
        }
        assert_eq!(sink.dropped, 2);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}
