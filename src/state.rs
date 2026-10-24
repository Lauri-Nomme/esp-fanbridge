//! Everything the tasks share: the configuration snapshot, the pairing state
//! machine's progress, the request flags between the console, the radio and the
//! Zigbee stack, and the counters behind `status`.
//!
//! Kept in one place on purpose: these are the only `static`s in the firmware,
//! and they are all accessed through the same critical-section mutex or through
//! atomics.

use core::cell::RefCell;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as CsMutex;
use embassy_sync::channel::Channel;

use crate::zigbee::advertised;
use crate::pairing::Stage;
use crate::radio::FRAME_GAP_MS;
use crate::radio::REPEATS;
use crate::config::Config;

pub struct State {
    pub cfg: Config,
    pub pairing: bool,
    pub pair_addr: u32,
    pub pair_step: usize,
    pub pair_last_ms: u64,
    /// When the current pairing session started (0 = not started yet).
    pub pair_start_ms: u64,
    /// Transient outcome of the last pairing attempt, shown until `stage_until_ms`.
    pub stage: Stage,
    pub stage_until_ms: u64,
}

impl State {
    /// The stage to report: a transient outcome if it has not expired, else
    /// derived from the pairing state machine.
    pub fn stage_at(&self, now_ms: u64) -> Stage {
        if self.stage != Stage::Idle && now_ms < self.stage_until_ms {
            return self.stage;
        }
        if !self.pairing {
            return Stage::Idle;
        }
        match self.pair_step {
            0 => Stage::WaitingSpeed1,
            1 => Stage::WaitingSpeed2,
            2 => Stage::WaitingSpeed3,
            _ => Stage::WaitingSpeed4,
        }
    }

    pub fn paired_count(&self) -> u8 {
        self.cfg.paired_count()
    }

    /// Remotes that currently have an endpoint (the set is fixed at boot).
    pub fn exposed_count(&self) -> u8 {
        advertised().len().saturating_sub(1) as u8
    }

    /// Whether a pairing change is waiting for a restart to become visible.
    pub fn restart_required(&self) -> bool {
        self.paired_count() != self.exposed_count()
    }
}

pub static STATE: CsMutex<CriticalSectionRawMutex, RefCell<State>> =
    CsMutex::new(RefCell::new(State {
        cfg: Config::EMPTY,
        pairing: false,
        pair_addr: 0,
        pair_step: 0,
        pair_last_ms: 0,
        pair_start_ms: 0,
        stage: Stage::Idle,
        stage_until_ms: 0,
    }));

/// Pending transmit request: 24-bit code, 0 = none.
pub static TX_REQ: AtomicU32 = AtomicU32::new(0);
/// Debug requests handled by the main loop (which owns the SPI + segments).
pub static DUMP_REQ: AtomicBool = AtomicBool::new(false);
pub static STAT_REQ: AtomicBool = AtomicBool::new(false);
pub static CLOCK_REQ: AtomicBool = AtomicBool::new(false);
/// Commands that arrived over Zigbee, drained and printed by the main loop (the
/// cluster handlers run inside the stack and cannot write to the console).
#[derive(Clone, Copy)]
pub struct ZbEvent {
    /// 0 = speed, 1 = power, 2 = direction, 3 = pairing
    pub kind: u8,
    /// slot index, or 0xFF when the command is not about a slot
    pub slot: u8,
    pub value: u8,
}

pub static ZB_EVENTS: Channel<CriticalSectionRawMutex, ZbEvent, 16> = Channel::new();

pub fn zb_event(kind: u8, slot: u8, value: u8) {
    let _ = ZB_EVENTS.try_send(ZbEvent { kind, slot, value });
}

/// Pending state reports, one bit per (cluster, endpoint):
/// bits 0..7  = FanMode of slot N
/// bits 8..15 = direction of slot N
/// bit 16     = pairing switch (OnOff on the master endpoint)
pub static REPORT_DIRTY: AtomicU32 = AtomicU32::new(0);
const REPORT_PAIRING_BIT: u32 = 1 << 16;
/// The whole state, e.g. after joining so the coordinator starts in sync.
const REPORT_ALL: u32 = 0x0001_FFFF;

pub fn mark_fan_mode(slot: usize) {
    REPORT_DIRTY.fetch_or(1 << slot, Ordering::Relaxed);
}

pub fn mark_direction(slot: usize) {
    REPORT_DIRTY.fetch_or(1 << (8 + slot), Ordering::Relaxed);
}

pub fn mark_pairing_switch() {
    REPORT_DIRTY.fetch_or(REPORT_PAIRING_BIT, Ordering::Relaxed);
}

pub fn mark_all_state() {
    REPORT_DIRTY.fetch_or(REPORT_ALL, Ordering::Relaxed);
}

/// Set when the pairing status needs to be reported to the coordinator.
pub static PAIRING_REPORT: AtomicBool = AtomicBool::new(false);
/// Paired count last reported, to notice a change.
pub static PAIRED_NOW: AtomicU32 = AtomicU32::new(0);
/// Exposed (endpoint-bearing) count last reported.
pub static EXPOSED_NOW: AtomicU32 = AtomicU32::new(0);
/// Receiver re-arm count last reported.
pub static REARMS_NOW: AtomicU32 = AtomicU32::new(0);
/// Counters for the `status` command.
pub static TX_COUNT: AtomicU32 = AtomicU32::new(0);
pub static RX_COUNT: AtomicU32 = AtomicU32::new(0);
pub static REARM_COUNT: AtomicU32 = AtomicU32::new(0);
pub static UPTIME_MS: AtomicU32 = AtomicU32::new(0);

/// `true` = packet-mode transmit (radio sends the bitstream), `false` = the
/// per-edge bit-bang path. Off by default until the expanded stream has been
/// confirmed against the physical fans; `txmode packet` switches at runtime.
pub static TX_PACKET: AtomicBool = AtomicBool::new(false);
/// Frames sent per command and the silence between them. The remotes send only
/// a few frames but with a long gap, which a picky receiver may need.
pub static TX_REPEATS: AtomicU32 = AtomicU32::new(REPEATS);
pub static TX_GAP_MS: AtomicU32 = AtomicU32::new(FRAME_GAP_MS);
/// opt-in noisy diagnostics
pub static DEBUG_ON: AtomicBool = AtomicBool::new(false);
/// counts every raw level change on GDO0, including sub-noise glitches
pub static RAW_EDGES: AtomicU32 = AtomicU32::new(0);

