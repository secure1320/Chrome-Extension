//! JSON messages exchanged with the Chrome extension. Only text/status/errors
//! ever leave the companion; raw audio never does.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Start,
    Stop,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NoApiKey,
    NoOutputDevice,
    AudioInitFailed,
    AudioDeviceLost,
    DeepgramAuthFailed,
    DeepgramConnectionFailed,
    DeepgramDisconnected,
    InvalidCommand,
}

impl ErrorCode {
    /// Short user-facing text. Diagnostic details belong in the local log only.
    pub fn message(self) -> &'static str {
        match self {
            ErrorCode::NoApiKey => "DEEPGRAM_API_KEY is not configured.",
            ErrorCode::NoOutputDevice => "No Windows playback device is available.",
            ErrorCode::AudioInitFailed => "Could not start Windows system audio capture.",
            ErrorCode::AudioDeviceLost => "The Windows playback device was lost.",
            ErrorCode::DeepgramAuthFailed => "Deepgram rejected the API key.",
            ErrorCode::DeepgramConnectionFailed => "Could not connect to transcription service.",
            ErrorCode::DeepgramDisconnected => "Lost connection to transcription service.",
            ErrorCode::InvalidCommand => "Unsupported command.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum OutMessage {
    Started {
        device: String,
        sample_rate: u32,
        channels: u16,
    },
    Stopped,
    Status {
        running: bool,
        state: &'static str,
        deepgram_connected: bool,
        device: Option<String>,
    },
    TranscriptPartial {
        text: String,
    },
    TranscriptFinal {
        text: String,
    },
    DeviceChanged {
        device: String,
    },
    Error {
        code: ErrorCode,
        message: &'static str,
    },
}

impl OutMessage {
    pub fn error(code: ErrorCode) -> Self {
        OutMessage::Error {
            code,
            message: code.message(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_commands() {
        assert_eq!(serde_json::from_str::<Command>(r#"{"type":"start"}"#).unwrap(), Command::Start);
        assert_eq!(serde_json::from_str::<Command>(r#"{"type":"stop"}"#).unwrap(), Command::Stop);
        assert_eq!(serde_json::from_str::<Command>(r#"{"type":"status"}"#).unwrap(), Command::Status);
        assert!(serde_json::from_str::<Command>(r#"{"type":"record_mic"}"#).is_err());
    }

    #[test]
    fn serializes_outbound_shapes() {
        let started = OutMessage::Started {
            device: "Speakers".into(),
            sample_rate: 48_000,
            channels: 1,
        };
        assert_eq!(
            serde_json::to_value(&started).unwrap(),
            json!({"type":"started","device":"Speakers","sampleRate":48000,"channels":1})
        );
        let status = OutMessage::Status {
            running: true,
            state: "listening",
            deepgram_connected: true,
            device: Some("Speakers".into()),
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({"type":"status","running":true,"state":"listening","deepgramConnected":true,"device":"Speakers"})
        );
        assert_eq!(
            serde_json::to_value(OutMessage::TranscriptFinal { text: "Hello everyone.".into() }).unwrap(),
            json!({"type":"transcript_final","text":"Hello everyone."})
        );
        assert_eq!(
            serde_json::to_value(OutMessage::error(ErrorCode::NoApiKey)).unwrap(),
            json!({"type":"error","code":"NO_API_KEY","message":"DEEPGRAM_API_KEY is not configured."})
        );
        assert_eq!(serde_json::to_value(OutMessage::Stopped).unwrap(), json!({"type":"stopped"}));
    }
}
