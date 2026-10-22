//! Rust `tracing` output to Android's logcat, tag `SessionsWallet`, so
//! `adb logcat | grep -i sessions` shows the wallet's own lines next to the
//! Kotlin plugins' (`SessionsBitbox`, `SessionsJade`).
//!
//! Only the wallet's own crates log at debug; everything else at warn.

use std::ffi::{c_char, c_int, CString};
use std::io::Write;

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[link(name = "log")]
extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

/// android/log.h: ANDROID_LOG_INFO.
const INFO: c_int = 4;

/// Install once, early. Later calls (and a subscriber already set) are ignored.
pub fn init() {
    let targets = Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("sessions_wallet_lib", LevelFilter::DEBUG)
        .with_target("rail_onchain", LevelFilter::INFO)
        .with_target("sessions_core", LevelFilter::INFO)
        .with_target("tauri_plugin_bitbox", LevelFilter::DEBUG)
        .with_target("tauri_plugin_jade", LevelFilter::DEBUG);
    let fmt = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .without_time()
        .with_writer(Logcat);
    let _ = tracing_subscriber::registry()
        .with(fmt)
        .with(targets)
        .try_init();
}

struct Logcat;

/// One formatted event, written to logcat when dropped.
struct Line(Vec<u8>);

impl Write for Line {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0);
        bytes.retain(|b| *b != 0);
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        if bytes.is_empty() {
            return;
        }
        let (Ok(text), Ok(tag)) = (CString::new(bytes), CString::new("SessionsWallet")) else {
            return;
        };
        // SAFETY: both are valid NUL-terminated strings for the call's duration.
        unsafe {
            __android_log_write(INFO, tag.as_ptr(), text.as_ptr());
        }
    }
}

impl<'a> MakeWriter<'a> for Logcat {
    type Writer = Line;

    fn make_writer(&'a self) -> Line {
        Line(Vec::new())
    }
}
