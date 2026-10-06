//! Minimal diagnostic logger.
//!
//! Writes to stderr and to `%LOCALAPPDATA%\SystemAudioCompanion\logs\companion.log`.
//! Never writes to stdout: stdout carries the Chrome Native Messaging protocol.
//! Callers must never pass secrets (API key, auth header) or raw audio here.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static ECHO_STDERR: AtomicBool = AtomicBool::new(true);

pub fn log_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("SystemAudioCompanion").join("logs"))
}

/// Opens the log file (rotating once if it exceeds `MAX_LOG_BYTES`).
/// Returns the log file path when file logging is available.
pub fn init(echo_stderr: bool) -> Option<PathBuf> {
    ECHO_STDERR.store(echo_stderr, Ordering::Relaxed);
    let path = log_dir().and_then(|dir| {
        fs::create_dir_all(&dir).ok()?;
        let path = dir.join("companion.log");
        if fs::metadata(&path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
            let _ = fs::rename(&path, dir.join("companion.old.log"));
        }
        Some(path)
    });
    let file = path
        .as_ref()
        .and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
    let has_file = file.is_some();
    let _ = LOG_FILE.set(Mutex::new(file));
    if has_file {
        path
    } else {
        None
    }
}

pub fn write(level: &str, args: fmt::Arguments) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} [{}] {}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, level, args
    );
    if ECHO_STDERR.load(Ordering::Relaxed) {
        let _ = std::io::stderr().write_all(format!("[{}] {}\n", level, args).as_bytes());
    }
    if let Some(lock) = LOG_FILE.get() {
        if let Ok(mut guard) = lock.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
    }
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logging::write("INFO", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logging::write("WARN", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::logging::write("ERROR", format_args!($($arg)*)) };
}
