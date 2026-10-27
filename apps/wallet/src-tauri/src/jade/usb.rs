//! Jade over a USB cable (desktop).
//!
//! Plugged in, Jade shows up as a serial port (its USB-to-serial chip, or
//! the ESP32-S3's own USB on Jade Plus) and speaks the same CBOR RPC as over
//! Bluetooth. No pairing; the PIN comes up as usual.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::Duration;

use async_trait::async_trait;
use serialport::{SerialPort, SerialPortType};
use sessions_core::{Error, Result};
use tokio::sync::{mpsc, Mutex};

use super::rpc::{Fail, Link};

/// USB ids Jade's serial port can have (from Blockstream's own client).
const JADE_IDS: &[(u16, u16)] = &[
    (0x10c4, 0xea60), // CP210x (Jade 1.0)
    (0x1a86, 0x55d4), // CH9102 (Jade 1.1)
    (0x0403, 0x6001), // FTDI
    (0x1a86, 0x7523), // CH340
    (0x303a, 0x4001), // ESP32-S3 native USB (Jade Plus)
    (0x303a, 0x1001), // ESP32-S3 USB-serial/JTAG
];

const BAUD: u32 = 115_200;

/// The plugged-in Jade's port, if there is one.
pub fn find() -> Option<String> {
    let ports = serialport::available_ports().ok()?;
    let mut found: Vec<String> = ports
        .into_iter()
        .filter(|p| match &p.port_type {
            SerialPortType::UsbPort(u) => JADE_IDS.contains(&(u.vid, u.pid)),
            _ => false,
        })
        .map(|p| p.port_name)
        // macOS lists each port twice; the "cu." one is for outgoing use.
        .filter(|n| !n.starts_with("/dev/tty.") || !cfg!(target_os = "macos"))
        .collect();
    found.sort();
    found.into_iter().next()
}

pub async fn open(path: &str) -> Result<Box<dyn Link>> {
    let path = path.to_string();
    let port = tokio::task::spawn_blocking(move || {
        serialport::new(&path, BAUD)
            // Raising DTR/RTS on open resets the ESP32: keep them low.
            .dtr_on_open(false)
            .timeout(Duration::from_millis(200))
            .open()
            .map(|mut p| {
                let _ = p.write_request_to_send(false);
                p
            })
    })
    .await
    .map_err(|e| Error::Invalid(e.to_string()))?
    .map_err(open_error)?;

    let reader = port.try_clone().map_err(open_error)?;
    let (tx, rx) = mpsc::unbounded_channel();
    let up = Arc::new(AtomicBool::new(true));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (up, stop) = (up.clone(), stop.clone());
        std::thread::spawn(move || read_loop(reader, tx, up, stop));
    }
    Ok(Box::new(UsbLink {
        port: Arc::new(SyncMutex::new(port)),
        incoming: Mutex::new(rx),
        up,
        stop,
    }))
}

fn read_loop(mut port: Box<dyn SerialPort>, tx: mpsc::UnboundedSender<Vec<u8>>, up: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::SeqCst) {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                tracing::info!("jade usb: read stopped ({e})");
                break;
            }
        }
    }
    up.store(false, Ordering::SeqCst);
}

fn open_error(e: serialport::Error) -> Error {
    let msg = e.to_string();
    if msg.to_lowercase().contains("permission") {
        Error::Invalid(if cfg!(target_os = "linux") {
            "This computer won't let the app use Jade's USB port. Add yourself to the \"dialout\" group \
             (sudo usermod -aG dialout $USER), log out and in, then try again"
                .into()
        } else {
            "Couldn't open Jade's USB connection. Close other apps using Jade, then try again".into()
        })
    } else if msg.to_lowercase().contains("busy") {
        Error::Invalid("Jade's USB connection is in use by another app. Close it, then try again".into())
    } else {
        Error::Invalid(format!("Couldn't open Jade over USB ({msg})"))
    }
}

struct UsbLink {
    port: Arc<SyncMutex<Box<dyn SerialPort>>>,
    incoming: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    up: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

#[async_trait]
impl Link for UsbLink {
    async fn send(&self, bytes: &[u8]) -> std::result::Result<(), Fail> {
        let port = self.port.clone();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut p = port.lock().map_err(|_| Fail::Link("Jade's USB link failed".into()))?;
            p.write_all(&bytes)
                .and_then(|_| p.flush())
                .map_err(|e| Fail::Link(format!("Jade disconnected ({e})")))
        })
        .await
        .map_err(|e| Fail::Link(e.to_string()))?
    }

    async fn recv(&self) -> std::result::Result<Vec<u8>, Fail> {
        self.incoming
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| Fail::Link("Jade was unplugged".into()))
    }

    fn is_up(&self) -> bool {
        self.up.load(Ordering::SeqCst)
    }

    fn interface(&self) -> &'static str {
        "SERIAL"
    }

    async fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for UsbLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
