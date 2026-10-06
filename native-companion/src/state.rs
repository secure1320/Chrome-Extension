//! Listening state machine and controller.
//!
//! The controller runs on a single tokio task and never blocks: starting
//! (WASAPI init + Deepgram connect) and stopping (capture stop + Deepgram
//! CloseStream) run on short-lived threads and report back via oneshots.

use std::thread::JoinHandle;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::audio::{self, AudioError, CaptureSession};
use crate::convert::{self, CaptureEvent, PcmChunkSink};
use crate::deepgram::{self, DeepgramConfig, DeepgramError, TranscriptEvent};
use crate::messaging::Inbound;
use crate::protocol::{Command, ErrorCode, OutMessage};
use crate::screen;
use crate::{log_error, log_info, log_warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenState {
    Stopped,
    Starting,
    Listening,
    Stopping,
    Error,
}

impl ListenState {
    pub fn as_str(self) -> &'static str {
        match self {
            ListenState::Stopped => "stopped",
            ListenState::Starting => "starting",
            ListenState::Listening => "listening",
            ListenState::Stopping => "stopping",
            ListenState::Error => "error",
        }
    }
}

/// Everything owned by one listening session.
struct Session {
    capture: CaptureSession,
    deepgram_thread: JoinHandle<()>,
    deepgram_done: oneshot::Receiver<Result<(), DeepgramError>>,
    capture_events: UnboundedReceiver<CaptureEvent>,
    device: String,
    sample_rate: u32,
}

fn audio_error_code(e: &AudioError) -> ErrorCode {
    match e {
        AudioError::NoOutputDevice(_) => ErrorCode::NoOutputDevice,
        AudioError::InitFailed(_) => ErrorCode::AudioInitFailed,
        AudioError::DeviceLost(_) => ErrorCode::AudioDeviceLost,
    }
}

fn deepgram_error_code(e: &DeepgramError) -> ErrorCode {
    match e {
        DeepgramError::Auth(_) => ErrorCode::DeepgramAuthFailed,
        DeepgramError::Connect(_) => ErrorCode::DeepgramConnectionFailed,
        DeepgramError::Disconnected(_) => ErrorCode::DeepgramDisconnected,
    }
}

/// Blocking: verify key -> open WASAPI loopback -> connect Deepgram -> start streaming.
fn start_session(out: UnboundedSender<OutMessage>) -> Result<Session, ErrorCode> {
    let Some(api_key) = deepgram::load_api_key() else {
        log_warn!("{} is not configured", deepgram::API_KEY_ENV);
        return Err(ErrorCode::NoApiKey);
    };
    let config = DeepgramConfig::default();

    let (_, format) = audio::query_default_output().map_err(|e| {
        log_error!("{e}");
        audio_error_code(&e)
    })?;
    let (audio_tx, audio_rx) = convert::audio_channel();
    let (events_tx, capture_events) = mpsc::unbounded_channel();
    let capture = audio::start_loopback_capture(PcmChunkSink::new(
        format.sample_rate,
        audio_tx,
        events_tx,
    ))
    .map_err(|e| {
        log_error!("{e}");
        audio_error_code(&e)
    })?;

    let sample_rate = capture.format.sample_rate;
    let socket = match deepgram::connect(&config, &api_key, sample_rate) {
        Ok(s) => s,
        Err(e) => {
            log_error!("{e}");
            capture.stop();
            return Err(deepgram_error_code(&e));
        }
    };
    drop(api_key);

    let (done_tx, deepgram_done) = oneshot::channel();
    let deepgram_thread = std::thread::Builder::new()
        .name("deepgram".into())
        .spawn(move || {
            let result = deepgram::run_session(socket, audio_rx, |event| {
                let msg = match event {
                    TranscriptEvent::Partial(text) => OutMessage::TranscriptPartial { text },
                    TranscriptEvent::Final(text) => OutMessage::TranscriptFinal { text },
                };
                let _ = out.send(msg);
            });
            let _ = done_tx.send(result);
        })
        .map_err(|e| {
            log_error!("Could not spawn Deepgram thread: {e}");
            ErrorCode::DeepgramConnectionFailed
        })?;

    Ok(Session {
        device: capture.device.name.clone(),
        sample_rate,
        capture,
        deepgram_thread,
        deepgram_done,
        capture_events,
    })
}

/// Blocking: stop capture immediately (closes the audio channel, which makes the
/// Deepgram thread send CloseStream), then wait for Deepgram to finish.
fn stop_session(session: Session) {
    let Session {
        capture,
        deepgram_thread,
        ..
    } = session;
    capture.stop();
    if deepgram_thread.join().is_err() {
        log_error!("Deepgram thread panicked");
    }
}

pub struct Controller {
    state: ListenState,
    out: UnboundedSender<OutMessage>,
    session: Option<SessionInfo>,
    capture_events: Option<UnboundedReceiver<CaptureEvent>>,
    deepgram_done: Option<oneshot::Receiver<Result<(), DeepgramError>>>,
    pending_start: Option<oneshot::Receiver<Result<Session, ErrorCode>>>,
    pending_stop: Option<oneshot::Receiver<()>>,
    stop_after_start: bool,
    error_after_stop: Option<ErrorCode>,
}

/// The parts of a running session the controller keeps (handles move to stop thread).
struct SessionInfo {
    capture: CaptureSession,
    deepgram_thread: JoinHandle<()>,
    device: String,
    deepgram_connected: bool,
}

async fn recv_opt<T>(rx: &mut Option<oneshot::Receiver<T>>) -> Option<T> {
    match rx {
        Some(r) => {
            let value = r.await.ok();
            *rx = None;
            value
        }
        None => std::future::pending().await,
    }
}

async fn recv_events(rx: &mut Option<UnboundedReceiver<CaptureEvent>>) -> Option<CaptureEvent> {
    match rx {
        Some(r) => {
            let ev = r.recv().await;
            if ev.is_none() {
                *rx = None;
            }
            ev
        }
        None => std::future::pending().await,
    }
}

impl Controller {
    pub fn new(out: UnboundedSender<OutMessage>) -> Self {
        Self {
            state: ListenState::Stopped,
            out,
            session: None,
            capture_events: None,
            deepgram_done: None,
            pending_start: None,
            pending_stop: None,
            stop_after_start: false,
            error_after_stop: None,
        }
    }

    pub async fn run(mut self, mut inbound: UnboundedReceiver<Inbound>) {
        loop {
            tokio::select! {
                msg = inbound.recv() => match msg {
                    Some(Inbound::Command(cmd)) => self.handle_command(cmd),
                    Some(Inbound::Invalid) => self.send(OutMessage::error(ErrorCode::InvalidCommand)),
                    Some(Inbound::Closed) | None => break,
                },
                result = recv_opt(&mut self.pending_start) => self.on_start_finished(result),
                done = recv_opt(&mut self.pending_stop) => {
                    let _ = done;
                    self.on_stop_finished();
                }
                Some(event) = recv_events(&mut self.capture_events) => self.on_capture_event(event),
                result = recv_opt(&mut self.deepgram_done) => self.on_deepgram_ended(result),
            }
        }
        log_info!("Chrome disconnected; shutting down");
        self.shutdown().await;
    }

    fn send(&self, msg: OutMessage) {
        let _ = self.out.send(msg);
    }

    fn set_state(&mut self, state: ListenState) {
        if self.state != state {
            log_info!("State: {} -> {}", self.state.as_str(), state.as_str());
            self.state = state;
        }
    }

    fn status_message(&self) -> OutMessage {
        let device = match &self.session {
            Some(s) => Some(s.device.clone()),
            None => audio::query_default_output().ok().map(|(d, _)| d.name),
        };
        OutMessage::Status {
            running: self.state == ListenState::Listening,
            state: self.state.as_str(),
            deepgram_connected: self.session.as_ref().is_some_and(|s| s.deepgram_connected),
            device,
        }
    }

    fn handle_command(&mut self, cmd: Command) {
        log_info!("Command: {cmd:?}");
        match cmd {
            Command::Status => self.send(self.status_message()),
            Command::Start => match self.state {
                ListenState::Stopped | ListenState::Error => self.begin_start(),
                ListenState::Starting | ListenState::Listening | ListenState::Stopping => {
                    self.send(self.status_message())
                }
            },
            Command::Stop => match self.state {
                ListenState::Stopped | ListenState::Error => {
                    self.set_state(ListenState::Stopped);
                    self.send(OutMessage::Stopped);
                }
                ListenState::Starting => {
                    self.stop_after_start = true;
                    self.set_state(ListenState::Stopping);
                }
                ListenState::Listening => self.begin_stop(None),
                ListenState::Stopping => {}
            },
            Command::CaptureScreen => self.begin_capture(),
        }
    }

    /// Independent of listening; runs off-thread and reports back on its own.
    fn begin_capture(&self) {
        let out = self.out.clone();
        let spawned = std::thread::Builder::new()
            .name("screen-capture".into())
            .spawn(move || {
                let msg = match screen::capture_and_save() {
                    Ok(outcome) => {
                        log_info!(
                            "Screen captured: {}x{}, saved={}, copied={}",
                            outcome.width,
                            outcome.height,
                            outcome.path.is_some(),
                            outcome.copied
                        );
                        OutMessage::ScreenCaptured {
                            path: outcome.path.map(|p| p.display().to_string()),
                            copied: outcome.copied,
                            width: outcome.width,
                            height: outcome.height,
                        }
                    }
                    Err(e) => {
                        log_error!("Screen capture failed: {e}");
                        OutMessage::error(ErrorCode::ScreenCaptureFailed)
                    }
                };
                let _ = out.send(msg);
            });
        if let Err(e) = spawned {
            log_error!("Could not spawn capture thread: {e}");
            self.send(OutMessage::error(ErrorCode::ScreenCaptureFailed));
        }
    }

    fn begin_start(&mut self) {
        self.set_state(ListenState::Starting);
        self.stop_after_start = false;
        let (tx, rx) = oneshot::channel();
        let out = self.out.clone();
        let spawned = std::thread::Builder::new()
            .name("session-start".into())
            .spawn(move || {
                let result = start_session(out);
                if let Err(Ok(session)) = tx.send(result) {
                    // Controller went away while starting; release everything.
                    stop_session(session);
                }
            });
        match spawned {
            Ok(_) => self.pending_start = Some(rx),
            Err(e) => {
                log_error!("Could not spawn start thread: {e}");
                self.set_state(ListenState::Error);
                self.send(OutMessage::error(ErrorCode::AudioInitFailed));
            }
        }
    }

    fn on_start_finished(&mut self, result: Option<Result<Session, ErrorCode>>) {
        let result = result.unwrap_or(Err(ErrorCode::AudioInitFailed));
        match result {
            Ok(session) => {
                let Session {
                    capture,
                    deepgram_thread,
                    deepgram_done,
                    capture_events,
                    device,
                    sample_rate,
                } = session;
                self.capture_events = Some(capture_events);
                self.deepgram_done = Some(deepgram_done);
                self.session = Some(SessionInfo {
                    capture,
                    deepgram_thread,
                    device: device.clone(),
                    deepgram_connected: true,
                });
                if self.stop_after_start {
                    self.begin_stop(None);
                    return;
                }
                self.set_state(ListenState::Listening);
                log_info!("Listening: {device} ({sample_rate} Hz mono PCM16 to Deepgram)");
                self.send(OutMessage::Started {
                    device,
                    sample_rate,
                    channels: 1,
                });
            }
            Err(code) => {
                if self.stop_after_start {
                    self.set_state(ListenState::Stopped);
                    self.send(OutMessage::Stopped);
                } else {
                    self.set_state(ListenState::Error);
                    self.send(OutMessage::error(code));
                    self.send(self.status_message());
                }
            }
        }
    }

    /// Tears the session down off-thread. `error` is reported once teardown completes.
    fn begin_stop(&mut self, error: Option<ErrorCode>) {
        self.set_state(ListenState::Stopping);
        self.stop_after_start = false;
        self.error_after_stop = error;
        self.capture_events = None;
        self.deepgram_done = None;
        let Some(info) = self.session.take() else {
            self.on_stop_finished();
            return;
        };
        let (tx, rx) = oneshot::channel();
        let SessionInfo {
            capture,
            deepgram_thread,
            ..
        } = info;
        std::thread::Builder::new()
            .name("session-stop".into())
            .spawn(move || {
                capture.stop();
                if deepgram_thread.join().is_err() {
                    log_error!("Deepgram thread panicked");
                }
                let _ = tx.send(());
            })
            .map(|_| self.pending_stop = Some(rx))
            .unwrap_or_else(|e| log_error!("Could not spawn stop thread: {e}"));
    }

    fn on_stop_finished(&mut self) {
        match self.error_after_stop.take() {
            Some(code) => {
                self.set_state(ListenState::Error);
                self.send(OutMessage::error(code));
                self.send(self.status_message());
            }
            None => {
                self.set_state(ListenState::Stopped);
                self.send(OutMessage::Stopped);
            }
        }
    }

    fn on_capture_event(&mut self, event: CaptureEvent) {
        match event {
            CaptureEvent::DeviceChanged(device) => {
                log_info!("Now capturing: {}", device.name);
                if let Some(s) = self.session.as_mut() {
                    s.device = device.name.clone();
                }
                self.send(OutMessage::DeviceChanged {
                    device: device.name,
                });
            }
            CaptureEvent::Fatal(e) => {
                log_error!("Capture failed: {e}");
                let code = match e {
                    AudioError::NoOutputDevice(_) => ErrorCode::NoOutputDevice,
                    _ => ErrorCode::AudioDeviceLost,
                };
                self.begin_stop(Some(code));
            }
        }
    }

    fn on_deepgram_ended(&mut self, result: Option<Result<(), DeepgramError>>) {
        if let Some(s) = self.session.as_mut() {
            s.deepgram_connected = false;
        }
        if self.state != ListenState::Listening {
            return;
        }
        let code = match result {
            Some(Err(e)) => {
                log_error!("{e}");
                match e {
                    DeepgramError::Auth(_) => ErrorCode::DeepgramAuthFailed,
                    _ => ErrorCode::DeepgramDisconnected,
                }
            }
            _ => {
                log_warn!("Deepgram session ended unexpectedly");
                ErrorCode::DeepgramDisconnected
            }
        };
        self.begin_stop(Some(code));
    }

    /// Chrome closed the port: release audio and network resources before exiting.
    async fn shutdown(mut self) {
        if let Some(rx) = self.pending_start.take() {
            if let Ok(Ok(session)) = rx.await {
                stop_session(session);
            }
        }
        if let Some(info) = self.session.take() {
            info.capture.stop();
            let _ = info.deepgram_thread.join();
        }
        if let Some(rx) = self.pending_stop.take() {
            let _ = rx.await;
        }
    }
}
