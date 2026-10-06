//! Deepgram live speech-to-text over a single WebSocket per listening session.
//!
//! The socket is driven from a dedicated thread with std networking (connect in
//! blocking mode, then non-blocking I/O). The API key is only used to build the
//! `Authorization` header; it is never logged, printed, or forwarded to Chrome.

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;
use tungstenite::http::{header::AUTHORIZATION, HeaderValue};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use windows::core::w;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

use crate::{log_info, log_warn};

pub const API_KEY_ENV: &str = "DEEPGRAM_API_KEY";
/// Optional endpoint override, used only for local testing against a mock server.
const ENDPOINT_OVERRIDE_ENV: &str = "SYSTEM_AUDIO_DEEPGRAM_ENDPOINT";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const KEEPALIVE_AFTER: Duration = Duration::from_secs(4);
/// Upper bound on how long incoming transcripts wait while no audio arrives.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Bounded outgoing buffer (~5 s of 48 kHz PCM16 mono) for when the network stalls.
const MAX_WRITE_BUFFER: usize = 512 * 1024;
/// Digital-silence chunks still streamed after audio stops, so Deepgram can endpoint
/// the last utterance before we switch to KeepAlive-only (avoids streaming paid silence).
const SILENCE_TAIL_CHUNKS: u32 = 10;

/// All Deepgram model/feature settings live here.
#[derive(Debug, Clone)]
pub struct DeepgramConfig {
    pub endpoint: String,
    pub model: String,
    pub language: String,
    pub interim_results: bool,
    pub punctuate: bool,
    pub smart_format: bool,
}

impl Default for DeepgramConfig {
    fn default() -> Self {
        Self {
            endpoint: std::env::var(ENDPOINT_OVERRIDE_ENV)
                .ok()
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| "wss://api.deepgram.com/v1/listen".into()),
            model: "nova-3".into(),
            language: "en".into(),
            interim_results: true,
            punctuate: true,
            smart_format: true,
        }
    }
}

impl DeepgramConfig {
    pub fn listen_url(&self, sample_rate: u32) -> String {
        format!(
            "{}?model={}&language={}&encoding=linear16&sample_rate={}&channels=1&interim_results={}&punctuate={}&smart_format={}",
            self.endpoint,
            self.model,
            self.language,
            sample_rate,
            self.interim_results,
            self.punctuate,
            self.smart_format
        )
    }
}

/// Reads the key from the process environment, falling back to the persistent
/// user environment (`HKCU\Environment`) so `setx` works without restarting Chrome.
pub fn load_api_key() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .or_else(read_user_environment_key)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

fn read_user_environment_key() -> Option<String> {
    let mut size: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            w!("DEEPGRAM_API_KEY"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status.is_err() || size < 2 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            w!("DEEPGRAM_API_KEY"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16(&buf[..len]).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    Partial(String),
    Final(String),
}

#[derive(Debug, Clone)]
pub enum DeepgramError {
    Auth(String),
    Connect(String),
    Disconnected(String),
}

impl std::fmt::Display for DeepgramError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeepgramError::Auth(d) => write!(f, "Deepgram rejected the API key: {d}"),
            DeepgramError::Connect(d) => write!(f, "Deepgram connection failed: {d}"),
            DeepgramError::Disconnected(d) => write!(f, "Deepgram connection lost: {d}"),
        }
    }
}

pub type DeepgramSocket = WebSocket<MaybeTlsStream<TcpStream>>;

fn tcp_connect(host: &str, port: u16) -> Result<TcpStream, DeepgramError> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| DeepgramError::Connect(format!("DNS lookup failed: {e}")))?;
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    Err(DeepgramError::Connect(match last_err {
        Some(e) => format!("TCP connect failed: {e}"),
        None => "no addresses resolved".into(),
    }))
}

/// Opens the WebSocket (blocking, with timeouts). Call from a non-async thread.
pub fn connect(
    config: &DeepgramConfig,
    api_key: &str,
    sample_rate: u32,
) -> Result<DeepgramSocket, DeepgramError> {
    let url = config.listen_url(sample_rate);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| DeepgramError::Connect(format!("bad request: {e}")))?;
    let mut auth = HeaderValue::from_str(&format!("Token {api_key}"))
        .map_err(|_| DeepgramError::Auth("API key contains invalid characters".into()))?;
    auth.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, auth);

    let host = request
        .uri()
        .host()
        .ok_or_else(|| DeepgramError::Connect("endpoint has no host".into()))?
        .to_string();
    let port = request.uri().port_u16().unwrap_or(443);

    log_info!(
        "Connecting to Deepgram (model={}, sample_rate={}, channels=1, encoding=linear16)",
        config.model,
        sample_rate
    );
    let tcp = tcp_connect(&host, port)?;
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(CONNECT_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(CONNECT_TIMEOUT));

    let ws_config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_WRITE_BUFFER);

    match tungstenite::client_tls_with_config(request, tcp, Some(ws_config), None) {
        Ok((socket, _response)) => {
            let tcp = match socket.get_ref() {
                MaybeTlsStream::Plain(s) => s,
                MaybeTlsStream::NativeTls(s) => s.get_ref(),
                _ => return Err(DeepgramError::Connect("unexpected stream type".into())),
            };
            tcp.set_read_timeout(None)
                .and_then(|_| tcp.set_write_timeout(None))
                .and_then(|_| tcp.set_nonblocking(true))
                .map_err(|e| DeepgramError::Connect(format!("socket setup: {e}")))?;
            log_info!("Deepgram connection established");
            Ok(socket)
        }
        Err(HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            let status = response.status();
            let detail = response
                .headers()
                .get("dg-error")
                .and_then(|v| v.to_str().ok())
                .map(|s| format!("HTTP {status}: {s}"))
                .unwrap_or_else(|| format!("HTTP {status}"));
            if status.as_u16() == 401 || status.as_u16() == 403 {
                Err(DeepgramError::Auth(detail))
            } else {
                Err(DeepgramError::Connect(detail))
            }
        }
        Err(HandshakeError::Failure(e)) => Err(DeepgramError::Connect(e.to_string())),
        Err(HandshakeError::Interrupted(_)) => {
            Err(DeepgramError::Connect("handshake interrupted".into()))
        }
    }
}

fn is_digital_silence(chunk: &[u8]) -> bool {
    chunk.iter().all(|&b| b == 0)
}

fn is_would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock)
}

/// Queues a message. `WouldBlock` means it is buffered and will be flushed later.
fn send(socket: &mut DeepgramSocket, msg: Message) -> Result<(), DeepgramError> {
    match socket.send(msg) {
        Ok(()) => Ok(()),
        Err(e) if is_would_block(&e) => Ok(()),
        Err(tungstenite::Error::WriteBufferFull(_)) => {
            log_warn!("Network is behind; dropped one audio frame");
            Ok(())
        }
        Err(e) => Err(DeepgramError::Disconnected(e.to_string())),
    }
}

/// Streams audio chunks until `audio_rx` disconnects (capture stopped), then performs
/// a graceful `CloseStream` so trailing finals are still delivered. Blocking.
pub fn run_session(
    mut socket: DeepgramSocket,
    audio_rx: Receiver<Vec<u8>>,
    mut on_event: impl FnMut(TranscriptEvent),
) -> Result<(), DeepgramError> {
    let mut last_sent = Instant::now();
    let mut silent_chunks: u32 = 0;
    let mut closing_deadline: Option<Instant> = None;

    let result = 'session: loop {
        // Deliver every message Deepgram has sent so far.
        loop {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    if let Some(event) = parse_message(text.as_str()) {
                        on_event(event);
                    }
                }
                Ok(Message::Close(frame)) => {
                    if closing_deadline.is_some() {
                        break 'session Ok(());
                    }
                    let reason = frame
                        .map(|f| format!("code {} {}", u16::from(f.code), f.reason))
                        .unwrap_or_else(|| "closed by server".into());
                    break 'session Err(DeepgramError::Disconnected(reason));
                }
                Ok(_) => {}
                Err(e) if is_would_block(&e) => break,
                Err(e) => {
                    if closing_deadline.is_some() {
                        break 'session Ok(());
                    }
                    break 'session Err(DeepgramError::Disconnected(e.to_string()));
                }
            }
        }

        if let Some(deadline) = closing_deadline {
            if Instant::now() >= deadline {
                log_warn!("Deepgram did not close in time; dropping connection");
                break Ok(());
            }
            let _ = socket.flush();
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        match audio_rx.recv_timeout(POLL_INTERVAL) {
            Ok(chunk) => {
                if is_digital_silence(&chunk) {
                    silent_chunks = silent_chunks.saturating_add(1);
                    if silent_chunks > SILENCE_TAIL_CHUNKS {
                        continue;
                    }
                    if silent_chunks == SILENCE_TAIL_CHUNKS {
                        if let Err(e) = send(&mut socket, Message::Text(r#"{"type":"Finalize"}"#.into())) {
                            break Err(e);
                        }
                    }
                } else {
                    silent_chunks = 0;
                }
                if let Err(e) = send(&mut socket, Message::Binary(chunk.into())) {
                    break Err(e);
                }
                last_sent = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Err(e) = socket.flush() {
                    if !is_would_block(&e) {
                        break Err(DeepgramError::Disconnected(e.to_string()));
                    }
                }
                if last_sent.elapsed() >= KEEPALIVE_AFTER {
                    if let Err(e) = send(&mut socket, Message::Text(r#"{"type":"KeepAlive"}"#.into())) {
                        break Err(e);
                    }
                    last_sent = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Err(e) = send(&mut socket, Message::Text(r#"{"type":"CloseStream"}"#.into())) {
                    log_warn!("Could not send CloseStream: {e}");
                    break Ok(());
                }
                closing_deadline = Some(Instant::now() + CLOSE_TIMEOUT);
            }
        }
    };

    let _ = socket.close(None);
    let _ = socket.flush();
    log_info!("Deepgram connection closed");
    result
}

#[derive(Deserialize)]
struct DgMessage {
    #[serde(rename = "type")]
    kind: Option<String>,
    is_final: Option<bool>,
    channel: Option<DgChannel>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct DgChannel {
    alternatives: Vec<DgAlternative>,
}

#[derive(Deserialize)]
struct DgAlternative {
    transcript: String,
}

/// Normalizes a Deepgram server message into a transcript event; ignores
/// metadata, speech-started/utterance-end events and empty transcripts.
pub fn parse_message(text: &str) -> Option<TranscriptEvent> {
    let msg: DgMessage = serde_json::from_str(text).ok()?;
    match msg.kind.as_deref() {
        Some("Results") => {}
        Some("Error") => {
            log_warn!(
                "Deepgram error message: {}",
                msg.description.as_deref().unwrap_or("unknown")
            );
            return None;
        }
        _ => return None,
    }
    let transcript = msg
        .channel?
        .alternatives
        .into_iter()
        .next()?
        .transcript
        .trim()
        .to_string();
    if transcript.is_empty() {
        return None;
    }
    Some(if msg.is_final.unwrap_or(false) {
        TranscriptEvent::Final(transcript)
    } else {
        TranscriptEvent::Partial(transcript)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_contains_actual_rate_and_options() {
        let url = DeepgramConfig::default().listen_url(48_000);
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?"));
        for part in [
            "model=nova-3",
            "encoding=linear16",
            "sample_rate=48000",
            "channels=1",
            "interim_results=true",
            "punctuate=true",
            "smart_format=true",
        ] {
            assert!(url.contains(part), "missing {part}");
        }
    }

    #[test]
    fn parses_partial_and_final() {
        let partial = r#"{"type":"Results","channel_index":[0,1],"duration":1.0,"start":0.0,"is_final":false,"speech_final":false,"channel":{"alternatives":[{"transcript":"Hello every","confidence":0.9,"words":[]}]}}"#;
        let fin = r#"{"type":"Results","is_final":true,"speech_final":true,"channel":{"alternatives":[{"transcript":"Hello everyone.","confidence":0.99,"words":[]}]}}"#;
        assert_eq!(parse_message(partial), Some(TranscriptEvent::Partial("Hello every".into())));
        assert_eq!(parse_message(fin), Some(TranscriptEvent::Final("Hello everyone.".into())));
    }

    #[test]
    fn ignores_empty_and_non_results() {
        let empty = r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":""}]}}"#;
        let meta = r#"{"type":"Metadata","request_id":"abc"}"#;
        assert_eq!(parse_message(empty), None);
        assert_eq!(parse_message(meta), None);
        assert_eq!(parse_message("not json"), None);
    }

    #[test]
    fn silence_detection() {
        assert!(is_digital_silence(&[0, 0, 0, 0]));
        assert!(!is_digital_silence(&[0, 0, 1, 0]));
    }
}
