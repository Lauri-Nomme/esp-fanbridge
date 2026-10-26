//! Log output that never blocks.
//!
//! The stack logs through the `log` crate, and the default backend writes
//! straight into the USB-Serial-JTAG FIFO, which only drains while something
//! reads the other end. A debug build with nobody attached therefore wedged
//! whatever task logged - the Zigbee stack included. This module takes over:
//! lines are formatted into a small ring buffer, and the main loop hands them to
//! the console, which drops output rather than waiting.

use core::cell::RefCell;
use core::fmt::Write;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;

/// Enough for a burst of diagnostics, small enough to stay cheap.
const RING: usize = 2048;

struct LogRing {
    buf: [u8; RING],
    head: usize,
    len: usize,
    dropped: usize,
}

impl LogRing {
    const fn new() -> Self {
        Self {
            buf: [0; RING],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.len == RING {
                self.dropped = self.dropped.saturating_add(1);
                continue;
            }
            let at = (self.head + self.len) % RING;
            self.buf[at] = byte;
            self.len += 1;
        }
    }

    fn pop(&mut self, out: &mut [u8]) -> usize {
        let take = self.len.min(out.len());
        for (i, slot) in out.iter_mut().take(take).enumerate() {
            *slot = self.buf[(self.head + i) % RING];
        }
        self.head = (self.head + take) % RING;
        self.len -= take;
        take
    }
}

static RING_BUF: Mutex<CriticalSectionRawMutex, RefCell<LogRing>> =
    Mutex::new(RefCell::new(LogRing::new()));

struct Sink<'a>(&'a mut LogRing);

impl Write for Sink<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.push(s.as_bytes());
        Ok(())
    }
}

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        RING_BUF.lock(|ring| {
            let mut ring = ring.borrow_mut();
            let mut sink = Sink(&mut ring);
            // a panic in `write_str` would be worse than a lost line: the sink
            // never fails, it drops what does not fit
            let _ = writeln!(sink, "[{}] {}", record.level(), record.args());
        });
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

/// Install the non-blocking logger with `level`.
pub fn init(level: log::LevelFilter) {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}

/// Hand buffered log bytes to `write`, in 128-byte chunks.
///
/// Called from the main loop with the console as the sink; the console drops
/// output when the host is not reading, so this never blocks the caller.
pub fn drain(mut write: impl FnMut(&[u8])) {
    let mut chunk = [0u8; 128];
    loop {
        let n = RING_BUF.lock(|ring| ring.borrow_mut().pop(&mut chunk));
        if n == 0 {
            return;
        }
        write(&chunk[..n]);
    }
}

/// Bytes lost because nothing drained the ring (shown in `status`).
pub fn dropped() -> usize {
    RING_BUF.lock(|ring| ring.borrow().dropped)
}

/// Parse a level name for the `log` console command.
pub fn parse_level(name: &str) -> Option<log::LevelFilter> {
    Some(match name {
        "off" => log::LevelFilter::Off,
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => return None,
    })
}

/// The level the firmware starts at: quiet enough for unattended use.
pub const fn default_level() -> log::LevelFilter {
    log::LevelFilter::Info
}
