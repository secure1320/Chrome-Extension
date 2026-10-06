//! Chrome Native Messaging framing: 4-byte little-endian length + UTF-8 JSON.
//!
//! stdin carries Chrome -> companion, stdout carries companion -> Chrome.
//! Exactly one thread (the writer) ever touches stdout.

use std::io::{self, Read, Write};
use std::thread::JoinHandle;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::protocol::{Command, OutMessage};
use crate::{log_error, log_warn};

/// Chrome limits host -> extension messages to 1 MB; we apply the same cap inbound.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum Inbound {
    Command(Command),
    Invalid,
    Closed,
}

/// Reads one framed message. Returns `Ok(None)` on clean EOF.
pub fn read_frame(reader: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large ({len} bytes)"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

pub fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "message too large"));
    }
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Blocking stdin reader thread; forwards parsed commands and signals EOF.
pub fn spawn_stdin_reader(tx: UnboundedSender<Inbound>) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("nm-stdin".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            loop {
                match read_frame(&mut stdin) {
                    Ok(Some(payload)) => {
                        let msg = match serde_json::from_slice::<Command>(&payload) {
                            Ok(cmd) => Inbound::Command(cmd),
                            Err(e) => {
                                log_warn!("Ignoring invalid command ({} bytes): {e}", payload.len());
                                Inbound::Invalid
                            }
                        };
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log_error!("Native Messaging read failed: {e}");
                        break;
                    }
                }
            }
            let _ = tx.send(Inbound::Closed);
        })
}

/// The single stdout owner. Exits when all senders are dropped or stdout breaks.
pub fn spawn_stdout_writer(mut rx: UnboundedReceiver<OutMessage>) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("nm-stdout".into())
        .spawn(move || {
            let mut stdout = io::stdout().lock();
            while let Some(msg) = rx.blocking_recv() {
                let payload = match serde_json::to_vec(&msg) {
                    Ok(p) => p,
                    Err(e) => {
                        log_error!("Could not serialize message: {e}");
                        continue;
                    }
                };
                if let Err(e) = write_frame(&mut stdout, &payload) {
                    log_error!("Native Messaging write failed: {e}");
                    break;
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"type":"stopped"}"#).unwrap();
        assert_eq!(&buf[..4], &18u32.to_le_bytes());
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), br#"{"type":"stopped"}"#);
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    #[test]
    fn rejects_oversized_frames() {
        let mut data = ((MAX_MESSAGE_BYTES + 1) as u32).to_le_bytes().to_vec();
        data.extend_from_slice(b"{}");
        assert!(read_frame(&mut Cursor::new(data)).is_err());
    }

    #[test]
    fn truncated_payload_is_error() {
        let mut data = 10u32.to_le_bytes().to_vec();
        data.extend_from_slice(b"{}");
        assert!(read_frame(&mut Cursor::new(data)).is_err());
    }
}
