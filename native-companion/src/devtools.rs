//! Developer verification modes (run from a terminal, never by Chrome):
//! `--meter`, `--pcm-test`, `--transcribe-stderr`. All output goes to stderr.

use std::io::{IsTerminal, Write};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::{self, AudioError, CaptureSink, DeviceInfo, MixFormat};
use crate::convert;
use crate::deepgram;
use crate::logging;
use crate::{log_error, log_info};

const METER_WIDTH: usize = 10;
const METER_FLOOR_DB: f32 = -60.0;

pub fn print_banner(device: &DeviceInfo, format: &MixFormat) {
    eprintln!(
        "System Audio Companion\n\n\
         Output device:\n{}\n\n\
         Mode:\nWASAPI loopback\n\n\
         Windows mix format:\n\
         Sample rate: {} Hz\n\
         Channels: {}\n\
         Format: {}\n\n\
         Microphone capture:\nDISABLED\n",
        device.name, format.sample_rate, format.channels, format.kind
    );
}

#[derive(Default)]
struct LevelAccumulator {
    sum_squares: f64,
    samples: u64,
}

struct MeterSink {
    level: Arc<Mutex<LevelAccumulator>>,
}

impl CaptureSink for MeterSink {
    fn on_audio(&mut self, interleaved: &[f32], _channels: u16) {
        let sum: f64 = interleaved.iter().map(|&s| (s as f64) * (s as f64)).sum();
        if let Ok(mut acc) = self.level.lock() {
            acc.sum_squares += sum;
            acc.samples += interleaved.len() as u64;
        }
    }

    fn on_device_changed(&mut self, device: &DeviceInfo, format: &MixFormat) {
        eprintln!(
            "\nOutput device changed: {} ({} Hz, {} ch, {})",
            device.name, format.sample_rate, format.channels, format.kind
        );
    }

    fn on_fatal(&mut self, error: &AudioError) {
        eprintln!("\nCapture stopped: {error}");
    }
}

fn rms_to_db(rms: f64) -> f32 {
    if rms <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * (rms as f32).log10()
    }
}

fn meter_bar(db: f32) -> String {
    let filled = if db.is_finite() {
        (((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0) * METER_WIDTH as f32).round()
            as usize
    } else {
        0
    };
    format!("{}{}", "█".repeat(filled), "░".repeat(METER_WIDTH - filled))
}

fn format_db(db: f32) -> String {
    if db.is_finite() {
        format!("{db:6.1} dBFS")
    } else {
        "  -inf dBFS".to_string()
    }
}

fn deadline_reached(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

pub fn run_meter(seconds: Option<u64>) -> i32 {
    logging::init(false);
    log_info!("Companion started (meter mode)");

    let level = Arc::new(Mutex::new(LevelAccumulator::default()));
    let session = match audio::start_loopback_capture(MeterSink {
        level: level.clone(),
    }) {
        Ok(s) => s,
        Err(e) => {
            log_error!("{e}");
            eprintln!("Could not start system audio capture: {e}");
            return 1;
        }
    };
    print_banner(&session.device, &session.format);

    let interactive = std::io::stderr().is_terminal();
    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let (mut ticks, mut active_ticks) = (0u64, 0u64);
    let mut peak_db = f32::NEG_INFINITY;
    let mut stderr = std::io::stderr();

    while !deadline_reached(deadline) {
        std::thread::sleep(Duration::from_millis(100));
        let (sum, n) = {
            let mut acc = level.lock().unwrap();
            let snapshot = (acc.sum_squares, acc.samples);
            *acc = LevelAccumulator::default();
            snapshot
        };
        let db = if n == 0 {
            f32::NEG_INFINITY
        } else {
            rms_to_db((sum / n as f64).sqrt())
        };
        ticks += 1;
        if db > METER_FLOOR_DB {
            active_ticks += 1;
        }
        peak_db = peak_db.max(db);

        let line = format!("Audio: {}  {}", meter_bar(db), format_db(db));
        let _ = if interactive {
            write!(stderr, "\r{line}   ")
        } else {
            writeln!(stderr, "{line}")
        };
        let _ = stderr.flush();
    }

    session.stop();
    eprintln!(
        "\nMeter summary: {active_ticks}/{ticks} intervals with audio above {METER_FLOOR_DB} dBFS, peak {}",
        format_db(peak_db).trim()
    );
    0
}

pub fn run_pcm_test(seconds: Option<u64>) -> i32 {
    logging::init(false);
    log_info!("Companion started (pcm-test mode)");

    let (info, format) = match audio::query_default_output() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Could not query playback device: {e}");
            return 1;
        }
    };
    let (audio_tx, audio_rx) = convert::audio_channel();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let session = match audio::start_loopback_capture(convert::PcmChunkSink::new(
        format.sample_rate,
        audio_tx,
        events_tx,
    )) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Could not start system audio capture: {e}");
            return 1;
        }
    };
    print_banner(&info, &session.format);
    eprintln!(
        "Output stream: {} Hz, 1 channel, PCM16 little-endian, {} ms chunks ({} bytes)\n",
        session.format.sample_rate,
        convert::CHUNK_MS,
        session.format.sample_rate as u64 * convert::CHUNK_MS as u64 / 1000 * 2
    );

    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let mut next_report = Instant::now() + Duration::from_secs(1);
    let (mut chunks, mut bytes, mut peak, mut sum_sq) = (0u64, 0u64, 0i32, 0f64);
    let mut total_samples = 0u64;
    loop {
        match audio_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                chunks += 1;
                bytes += chunk.len() as u64;
                for s in chunk
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
                {
                    peak = peak.max(s.abs());
                    sum_sq += (s as f64) * (s as f64);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(ev) = events_rx.try_recv() {
            eprintln!("Capture event: {ev:?}");
        }
        if Instant::now() >= next_report {
            next_report += Duration::from_secs(1);
            let samples = bytes / 2;
            total_samples += samples;
            let rms = if samples > 0 {
                (sum_sq / samples as f64).sqrt()
            } else {
                0.0
            };
            eprintln!(
                "PCM16 mono: {chunks:2} chunks/s  {bytes:6} bytes/s  {samples:6} samples/s  peak {peak:5}  rms {rms:7.1}"
            );
            (chunks, bytes, peak, sum_sq) = (0, 0, 0, 0.0);
            if deadline_reached(deadline) {
                break;
            }
        }
    }
    session.stop();
    eprintln!("\nPCM summary: {total_samples} mono samples converted");
    0
}

pub fn run_transcribe_stderr(seconds: Option<u64>) -> i32 {
    logging::init(true);
    log_info!("Companion started (transcribe-stderr mode)");

    let Some(api_key) = deepgram::load_api_key() else {
        eprintln!("{} is not configured.", deepgram::API_KEY_ENV);
        return 1;
    };
    let config = deepgram::DeepgramConfig::default();

    let (info, format) = match audio::query_default_output() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("No Windows playback device is available: {e}");
            return 1;
        }
    };
    let (audio_tx, audio_rx) = convert::audio_channel();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let session = match audio::start_loopback_capture(convert::PcmChunkSink::new(
        format.sample_rate,
        audio_tx,
        events_tx,
    )) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Could not start system audio capture: {e}");
            return 1;
        }
    };
    print_banner(&info, &session.format);

    let socket = match deepgram::connect(&config, &api_key, session.format.sample_rate) {
        Ok(s) => s,
        Err(e) => {
            log_error!("{e}");
            session.stop();
            return 1;
        }
    };
    drop(api_key);

    let dg = std::thread::spawn(move || {
        deepgram::run_session(socket, audio_rx, |event| match event {
            deepgram::TranscriptEvent::Partial(t) => eprintln!("[partial] {t}"),
            deepgram::TranscriptEvent::Final(t) => eprintln!("[FINAL]   {t}"),
        })
    });

    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    while !deadline_reached(deadline) && !dg.is_finished() {
        std::thread::sleep(Duration::from_millis(100));
        while let Ok(ev) = events_rx.try_recv() {
            log_info!("Capture event: {ev:?}");
        }
    }
    session.stop();
    match dg.join() {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            log_error!("{e}");
            1
        }
        Err(_) => {
            log_error!("Deepgram thread panicked");
            1
        }
    }
}
