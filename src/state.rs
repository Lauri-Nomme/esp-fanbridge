//! Everything the tasks share: the configuration snapshot, the pairing state
//! machine's progress, the request flags between the console, the radio and the
//! Zigbee stack, and the counters behind `status`.
//!
//! Kept in one place on purpose: these are the only `static`s in the firmware,
//! and they are all accessed through the same critical-section mutex or through
//! atomics.

use core::cell::RefCell;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

use embassy_sync::blocking_mutex::Mutex as CsMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

use crate::config::Config;
use crate::pairing::Stage;
use crate::radio::FRAME_GAP_MS;
use crate::radio::REPEATS;
use crate::zigbee::advertised;

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

/// Depth of the transmit queue. One code holds the air for ~400 ms (8 repeats),
/// so this covers about three seconds of backlog: a scene that sets three fans
/// is six commands, and even a remote-learning burst fits twice over.
pub const TX_QUEUE_LEN: usize = 8;

/// Codes waiting for the radio, in the order their commands arrived.
///
/// This used to be a single slot, and that made it a silent command dropper:
/// `apply()` stores the code and answers the ZCL write immediately, while
/// `send_code()` holds the main loop (and therefore the slot) for ~400 ms.
/// Anything that arrived during those 400 ms overwrote the pending code, so the
/// fan never heard the command - while the write response, the attribute report
/// and Home Assistant all said it had worked.
///
/// A queue keeps the order (speed then direction for the same fan) and makes
/// overflow countable. It drops the *oldest* code when full: every command here
/// is absolute - a speed, a direction, a power state - so the newest one is the
/// better description of what is wanted now, and skipping an older one cannot
/// invert anything.
pub struct TxQueue {
    codes: [u32; TX_QUEUE_LEN],
    head: usize,
    len: usize,
    dropped: u32,
}

impl TxQueue {
    const fn new() -> Self {
        Self {
            codes: [0; TX_QUEUE_LEN],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, code: u32) {
        if self.len == TX_QUEUE_LEN {
            self.head = (self.head + 1) % TX_QUEUE_LEN;
            self.len -= 1;
            self.dropped = self.dropped.saturating_add(1);
        }
        self.codes[(self.head + self.len) % TX_QUEUE_LEN] = code;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<u32> {
        if self.len == 0 {
            return None;
        }
        let code = self.codes[self.head];
        self.head = (self.head + 1) % TX_QUEUE_LEN;
        self.len -= 1;
        Some(code)
    }
}

static TX_QUEUE: CsMutex<CriticalSectionRawMutex, RefCell<TxQueue>> =
    CsMutex::new(RefCell::new(TxQueue::new()));

/// Mirrors of `len` and `dropped`, kept in step inside the critical section.
///
/// The main loop polls the radio edges in a tight loop, so it must not take a
/// lock on every iteration: it reads these instead and only enters the critical
/// section when something is actually waiting. A stale zero costs one extra
/// iteration (~microseconds) before the code is taken.
static TX_QUEUED: AtomicU32 = AtomicU32::new(0);
static TX_LOST: AtomicU32 = AtomicU32::new(0);

/// Queue a code for the radio. Called from the console and from the Zigbee
/// cluster handlers (which run inside the stack task), drained by the main loop.
pub fn tx_push(code: u32) {
    TX_QUEUE.lock(|q| {
        let mut q = q.borrow_mut();
        q.push(code);
        TX_QUEUED.store(q.len as u32, Ordering::Relaxed);
        TX_LOST.store(q.dropped, Ordering::Relaxed);
    });
}

/// Take the next code, if any. The main loop sends one per iteration.
pub fn tx_pop() -> Option<u32> {
    if TX_QUEUED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    TX_QUEUE.lock(|q| {
        let mut q = q.borrow_mut();
        let code = q.pop();
        TX_QUEUED.store(q.len as u32, Ordering::Relaxed);
        code
    })
}

/// Codes still waiting; 0 when the radio has caught up with the commands.
pub fn tx_pending() -> usize {
    TX_QUEUED.load(Ordering::Relaxed) as usize
}

/// Codes thrown away because the queue was full (a bug, not a mode).
pub fn tx_dropped() -> u32 {
    TX_LOST.load(Ordering::Relaxed)
}
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
/// Dropped-command count last reported (see backlog #54).
pub static TXDROPPED_NOW: AtomicU32 = AtomicU32::new(0);
/// Counters for the `status` command.
pub static TX_COUNT: AtomicU32 = AtomicU32::new(0);
/// Dropped-command count last printed on the console (so a full queue is
/// announced once, when it happens, instead of silently losing commands).
pub static TX_DROPPED_NOW: AtomicU32 = AtomicU32::new(0);
/// Ignore received 433 MHz frames until this uptime: our own transmission is
/// metres away from our own antenna, so the receiver hears it and the decoder
/// can turn it into a *different* button (one bit of the envelope is enough),
/// which then overwrites the state the command just set.
pub static IGNORE_RX_UNTIL_MS: AtomicU32 = AtomicU32::new(0);
pub static RX_COUNT: AtomicU32 = AtomicU32::new(0);
pub static REARM_COUNT: AtomicU32 = AtomicU32::new(0);
/// Attribute reports the coordinator acknowledged, and ones that had to be
/// retried. A rising failure count with a working RF side means the Zigbee link
/// is the problem, not the fan.
pub static REPORT_OK: AtomicU32 = AtomicU32::new(0);
pub static REPORT_FAIL: AtomicU32 = AtomicU32::new(0);
pub static UPTIME_MS: AtomicU32 = AtomicU32::new(0);

/// Set by the console, drained by the main loop: re-ascociate with the network.
///
/// A device that only ever *resumes* keeps the parent's child-table entry from
/// the association it originally joined with. If the capability information has
/// changed since (this device went from sleepy to `rxOnWhenIdle`), the parent
/// keeps queueing frames for a device that no longer polls, so commands arrive
/// late, in batches, or expire.
pub static REJOIN_REQ: AtomicBool = AtomicBool::new(false);

/// Set by the console: forget the network and commission from scratch.
///
/// The next boot scans and joins as a new association, which is the only thing
/// that makes a parent build a fresh child-table entry for us - and the recipe
/// in `docs/DESIGN.md` §9, in one command.
pub static LEAVE_REQ: AtomicBool = AtomicBool::new(false);

/// Uptime at which to restart, or 0. A coordinator command sets it instead of
/// resetting on the spot, so the write response still reaches the caller.
pub static REBOOT_AT_MS: AtomicU32 = AtomicU32::new(0);

/// Ask for a restart in `delay_ms`, unless one is already pending.
pub fn request_reboot_in(delay_ms: u32) {
    let at = UPTIME_MS
        .load(Ordering::Relaxed)
        .saturating_add(delay_ms)
        .max(1);
    REBOOT_AT_MS.store(at, Ordering::Relaxed);
}

/// `true` = packet-mode transmit (radio sends the bitstream), `false` = the
/// per-edge bit-bang path. Off by default until the expanded stream has been
/// confirmed against the physical fans; `txmode packet` switches at runtime.
pub static TX_PACKET: AtomicBool = AtomicBool::new(false);
/// Bit in a queued transmit code that means "send this one with the radio's
/// packet engine". Resolved per slot when the code is queued, so one fan can be
/// tried in packet mode while the others keep the bit-bang path (backlog #5).
pub const TX_FLAG_PACKET: u32 = 0x8000_0000;
/// Per-slot transmit-mode override: 0 = follow `txmode`, 1 = packet, 2 = async.
///
/// Runtime only, never persisted: it exists for the A/B experiment behind #5 -
/// one fan at a time, everything else untouched, and a power cycle puts it back.
pub static TX_MODE_SLOT: [AtomicU8; 8] = [const { AtomicU8::new(0) }; 8];

/// Whether a code for `slot` should go out via the packet engine.
pub fn tx_packet_for(slot: usize) -> bool {
    match TX_MODE_SLOT.get(slot).map(|m| m.load(Ordering::Relaxed)) {
        Some(1) => true,
        Some(2) => false,
        _ => TX_PACKET.load(Ordering::Relaxed),
    }
}

/// Set the per-slot override (0 = default, 1 = packet, 2 = async).
pub fn set_tx_mode_slot(slot: usize, mode: u8) {
    if let Some(m) = TX_MODE_SLOT.get(slot) {
        m.store(mode, Ordering::Relaxed);
    }
}

/// Queue `code` with the global transmit mode (no slot: raw console test codes).
pub fn tx_push_default(code: u32) {
    tx_push(
        code | if TX_PACKET.load(Ordering::Relaxed) {
            TX_FLAG_PACKET
        } else {
            0
        },
    );
}

/// Queue `code` for `slot`, resolving that slot's transmit mode now.
pub fn tx_push_for(slot: usize, code: u32) {
    tx_push(
        code | if tx_packet_for(slot) {
            TX_FLAG_PACKET
        } else {
            0
        },
    );
}
/// Frames sent per command and the silence between them. The remotes send only
/// a few frames but with a long gap, which a picky receiver may need.
pub static TX_REPEATS: AtomicU32 = AtomicU32::new(REPEATS);
pub static TX_GAP_MS: AtomicU32 = AtomicU32::new(FRAME_GAP_MS);
/// opt-in noisy diagnostics
pub static DEBUG_ON: AtomicBool = AtomicBool::new(false);
/// counts every raw level change on GDO0, including sub-noise glitches
pub static RAW_EDGES: AtomicU32 = AtomicU32::new(0);
