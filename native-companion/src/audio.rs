//! WASAPI loopback capture of the default Windows RENDER (playback) endpoint.
//!
//! This module only ever asks Core Audio for `eRender` endpoints and opens them
//! with `AUDCLNT_STREAMFLAGS_LOOPBACK`, which yields whatever Windows is playing
//! to the speakers/headphones. It never enumerates or opens input endpoints,
//! so no microphone is ever touched.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::{log_error, log_info, log_warn};

/// WASAPI shared-mode buffer: 200 ms (in 100 ns units). Large enough to absorb
/// scheduling hiccups without overruns; fixed size so memory is bounded.
const BUFFER_DURATION_HNS: i64 = 2_000_000;
const WAIT_TIMEOUT_MS: u32 = 100;
const DEFAULT_DEVICE_CHECK: Duration = Duration::from_secs(2);
const REOPEN_RETRY_DELAY: Duration = Duration::from_millis(500);
const REOPEN_GIVE_UP: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    Float32,
    Int16,
    Int24,
    Int32,
}

impl fmt::Display for SampleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SampleKind::Float32 => "Float32",
            SampleKind::Int16 => "PCM16",
            SampleKind::Int24 => "PCM24",
            SampleKind::Int32 => "PCM32",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub kind: SampleKind,
    pub block_align: u16,
}

impl MixFormat {
    pub fn bytes_per_sample(&self) -> usize {
        match self.kind {
            SampleKind::Float32 | SampleKind::Int32 => 4,
            SampleKind::Int16 => 2,
            SampleKind::Int24 => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub enum AudioError {
    NoOutputDevice(String),
    InitFailed(String),
    DeviceLost(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NoOutputDevice(d) => write!(f, "no playback device: {d}"),
            AudioError::InitFailed(d) => write!(f, "WASAPI loopback init failed: {d}"),
            AudioError::DeviceLost(d) => write!(f, "playback device lost: {d}"),
        }
    }
}

impl From<windows::core::Error> for AudioError {
    fn from(e: windows::core::Error) -> Self {
        AudioError::InitFailed(format!("{} (0x{:08X})", e.message(), e.code().0))
    }
}

/// Receives captured audio on the capture thread. Implementations must be cheap
/// and non-blocking (copy/convert and hand off over a bounded channel).
pub trait CaptureSink: Send + 'static {
    /// Interleaved float samples in [-1, 1] with `channels` channels.
    fn on_audio(&mut self, interleaved: &[f32], channels: u16);
    /// The default playback device changed and capture switched to it.
    fn on_device_changed(&mut self, _device: &DeviceInfo, _format: &MixFormat) {}
    /// Capture ended because of an unrecoverable error.
    fn on_fatal(&mut self, _error: &AudioError) {}
}

/// A running loopback capture. Dropping it stops capture and joins the thread.
pub struct CaptureSession {
    pub device: DeviceInfo,
    pub format: MixFormat,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureSession {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Opens the default RENDER endpoint in loopback mode on a dedicated thread
/// and starts delivering audio to `sink`. Returns once capture is running.
pub fn start_loopback_capture<S: CaptureSink>(sink: S) -> Result<CaptureSession, AudioError> {
    let stop = Arc::new(AtomicBool::new(false));
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(DeviceInfo, MixFormat), AudioError>>(1);
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("wasapi-loopback".into())
        .spawn(move || capture_thread(sink, thread_stop, init_tx))
        .map_err(|e| AudioError::InitFailed(format!("spawn capture thread: {e}")))?;

    match init_rx.recv() {
        Ok(Ok((device, format))) => Ok(CaptureSession {
            device,
            format,
            stop,
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(AudioError::InitFailed("capture thread exited during init".into()))
        }
    }
}

/// Initializes COM (MTA) for this thread and uninitializes it on drop.
struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self, AudioError> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| AudioError::InitFailed(format!("CoInitializeEx: {}", e.message())))?;
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn capture_thread<S: CaptureSink>(
    mut sink: S,
    stop: Arc<AtomicBool>,
    init_tx: mpsc::SyncSender<Result<(DeviceInfo, MixFormat), AudioError>>,
) {
    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    // All COM objects live inside `run_capture` so they are released before CoUninitialize.
    run_capture(&mut sink, &stop, init_tx);
}

fn run_capture<S: CaptureSink>(
    sink: &mut S,
    stop: &AtomicBool,
    init_tx: mpsc::SyncSender<Result<(DeviceInfo, MixFormat), AudioError>>,
) {
    let enumerator: IMMDeviceEnumerator =
        match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
            Ok(e) => e,
            Err(e) => {
                let _ = init_tx.send(Err(e.into()));
                return;
            }
        };

    let mut current = match LoopbackStream::open_default(&enumerator, None) {
        Ok(s) => Some(s),
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    let Some(stream) = current.as_ref() else { return };
    // Rate the consumer (Deepgram) was configured for; kept stable across device switches.
    let locked_rate = stream.format.sample_rate;
    let _ = init_tx.send(Ok((stream.device.clone(), stream.format)));
    drop(init_tx);

    log_info!("Capture started");
    let mut scratch: Vec<f32> = Vec::new();
    let mut last_check = Instant::now();

    while !stop.load(Ordering::SeqCst) {
        let Some(stream) = current.as_ref() else { break };
        unsafe { WaitForSingleObject(stream.event, WAIT_TIMEOUT_MS) };
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let mut needs_reopen = false;
        if let Err(e) = stream.drain(sink, &mut scratch) {
            if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                log_warn!("Playback device invalidated; switching to new default");
            } else {
                log_warn!("Capture read error: {} (0x{:08X}); reopening", e.message(), e.code().0);
            }
            needs_reopen = true;
        }

        if !needs_reopen && last_check.elapsed() >= DEFAULT_DEVICE_CHECK {
            last_check = Instant::now();
            if let Ok(id) = default_render_id(&enumerator) {
                if id != stream.device.id {
                    log_info!("Default playback device changed");
                    needs_reopen = true;
                }
            }
        }

        if needs_reopen {
            current = None;
            match reopen_with_retry(&enumerator, locked_rate, stop) {
                Some(Ok(s)) => {
                    sink.on_device_changed(&s.device, &s.format);
                    current = Some(s);
                    last_check = Instant::now();
                }
                Some(Err(e)) => {
                    log_error!("{e}");
                    sink.on_fatal(&e);
                    return;
                }
                None => break,
            }
        }
    }
    drop(current);
    log_info!("Capture stopped");
}

/// Returns `None` if stop was requested while retrying.
fn reopen_with_retry(
    enumerator: &IMMDeviceEnumerator,
    locked_rate: u32,
    stop: &AtomicBool,
) -> Option<Result<LoopbackStream, AudioError>> {
    let started = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        match LoopbackStream::open_default(enumerator, Some(locked_rate)) {
            Ok(s) => return Some(Ok(s)),
            Err(e) if started.elapsed() >= REOPEN_GIVE_UP => {
                return Some(Err(match e {
                    AudioError::NoOutputDevice(d) => AudioError::NoOutputDevice(d),
                    other => AudioError::DeviceLost(other.to_string()),
                }))
            }
            Err(_) => std::thread::sleep(REOPEN_RETRY_DELAY),
        }
    }
}

struct LoopbackStream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    device: DeviceInfo,
    format: MixFormat,
}

impl LoopbackStream {
    /// Opens the default RENDER endpoint (eRender/eConsole) in shared loopback mode.
    /// When `force_rate` differs from the device mix rate, asks the audio engine to
    /// convert to that rate so downstream consumers keep a stable sample rate.
    fn open_default(
        enumerator: &IMMDeviceEnumerator,
        force_rate: Option<u32>,
    ) -> Result<Self, AudioError> {
        let device: IMMDevice = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|e| AudioError::NoOutputDevice(e.message().to_string()))?;
        let info = device_info(&device)?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;

        let mix_ptr = unsafe { client.GetMixFormat() }?;
        let mix = CoTaskMem(mix_ptr as *mut core::ffi::c_void);
        let mut format = parse_wave_format(mix_ptr)?;

        let mut flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let converted;
        let format_ptr: *const WAVEFORMATEX = match force_rate {
            Some(rate) if rate != format.sample_rate => {
                log_warn!(
                    "New device mix rate {} Hz differs from stream rate {} Hz; using engine conversion",
                    format.sample_rate,
                    rate
                );
                converted = float32_extensible(rate, format.channels);
                flags |= AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
                format = MixFormat {
                    sample_rate: rate,
                    channels: format.channels,
                    kind: SampleKind::Float32,
                    block_align: format.channels * 4,
                };
                &converted as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX
            }
            _ => mix_ptr,
        };

        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                BUFFER_DURATION_HNS,
                0,
                format_ptr,
                None,
            )
        }?;
        drop(mix);

        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }?;
        let stream = (|| -> Result<(IAudioCaptureClient,), AudioError> {
            unsafe { client.SetEventHandle(event) }?;
            let capture: IAudioCaptureClient = unsafe { client.GetService() }?;
            unsafe { client.Start() }?;
            Ok((capture,))
        })();
        let capture = match stream {
            Ok((c,)) => c,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(event);
                }
                return Err(e);
            }
        };

        log_info!("Default render device: {}", info.name);
        log_info!("WASAPI loopback initialized");
        log_info!(
            "Windows mix: {}Hz {}ch {}",
            format.sample_rate,
            format.channels,
            format.kind
        );

        Ok(LoopbackStream {
            client,
            capture,
            event,
            device: info,
            format,
        })
    }

    /// Reads every pending packet and forwards it to the sink as interleaved f32.
    fn drain<S: CaptureSink>(
        &self,
        sink: &mut S,
        scratch: &mut Vec<f32>,
    ) -> windows::core::Result<()> {
        let channels = self.format.channels as usize;
        loop {
            let pending = unsafe { self.capture.GetNextPacketSize() }?;
            if pending == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            }?;

            let samples = frames as usize * channels;
            scratch.clear();
            if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                scratch.resize(samples, 0.0);
            } else {
                let bytes = unsafe {
                    std::slice::from_raw_parts(data, frames as usize * self.format.block_align as usize)
                };
                decode_to_f32(bytes, &self.format, scratch);
            }
            unsafe { self.capture.ReleaseBuffer(frames) }?;

            if !scratch.is_empty() {
                sink.on_audio(scratch, self.format.channels);
            }
        }
    }
}

impl Drop for LoopbackStream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// Frees a CoTaskMemAlloc'd pointer on drop.
struct CoTaskMem(*mut core::ffi::c_void);

impl Drop for CoTaskMem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
        }
    }
}

fn pwstr_to_string_and_free(p: PWSTR) -> String {
    let _free = CoTaskMem(p.0 as *mut core::ffi::c_void);
    unsafe { p.to_string() }.unwrap_or_default()
}

fn device_info(device: &IMMDevice) -> Result<DeviceInfo, AudioError> {
    let id = pwstr_to_string_and_free(unsafe { device.GetId() }?);
    let name = unsafe { device.OpenPropertyStore(STGM_READ) }
        .and_then(|store| unsafe { store.GetValue(&PKEY_Device_FriendlyName) })
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "Default playback device".to_string());
    Ok(DeviceInfo { id, name })
}

fn default_render_id(enumerator: &IMMDeviceEnumerator) -> windows::core::Result<String> {
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?;
    Ok(pwstr_to_string_and_free(unsafe { device.GetId() }?))
}

/// Queries the current default playback device without starting capture.
pub fn query_default_output() -> Result<(DeviceInfo, MixFormat), AudioError> {
    let _com = ComGuard::new()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .map_err(|e| AudioError::NoOutputDevice(e.message().to_string()))?;
    let info = device_info(&device)?;
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;
    let mix_ptr = unsafe { client.GetMixFormat() }?;
    let _mix = CoTaskMem(mix_ptr as *mut core::ffi::c_void);
    let format = parse_wave_format(mix_ptr)?;
    Ok((info, format))
}

fn parse_wave_format(ptr: *const WAVEFORMATEX) -> Result<MixFormat, AudioError> {
    if ptr.is_null() {
        return Err(AudioError::InitFailed("null mix format".into()));
    }
    let wf = unsafe { std::ptr::read_unaligned(ptr) };
    let tag = wf.wFormatTag as u32;
    let bits = wf.wBitsPerSample;
    let cb_size = wf.cbSize;
    let (is_float, is_pcm) = if tag == WAVE_FORMAT_EXTENSIBLE && cb_size >= 22 {
        let ext = unsafe { std::ptr::read_unaligned(ptr as *const WAVEFORMATEXTENSIBLE) };
        let sub = ext.SubFormat;
        (sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, sub == KSDATAFORMAT_SUBTYPE_PCM)
    } else {
        (tag == WAVE_FORMAT_IEEE_FLOAT, tag == WAVE_FORMAT_PCM)
    };

    let kind = match (is_float, is_pcm, bits) {
        (true, _, 32) => SampleKind::Float32,
        (_, true, 16) => SampleKind::Int16,
        (_, true, 24) => SampleKind::Int24,
        (_, true, 32) => SampleKind::Int32,
        _ => {
            return Err(AudioError::InitFailed(format!(
                "unsupported mix format (tag 0x{tag:04X}, {bits} bits)"
            )))
        }
    };
    let channels = wf.nChannels;
    let block_align = wf.nBlockAlign;
    if channels == 0 || block_align as usize != channels as usize * (bits as usize / 8) {
        return Err(AudioError::InitFailed(format!(
            "inconsistent mix format ({channels} ch, block align {block_align}, {bits} bits)"
        )));
    }
    Ok(MixFormat {
        sample_rate: wf.nSamplesPerSec,
        channels,
        kind,
        block_align,
    })
}

fn float32_extensible(rate: u32, channels: u16) -> WAVEFORMATEXTENSIBLE {
    let block_align = channels * 4;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: 0,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// Decodes raw WASAPI bytes of any supported sample kind into interleaved f32 in [-1, 1].
pub fn decode_to_f32(bytes: &[u8], format: &MixFormat, out: &mut Vec<f32>) {
    let bps = format.bytes_per_sample();
    out.reserve(bytes.len() / bps);
    match format.kind {
        SampleKind::Float32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        ),
        SampleKind::Int16 => out.extend(
            bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0),
        ),
        SampleKind::Int24 => out.extend(bytes.chunks_exact(3).map(|b| {
            let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
            v as f32 / 8_388_608.0
        })),
        SampleKind::Int32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(kind: SampleKind, channels: u16) -> MixFormat {
        let bps = match kind {
            SampleKind::Float32 | SampleKind::Int32 => 4,
            SampleKind::Int16 => 2,
            SampleKind::Int24 => 3,
        };
        MixFormat {
            sample_rate: 48_000,
            channels,
            kind,
            block_align: channels * bps,
        }
    }

    #[test]
    fn decodes_float32() {
        let mut bytes = Vec::new();
        for v in [0.5f32, -0.25] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = Vec::new();
        decode_to_f32(&bytes, &fmt(SampleKind::Float32, 2), &mut out);
        assert_eq!(out, vec![0.5, -0.25]);
    }

    #[test]
    fn decodes_pcm24_sign_extension() {
        // +max, -max (0x800000), -1 (0xFFFFFF)
        let bytes = [0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF];
        let mut out = Vec::new();
        decode_to_f32(&bytes, &fmt(SampleKind::Int24, 1), &mut out);
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert_eq!(out[1], -1.0);
        assert!(out[2] < 0.0 && out[2] > -1e-6);
    }

    #[test]
    fn decodes_pcm16() {
        let bytes = [0x00, 0x80, 0x00, 0x40];
        let mut out = Vec::new();
        decode_to_f32(&bytes, &fmt(SampleKind::Int16, 2), &mut out);
        assert_eq!(out, vec![-1.0, 0.5]);
    }
}
