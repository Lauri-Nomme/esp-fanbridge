//! `esp-fanbridge` — 433 MHz fan-remote bridge: RF + config + TTY menu + Zigbee.
//!
//! Learns up to 8 VT100-style fan remotes and exposes each one to Zigbee (and
//! therefore Home Assistant) as its own Fan Control endpoint, while still
//! being able to replay any of the remote's buttons.
//!
//! Wiring: CSN=IO4 SCK=IO23 MOSI=IO22 MISO=IO21 GDO0=IO5 VCC=3V3 GND=GND
//!         LED=IO15 (onboard)
//!
//! Protocol (reverse engineered, see docs/PROTOCOL.md):
//!   433.92 MHz ASK/OOK, 24 bits MSB first, PWM symbols
//!   '0' = 400 us high + 1200 us low, '1' = 1200 us high + 400 us low
//!   20-bit address << 4 | 4-bit button
//!   speed1=0x2 speed2=0x1 speed3=0xC speed4=0x7
//!   dir in=0x8 dir out=0x4 dir toggle=0x3 power=0xA

#![no_std]
#![no_main]

use core::cell::RefCell;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as CsMutex;
use embassy_sync::channel::Channel;
use embassy_time::Delay as AsyncDelay;
use embassy_time::Timer;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::delay::Delay;
use esp_hal::gpio::Flex;
use esp_hal::gpio::InputConfig;
use esp_hal::gpio::Level;
use esp_hal::gpio::Output;
use esp_hal::gpio::OutputConfig;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::spi::Mode;
use esp_hal::spi::master::Config as SpiConfig;
use esp_hal::spi::master::Spi;
use esp_hal::time::Instant;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::UsbSerialJtag;
use esp_println::println;
use zigbee::zdo::ClusterReply;
use zigbee::zdo::ClusterRequest;
use zigbee::zdo::ClusterRequestHandler;
use zigbee::config::StackConfig;
use zigbee::nwk::nib::CapabilityInformation;
use zigbee::zdo::config::DiscoveryType;
use zigbee::zdo::descriptor::DeviceDescriptorConfig;
use zigbee::zdo::descriptor::EndpointDescriptor;
use zigbee::zdo::descriptor::NodeDescriptorConfig;
use zigbee::zdo::descriptor::PowerDescriptorConfig;
use zigbee::DeviceConfig;
use zigbee::LogicalType;
use zigbee::NetworkConfig;
use zigbee::PowerSource;
use zigbee::TimingConfig;
use zigbee::CurrentPowerMode;
use zigbee::CurrentPowerSourceLevel;
use zigbee_cluster_library::clusters::general::basic;
use zigbee_cluster_library::clusters::general::identify;
use zigbee_cluster_library::clusters::general::identify::IdentifyServer;
use zigbee_cluster_library::frame::Status;
use zigbee_cluster_library::profile;
use zigbee_cluster_library::reporting::AttributeReportBuilder;
use zigbee_cluster_library::reporting::AttributeReporting;
use zigbee_cluster_library::reporting::ReportingTable;
use zigbee_cluster_library::server::ClusterCommand;
use zigbee_cluster_library::server::ClusterServer;
use zigbee_cluster_library::server::CommandOutcome;
use zigbee_cluster_library::server::UnsupportedClusterResponder;
use zigbee_cluster_library::sender::ZclReportTarget;
use zigbee_cluster_library::sender::ZclSender;
use zigbee_cluster_library::types::AttrInfo;
use zigbee_cluster_library::types::Attribute;
use zigbee_cluster_library::types::AttributeId;
use zigbee_cluster_library::types::Bool;
use zigbee_cluster_library::types::Cluster;
use zigbee_cluster_library::types::ClusterId;
use zigbee_cluster_library::types::ReadOnly;
use zigbee_cluster_library::types::ReadWrite;
use zigbee_cluster_library::types::Reportable;
use zigbee_cluster_library::types::TypeId;
use zigbee_cluster_library::types::Uint8;
use zigbee_cluster_library::types::Enum8;
use zigbee_cluster_library::types::ZclEnum8;
use zigbee_cluster_library::types::strings::ShortStr;
use zigbee_cluster_library::types::strings::ShortText;
use zigbee_types::IeeeAddress;

esp_bootloader_esp_idf::esp_app_desc!();

// ------------------------------------------------------------ CC1101 registers
const READ_BURST: u8 = 0xC0;
const WRITE_BURST: u8 = 0x40;

const IOCFG2: u8 = 0x00;
const IOCFG0: u8 = 0x02;
const FIFOTHR: u8 = 0x03;
const PKTCTRL1: u8 = 0x07;
const PKTLEN: u8 = 0x06;
const SFTX: u8 = 0x3B;
const SCAL: u8 = 0x33;
const PKTCTRL0: u8 = 0x08;
const FSCTRL1: u8 = 0x0B;
const FREQ2: u8 = 0x0D;
const FREQ1: u8 = 0x0E;
const FREQ0: u8 = 0x0F;
const MDMCFG4: u8 = 0x10;
const MDMCFG3: u8 = 0x11;
const MDMCFG2: u8 = 0x12;
const MDMCFG1: u8 = 0x13;
const MDMCFG0: u8 = 0x14;
const DEVIATN: u8 = 0x15;
const MCSM2: u8 = 0x16;
const MCSM1: u8 = 0x17;
const MCSM0: u8 = 0x18;
const FOCCFG: u8 = 0x19;
const BSCFG: u8 = 0x1A;
const AGCCTRL2: u8 = 0x1B;
const AGCCTRL1: u8 = 0x1C;
const AGCCTRL0: u8 = 0x1D;
const FREND1: u8 = 0x21;
const FREND0: u8 = 0x22;
const FSCAL3: u8 = 0x23;
const FSCAL2: u8 = 0x24;
const FSCAL1: u8 = 0x25;
const FSCAL0: u8 = 0x26;
const PATABLE: u8 = 0x3E;

const SRES: u8 = 0x30;
const SIDLE: u8 = 0x36;
const SRX: u8 = 0x34;
const STX: u8 = 0x35;

const FREQ_KHZ: u32 = 433_920;

/// Ignore edges shorter than this (demodulator noise).
const NOISE_US: u32 = 100;
/// Silence that ends a frame.
const FRAME_GAP_US: u32 = 6_000;
/// Symbol split / sanity bounds.
const SHORT_MIN: u32 = 250;
const SHORT_MAX: u32 = 650;
const LONG_MIN: u32 = 850;
const LONG_MAX: u32 = 1800;
const MAX_SEG: usize = 80;

/// Symbol widths used for transmit.
const T_SHORT: u32 = 400;
const T_LONG: u32 = 1200;
const FRAME_GAP_MS: u32 = 12;
const REPEATS: u32 = 8;

// ------------------------------------------------------------------ button ids
const BTN_SPEED1: u8 = 0x2;
const BTN_SPEED2: u8 = 0x1;
const BTN_SPEED3: u8 = 0xC;
const BTN_SPEED4: u8 = 0x7;
const BTN_DIR_IN: u8 = 0x8;
const BTN_DIR_OUT: u8 = 0x4;
const BTN_DIR_TOGGLE: u8 = 0x3;
const BTN_POWER: u8 = 0xA;

fn speed_button(speed: u8) -> Option<u8> {
    match speed {
        1 => Some(BTN_SPEED1),
        2 => Some(BTN_SPEED2),
        3 => Some(BTN_SPEED3),
        4 => Some(BTN_SPEED4),
        _ => None,
    }
}

/// The learn sequence: speed 1, 2, 3, 4.
const PAIR_SEQ: [u8; 4] = [BTN_SPEED1, BTN_SPEED2, BTN_SPEED3, BTN_SPEED4];
/// Time allowed between consecutive learn presses.
const PAIR_STEP_MS: u64 = 15_000;
/// Whole pairing session timeout.
const PAIR_SESSION_MS: u64 = 90_000;
/// How long the outcome of a pairing attempt stays visible in the stage attribute.
const PAIR_RESULT_MS: u64 = 10_000;

const CC1101_CONFIG: &[(u8, u8)] = &[
    (IOCFG2, 0x0D),
    (IOCFG0, 0x0D),
    (FIFOTHR, 0x47),
    (PKTCTRL1, 0x04),
    (PKTCTRL0, 0x32),
    (FSCTRL1, 0x06),
    (MDMCFG4, 0x87),
    (MDMCFG3, 0x83),
    (MDMCFG2, 0x30),
    (MDMCFG1, 0x22),
    (MDMCFG0, 0xF8),
    (DEVIATN, 0x15),
    (MCSM2, 0x07),
    (MCSM1, 0x30),
    (MCSM0, 0x18),
    (FOCCFG, 0x16),
    (BSCFG, 0x6C),
    (AGCCTRL2, 0x03),
    (AGCCTRL1, 0x00),  // DN022: OOK wants 0x00, not the FSK default 0x40
    (AGCCTRL0, 0x91),
    (FREND1, 0xB6),    // DN022: RX filter > 101 kHz (ours is 203 kHz)
    (FREND0, 0x11),
    (FSCAL3, 0xE9),
    (FSCAL2, 0x2A),
    (FSCAL1, 0x00),
    (FSCAL0, 0x1F),
];

// ------------------------------------------------------------------- storage
const CFG_ADDR: u32 = 0x003E_0000;
const CFG_SECTOR: u32 = CFG_ADDR / 0x1000;
const CFG_MAGIC: u32 = 0x4641_4E31; // "FAN1"
/// Bumped whenever the layout below changes; older blobs are migrated.
const CFG_VERSION: u32 = 2;
/// Longest name (device or fan) accepted by the console.
pub const NAME_MAX: usize = 24;
/// Bytes written per save; the sector is 4 KiB, only the used part is written.
const CFG_BYTES: usize = 512;

/// A user-facing name: UTF-8, at most [`NAME_MAX`] bytes, zero-padded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    buf: [u8; NAME_MAX],
    len: u8,
}

impl Name {
    pub const EMPTY: Name = Name {
        buf: [0; NAME_MAX],
        len: 0,
    };

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns false (and leaves the name untouched) if it does not fit.
    pub fn set(&mut self, s: &str) -> bool {
        let bytes = s.as_bytes();
        if bytes.len() > NAME_MAX || core::str::from_utf8(bytes).is_err() {
            return false;
        }
        self.buf[..bytes.len()].copy_from_slice(bytes);
        self.buf[bytes.len()..].fill(0);
        self.len = bytes.len() as u8;
        true
    }

    fn write(&self, out: &mut [u8]) {
        out[..NAME_MAX].copy_from_slice(&self.buf);
    }

    fn read(input: &[u8]) -> Name {
        let mut name = Name::EMPTY;
        let len = input.iter().position(|&b| b == 0).unwrap_or(NAME_MAX);
        if core::str::from_utf8(&input[..len]).is_ok() {
            name.buf[..len].copy_from_slice(&input[..len]);
            name.len = len as u8;
        }
        name
    }
}

impl Default for Name {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[derive(Clone, Copy, Default)]
pub struct Slot {
    /// 20-bit device address; 0 means empty
    pub addr: u32,
    /// 0 = unknown/off, 1..=4
    pub speed: u8,
    /// 0 = in, 1 = out
    pub dir: u8,
    pub power: bool,
    /// user-facing name, empty when unset
    pub name: Name,
}

impl Slot {
    pub fn used(&self) -> bool {
        self.addr != 0
    }
}

/// On-flash layout (version 2, little-endian):
///
/// ```text
///   0..4    magic
///   4..8    version
///   8..32   device name (zero padded)
///   32..    slots, 32 bytes each: addr u32, speed u8, dir u8, power u8, pad u8,
///           name (24 bytes)
///   end..   checksum u32
/// ```
const DEVICE_NAME_OFF: usize = 8;
const SLOTS_OFF: usize = DEVICE_NAME_OFF + NAME_MAX;
const SLOT_SIZE: usize = 32;
const CHECKSUM_OFF: usize = SLOTS_OFF + 8 * SLOT_SIZE;

#[derive(Clone, Copy)]
pub struct Config {
    pub magic: u32,
    pub version: u32,
    pub device_name: Name,
    pub slots: [Slot; 8],
    pub checksum: u32,
}

impl Config {
    pub const EMPTY: Config = Config {
        magic: CFG_MAGIC,
        version: CFG_VERSION,
        device_name: Name::EMPTY,
        slots: [Slot {
            addr: 0,
            speed: 0,
            dir: 0,
            power: false,
            name: Name::EMPTY,
        }; 8],
        checksum: 0,
    };

    fn bytes(&self) -> [u8; CFG_BYTES] {
        let mut b = [0u8; CFG_BYTES];
        b[0..4].copy_from_slice(&self.magic.to_le_bytes());
        b[4..8].copy_from_slice(&self.version.to_le_bytes());
        self.device_name.write(&mut b[DEVICE_NAME_OFF..][..NAME_MAX]);
        for (i, s) in self.slots.iter().enumerate() {
            let o = SLOTS_OFF + i * SLOT_SIZE;
            b[o..o + 4].copy_from_slice(&s.addr.to_le_bytes());
            b[o + 4] = s.speed;
            b[o + 5] = s.dir;
            b[o + 6] = u8::from(s.power);
            b[o + 7] = 0;
            s.name.write(&mut b[o + 8..][..NAME_MAX]);
        }
        b[CHECKSUM_OFF..CHECKSUM_OFF + 4].copy_from_slice(&self.checksum.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; CFG_BYTES]) -> Option<Config> {
        let magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if magic != CFG_MAGIC {
            return None;
        }
        let version = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);

        // version 1 kept the slots right after the header (8 bytes each, no
        // names) and its checksum at 72; version 2 moved both.
        let (slots_off, slot_size, checksum_off) = if version >= 2 {
            (SLOTS_OFF, SLOT_SIZE, CHECKSUM_OFF)
        } else {
            (8, 8, 72)
        };

        let mut c = Config {
            magic,
            version,
            device_name: Name::EMPTY,
            slots: [Slot::default(); 8],
            checksum: 0,
        };
        if version >= 2 {
            c.device_name = Name::read(&b[DEVICE_NAME_OFF..][..NAME_MAX]);
        }
        for (i, s) in c.slots.iter_mut().enumerate() {
            let o = slots_off + i * slot_size;
            s.addr = u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) & 0x000F_FFFF;
            s.speed = b[o + 4];
            s.dir = b[o + 5];
            s.power = b[o + 6] != 0;
            if version >= 2 {
                s.name = Name::read(&b[o + 8..][..NAME_MAX]);
            }
        }

        let stored = u32::from_le_bytes([
            b[checksum_off],
            b[checksum_off + 1],
            b[checksum_off + 2],
            b[checksum_off + 3],
        ]);
        if c.compute_checksum(version) != stored {
            return None;
        }
        c.checksum = stored;
        c.version = CFG_VERSION;
        Some(c)
    }

    /// The checksum covers everything before it; a version-1 blob has a
    /// different layout, so it is checksummed the way it was written.
    fn compute_checksum(&self, version: u32) -> u32 {
        if version < 2 {
            // rebuild the version-1 image: header, 8-byte slots, then the sum
            let mut b = [0u8; 128];
            b[0..4].copy_from_slice(&self.magic.to_le_bytes());
            b[4..8].copy_from_slice(&version.to_le_bytes());
            for (i, s) in self.slots.iter().enumerate() {
                let o = 8 + i * 8;
                b[o..o + 4].copy_from_slice(&s.addr.to_le_bytes());
                b[o + 4] = s.speed;
                b[o + 5] = s.dir;
                b[o + 6] = u8::from(s.power);
            }
            return checksum(&b[..72]);
        }
        let b = self.bytes();
        checksum(&b[..CHECKSUM_OFF])
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for (i, v) in bytes.iter().enumerate() {
        sum = sum.wrapping_mul(31).wrapping_add(u32::from(*v) + i as u32);
    }
    sum ^ 0xA5A5_5A5A
}

/// Read the config sector. Returns the empty config if nothing valid is stored.
fn load_config() -> Config {
    let mut buf = [0u32; CFG_BYTES / 4];
    let r = unsafe {
        esp_storage::ll::spiflash_read(
            CFG_ADDR,
            buf.as_mut_ptr(),
            core::mem::size_of_val(&buf) as u32,
        )
    };
    if r.is_err() {
        return Config::EMPTY;
    }
    let bytes: &[u8; CFG_BYTES] = unsafe { &*(buf.as_ptr() as *const [u8; CFG_BYTES]) };
    Config::from_bytes(bytes).unwrap_or(Config::EMPTY)
}

/// Erase the sector and write the config back.
fn save_config(cfg: &Config) -> bool {
    let mut c = *cfg;
    c.magic = CFG_MAGIC;
    c.version = CFG_VERSION;
    c.checksum = c.compute_checksum(CFG_VERSION);
    let mut buf = [0u32; CFG_BYTES / 4];
    {
        let bytes: &mut [u8; CFG_BYTES] = unsafe { &mut *(buf.as_mut_ptr() as *mut [u8; CFG_BYTES]) };
        *bytes = c.bytes();
    }
    critical_section::with(|_| unsafe {
        let _ = esp_storage::ll::spiflash_unlock();
        if esp_storage::ll::spiflash_erase_sector(CFG_SECTOR).is_err() {
            return false;
        }
        esp_storage::ll::spiflash_write(
            CFG_ADDR,
            buf.as_ptr(),
            core::mem::size_of_val(&buf) as u32,
        )
        .is_ok()
    })
}

// -------------------------------------------------------------- shared state
pub struct State {
    pub cfg: Config,
    pub pairing: bool,
    pub pair_addr: u32,
    pub pair_step: usize,
    pub pair_last_ms: u64,
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
        self.cfg.slots.iter().filter(|s| s.used()).count() as u8
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

static STATE: CsMutex<CriticalSectionRawMutex, RefCell<State>> =
    CsMutex::new(RefCell::new(State {
        cfg: Config::EMPTY,
        pairing: false,
        pair_addr: 0,
        pair_step: 0,
        pair_last_ms: 0,
        stage: Stage::Idle,
        stage_until_ms: 0,
    }));

/// Pending transmit request: 24-bit code, 0 = none.
static TX_REQ: AtomicU32 = AtomicU32::new(0);
/// Debug requests handled by the main loop (which owns the SPI + segments).
static DUMP_REQ: AtomicBool = AtomicBool::new(false);
static STAT_REQ: AtomicBool = AtomicBool::new(false);
static CLOCK_REQ: AtomicBool = AtomicBool::new(false);
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

static ZB_EVENTS: Channel<CriticalSectionRawMutex, ZbEvent, 16> = Channel::new();

fn zb_event(kind: u8, slot: u8, value: u8) {
    let _ = ZB_EVENTS.try_send(ZbEvent { kind, slot, value });
}

/// Pending state reports, one bit per (cluster, endpoint):
/// bits 0..7  = FanMode of slot N
/// bits 8..15 = direction of slot N
/// bit 16     = pairing switch (OnOff on the master endpoint)
static REPORT_DIRTY: AtomicU32 = AtomicU32::new(0);
const REPORT_PAIRING_BIT: u32 = 1 << 16;
/// The whole state, e.g. after joining so the coordinator starts in sync.
const REPORT_ALL: u32 = 0x0001_FFFF;

fn mark_fan_mode(slot: usize) {
    REPORT_DIRTY.fetch_or(1 << slot, Ordering::Relaxed);
}

fn mark_direction(slot: usize) {
    REPORT_DIRTY.fetch_or(1 << (8 + slot), Ordering::Relaxed);
}

fn mark_pairing_switch() {
    REPORT_DIRTY.fetch_or(REPORT_PAIRING_BIT, Ordering::Relaxed);
}

fn mark_all_state() {
    REPORT_DIRTY.fetch_or(REPORT_ALL, Ordering::Relaxed);
}

/// Set when the pairing status needs to be reported to the coordinator.
static PAIRING_REPORT: AtomicBool = AtomicBool::new(false);
/// Paired count last reported, to notice a change.
static PAIRED_NOW: AtomicU32 = AtomicU32::new(0);
/// Exposed (endpoint-bearing) count last reported.
static EXPOSED_NOW: AtomicU32 = AtomicU32::new(0);
/// Counters for the `status` command.
static TX_COUNT: AtomicU32 = AtomicU32::new(0);
static RX_COUNT: AtomicU32 = AtomicU32::new(0);
static REARM_COUNT: AtomicU32 = AtomicU32::new(0);
static UPTIME_MS: AtomicU32 = AtomicU32::new(0);

/// `true` = packet-mode transmit (radio sends the bitstream), `false` = the
/// per-edge bit-bang path. Off by default until the expanded stream has been
/// confirmed against the physical fans; `txmode packet` switches at runtime.
static TX_PACKET: AtomicBool = AtomicBool::new(false);
/// Experiment hooks: modulation format / PA index / PATABLE used for packet TX.
static PKT_MDMCFG2: AtomicU32 = AtomicU32::new(0x30);
static PKT_FREND0: AtomicU32 = AtomicU32::new(0x11);
static PKT_PATABLE: AtomicU32 = AtomicU32::new(0x00C0); // low byte = index 1
/// Frames sent per command and the silence between them. The remotes send only
/// a few frames but with a long gap, which a picky receiver may need.
static TX_REPEATS: AtomicU32 = AtomicU32::new(REPEATS);
static TX_GAP_MS: AtomicU32 = AtomicU32::new(FRAME_GAP_MS);
/// Set to `0x100 | byte` to transmit one FIFO filled with that byte.
static PKT_FILL: AtomicU32 = AtomicU32::new(0);
/// opt-in noisy diagnostics
static DEBUG_ON: AtomicBool = AtomicBool::new(false);
/// counts every raw level change on GDO0, including sub-noise glitches
static RAW_EDGES: AtomicU32 = AtomicU32::new(0);

// --------------------------------------------------------------- CC1101 SPI
fn wreg(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, addr: u8, val: u8) {
    cs.set_low();
    let mut b = [addr & 0x3F, val];
    let _ = spi.transfer(&mut b);
    cs.set_high();
}

fn rstat(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, addr: u8) -> u8 {
    cs.set_low();
    let mut b = [READ_BURST | (addr & 0x3F), 0x00];
    let _ = spi.transfer(&mut b);
    cs.set_high();
    b[1]
}

fn rreg(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, addr: u8) -> u8 {
    cs.set_low();
    let mut b = [0x80 | (addr & 0x3F), 0x00];
    let _ = spi.transfer(&mut b);
    cs.set_high();
    b[1]
}

fn strobe(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, cmd: u8) {
    cs.set_low();
    let mut b = [cmd, 0x00];
    let _ = spi.transfer(&mut b);
    cs.set_high();
}

fn set_freq_khz(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, khz: u32) {
    let f = ((khz as u64) * 65536) / 26_000;
    wreg(spi, cs, FREQ2, (f >> 16) as u8);
    wreg(spi, cs, FREQ1, (f >> 8) as u8);
    wreg(spi, cs, FREQ0, f as u8);
}

fn write_patable(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, p: [u8; 2]) {
    cs.set_low();
    let mut b = [WRITE_BURST | (PATABLE & 0x3F), p[0], p[1]];
    let _ = spi.transfer(&mut b);
    cs.set_high();
}

fn cc1101_init(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, d: &mut Delay) -> u8 {
    strobe(spi, cs, SRES);
    d.delay_millis(10);
    let version = rstat(spi, cs, 0x31);
    for &(a, v) in CC1101_CONFIG {
        wreg(spi, cs, a, v);
    }
    strobe(spi, cs, SIDLE);
    set_freq_khz(spi, cs, FREQ_KHZ);
    version
}

fn enter_rx(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>) {
    strobe(spi, cs, SIDLE);
    strobe(spi, cs, SRX);
}

async fn send_code(
    spi: &mut Spi<'_, esp_hal::Blocking>,
    cs: &mut Output<'_>,
    gdo0: &mut Flex<'_>,
    code: u32,
) {
    // GDO0 becomes the TX data input in async mode
    gdo0.set_low();
    gdo0.set_input_enable(false);
    gdo0.set_output_enable(true);

    strobe(spi, cs, SIDLE);
    write_patable(spi, cs, [0x00, 0xC0]);
    strobe(spi, cs, STX);
    Timer::after_millis(2).await;

    // The symbol timing is generous (the decoder accepts 250..650 us for a short
    // and 850..1800 us for a long symbol), so yielding between edges is free
    // accuracy-wise. It matters: the whole frame takes ~400 ms, and busy-waiting
    // through it starved the Zigbee task, which then timed out its transmissions
    // (`MLME tx-done timeout`) after every command that also drove the fan.
    for _ in 0..TX_REPEATS.load(Ordering::Relaxed) {
        for i in (0..24).rev() {
            if (code >> i) & 1 == 1 {
                gdo0.set_high();
                Timer::after_micros(u64::from(T_LONG)).await;
                gdo0.set_low();
                Timer::after_micros(u64::from(T_SHORT)).await;
            } else {
                gdo0.set_high();
                Timer::after_micros(u64::from(T_SHORT)).await;
                gdo0.set_low();
                Timer::after_micros(u64::from(T_LONG)).await;
            }
        }
        gdo0.set_high();
        Timer::after_micros(u64::from(T_SHORT)).await;
        gdo0.set_low();
        Timer::after_millis(u64::from(TX_GAP_MS.load(Ordering::Relaxed))).await;
    }

    // back to receive
    gdo0.set_output_enable(false);
    gdo0.set_input_enable(true);
    enter_rx(spi, cs);
}

// ------------------------------------------------------- packet-mode transmit
//
// The remote protocol is pulse-width coded: a `0` is 400 us high + 1200 us low,
// a `1` is 1200 us high + 400 us low. The CC1101 can not emit that itself, but
// it *can* emit an arbitrary NRZ bitstream from its TX FIFO. Sampling the PWM
// waveform on a 100 us grid turns every symbol into 16 plain bits (4 on + 12
// off, or 12 on + 4 off), so the whole frame becomes 49 ordinary bytes that the
// radio clocks out on its own. See docs/PACKET-TX.md.

/// Bit period of the expanded stream: 100 us, so 400/1200 us pulses are exactly
/// 4/12 bits and the on-air timing matches the bit-banged version.
const TX_BIT_US: u32 = 100;
/// 24 symbols * 16 bits + the 4-bit trailing pulse, padded to whole bytes.
const TX_PACKET_LEN: u8 = 49;
/// Data rate for 100 us per bit (26 MHz crystal): 403 * 256 * 26e6 / 2^28.
const TX_DRATE_E: u8 = 8;
const TX_DRATE_M: u8 = 147;

/// Registers for packet transmission: OOK, no preamble/sync, fixed length,
/// no CRC, no whitening.
const PKT_TX_REGS: &[(u8, u8)] = &[
    (PKTCTRL1, 0x00),
    (PKTCTRL0, 0x00),
    (PKTLEN, TX_PACKET_LEN),
    (MDMCFG4, 0x80 | TX_DRATE_E),
    (MDMCFG3, TX_DRATE_M),
    // MDMCFG2: MOD_FORMAT[6:4] = 011 (ASK/OOK), MANCHESTER_EN[3] = 0,
    // SYNC_MODE[2:0] = 000 (no preamble/sync). 0x02 here would mean 2-FSK.
    (MDMCFG2, 0x30),
];

/// Registers to put the radio back into asynchronous serial mode for RX.
const ASYNC_RX_REGS: &[(u8, u8)] = &[
    (PKTCTRL1, 0x04),
    (PKTCTRL0, 0x32),
    (PKTLEN, 0xFF),
    (MDMCFG4, 0x87),
    (MDMCFG3, 0x83),
    (MDMCFG2, 0x30),
];

/// Set one bit of a big-endian bit buffer.
fn put_bit(out: &mut [u8], pos: &mut usize, one: bool) {
    if one {
        out[*pos / 8] |= 1 << (7 - *pos % 8);
    }
    *pos += 1;
}

/// Expand a 24-bit remote code into the NRZ bitstream the radio can send.
fn expand_code(code: u32) -> [u8; TX_PACKET_LEN as usize] {
    let mut out = [0u8; TX_PACKET_LEN as usize];
    let mut pos = 0usize;
    for i in (0..24).rev() {
        // 0 -> 400 us high + 1200 us low = 4 on + 12 off
        // 1 -> 1200 us high + 400 us low = 12 on + 4 off
        let (high, low) = if (code >> i) & 1 == 1 { (12, 4) } else { (4, 12) };
        for _ in 0..high {
            put_bit(&mut out, &mut pos, true);
        }
        for _ in 0..low {
            put_bit(&mut out, &mut pos, false);
        }
    }
    // the remote ends a frame with one short pulse
    for _ in 0..4 {
        put_bit(&mut out, &mut pos, true);
    }
    out
}

fn apply_regs(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, regs: &[(u8, u8)]) {
    for &(a, v) in regs {
        wreg(spi, cs, a, v);
    }
}

/// Transmit `code` eight times using the radio's own packet engine.
///
/// Returns how long the whole burst took, for the debug log.
async fn send_code_packet(
    spi: &mut Spi<'_, esp_hal::Blocking>,
    cs: &mut Output<'_>,
    code: u32,
) -> u32 {
    let frame = expand_code(code);
    // +1: the first STX after the async→packet switch does not go out
    send_packet_frame(spi, cs, &frame, REPEATS + 1).await
}

/// Enter packet mode, send `frame` `repeats` times, restore async RX.
async fn send_packet_frame(
    spi: &mut Spi<'_, esp_hal::Blocking>,
    cs: &mut Output<'_>,
    frame: &[u8],
    repeats: u32,
) -> u32 {
    strobe(spi, cs, SIDLE);
    apply_regs(spi, cs, PKT_TX_REGS);
    wreg(spi, cs, IOCFG0, 0x06); // GDO0 = "packet transmitted" (unused, kept sane)
    write_patable(spi, cs, [0x00, 0xC0]);
    // Recalibrate after changing the modem configuration; the first STX after
    // the switch is otherwise dropped, so one extra frame is sent below.
    strobe(spi, cs, SCAL);
    Timer::after_millis(1).await;


    let start = Instant::now();
    for _ in 0..repeats {
        strobe(spi, cs, SFTX);
        cs.set_low();
        let header = [WRITE_BURST | 0x3F];
        let _ = spi.write(&header);
        let _ = spi.write(frame);
        cs.set_high();
        strobe(spi, cs, STX);
        Timer::after_millis(1).await;

        // 49 bytes at ~10 kbaud is ~39 ms; wait it out, then make sure the
        // state machine really returned to idle before touching the FIFO again
        Timer::after_millis(40).await;
        let poll = Instant::now();
        while rstat(spi, cs, 0x35) & 0x1F != 0x01 && poll.elapsed().as_millis() < 50 {
            Timer::after_millis(1).await;
        }
        Timer::after_millis(u64::from(TX_GAP_MS.load(Ordering::Relaxed))).await;
    }
    let elapsed = start.elapsed().as_millis() as u32;

    strobe(spi, cs, SIDLE);
    apply_regs(spi, cs, ASYNC_RX_REGS);
    wreg(spi, cs, IOCFG0, 0x0D);
    enter_rx(spi, cs);
    elapsed
}

// ------------------------------------------------------------------ decoding
#[derive(Clone, Copy)]
struct Seg {
    dt: u32,
    high: bool,
}

fn sym(dt: u32) -> Option<bool> {
    if (SHORT_MIN..=SHORT_MAX).contains(&dt) {
        Some(false)
    } else if (LONG_MIN..=LONG_MAX).contains(&dt) {
        Some(true)
    } else {
        None
    }
}

fn decode_frame(segs: &[Seg]) -> Option<u32> {
    for start in 0..segs.len() {
        if !segs[start].high || start + 48 > segs.len() {
            continue;
        }
        let mut bits: u32 = 0;
        let mut ok = true;
        for i in 0..24 {
            let h = segs[start + 2 * i];
            let l = segs[start + 2 * i + 1];
            if !h.high || l.high {
                ok = false;
                break;
            }
            let (hs, ls) = match (sym(h.dt), sym(l.dt)) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    ok = false;
                    break;
                }
            };
            if hs == ls {
                ok = false;
                break;
            }
            bits = (bits << 1) | u32::from(hs);
        }
        if ok {
            let total: u32 = segs[start..start + 48].iter().map(|s| s.dt).sum();
            if (33_000..=45_000).contains(&total) {
                return Some(bits);
            }
        }
    }
    None
}

// ------------------------------------------------------------------- helpers
fn apply_button(slot: &mut Slot, button: u8) {
    match button {
        BTN_SPEED1 | BTN_SPEED2 | BTN_SPEED3 | BTN_SPEED4 => {
            slot.speed = match button {
                BTN_SPEED1 => 1,
                BTN_SPEED2 => 2,
                BTN_SPEED3 => 3,
                _ => 4,
            };
            slot.power = true;
        }
        BTN_DIR_IN => slot.dir = 0,
        BTN_DIR_OUT => slot.dir = 1,
        // the third direction button puts the fan in automatic mode, where it
        // alternates between the two directions on its own
        BTN_DIR_TOGGLE => slot.dir = 2,
        BTN_POWER => slot.power = !slot.power,
        _ => {}
    }
}

/// Button that selects a direction (0 in, 1 out, 2 auto).
fn dir_button(dir: u8) -> Option<u8> {
    match dir {
        0 => Some(BTN_DIR_IN),
        1 => Some(BTN_DIR_OUT),
        2 => Some(BTN_DIR_TOGGLE),
        _ => None,
    }
}

fn dir_name(dir: u8) -> &'static str {
    match dir {
        0 => "in",
        1 => "out",
        _ => "auto",
    }
}

/// Code we would send to make `slot` match its desired state.
fn code_for(slot: &Slot, button: u8) -> u32 {
    (slot.addr << 4) | u32::from(button)
}

// -------------------------------------------------------------------- console
struct Console<'d> {
    usb: UsbSerialJtag<'d, esp_hal::Blocking>,
    line: [u8; 64],
    n: usize,
}

impl<'d> Console<'d> {
    fn new(usb: UsbSerialJtag<'d, esp_hal::Blocking>) -> Self {
        Self {
            usb,
            line: [0; 64],
            n: 0,
        }
    }

    fn out(&mut self, s: &str) {
        let _ = self.usb.write(s.as_bytes());
    }

    fn outln(&mut self, s: &str) {
        self.out(s);
        self.out("\r\n");
    }

    fn poll(&mut self) -> Option<[u8; 64]> {
        loop {
            match self.usb.read_byte() {
                Ok(b) => {
                    if b == b'\r' || b == b'\n' {
                        if self.n == 0 {
                            continue;
                        }
                        let mut l = [0u8; 64];
                        l[..self.n].copy_from_slice(&self.line[..self.n]);
                        let n = self.n;
                        self.n = 0;
                        self.out("\r\n");
                        let mut out = [0u8; 64];
                        out[..n].copy_from_slice(&l[..n]);
                        // null-terminate marker by returning length via first 0? keep simple:
                        return Some(out);
                    } else if b == 8 || b == 127 {
                        if self.n > 0 {
                            self.n -= 1;
                            self.out("\x08 \x08");
                        }
                    } else if self.n < 63 && b >= 32 && b < 127 {
                        self.line[self.n] = b;
                        self.n += 1;
                        let s = [b];
                        let _ = self.usb.write(&s);
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

fn cmd_str(line: &[u8; 64]) -> &str {
    let end = line.iter().position(|&c| c == 0).unwrap_or(64);
    core::str::from_utf8(&line[..end]).unwrap_or("")
}

/// Copy a name out of the shared state (a `&str` can not escape the lock).
fn copy_name(name: Name) -> [u8; NAME_MAX] {
    let mut buf = [0u8; NAME_MAX];
    let bytes = name.as_str().as_bytes();
    buf[..bytes.len()].copy_from_slice(bytes);
    buf
}

fn fmt_name<'a>(b: &'a mut [u8; 96], text: &[u8; NAME_MAX], slot: Option<usize>) -> &'a str {
    let text = core::str::from_utf8(&text[..text.iter().position(|&c| c == 0).unwrap_or(NAME_MAX)]).unwrap_or("");
    let pos = {
        let mut w = Writer96 { buf: b, pos: 0 };
        match slot {
            Some(i) => {
                let _ = core::fmt::Write::write_fmt(&mut w, format_args!("fan {i} name: \"{text}\""));
            }
            None => {
                let _ = core::fmt::Write::write_fmt(&mut w, format_args!("device name: \"{text}\""));
            }
        }
        w.pos
    };
    core::str::from_utf8(&b[..pos]).unwrap_or("")
}

fn print_list(c: &mut Console<'_>) {
    STATE.lock(|s| {
        let st = s.borrow();
        c.outln("slot  address    speed  dir    power  name");
        for i in 0..8 {
            let sl = &st.cfg.slots[i];
            if sl.used() {
                let mut b = [0u8; 96];
                let txt = format_slot(&mut b, i, sl);
                c.outln(txt);
            }
        }
        c.outln("(empty slots are not listed)");
    });
}

struct Writer<'a> {
    buf: &'a mut [u8; 64],
    pos: usize,
}

impl core::fmt::Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &c in s.as_bytes() {
            if self.pos < 64 {
                self.buf[self.pos] = c;
                self.pos += 1;
            }
        }
        Ok(())
    }
}

fn format_slot<'a>(b: &'a mut [u8; 96], i: usize, sl: &Slot) -> &'a str {
    use core::fmt::Write;
    let mut w = Writer96 { buf: b, pos: 0 };
    let _ = write!(
        w,
        "  {}   0x{:05X}    {}     {}     {}    {}",
        i,
        sl.addr,
        sl.speed,
        dir_name(sl.dir),
        if sl.power { "on" } else { "off" },
        sl.name.as_str()
    );
    let n = w.pos;
    core::str::from_utf8(&b[..n]).unwrap_or("")
}

fn handle_command(c: &mut Console<'_>, line: &str) {
    let mut it = line.split_whitespace();
    let cmd = it.next().unwrap_or("");
    match cmd {
        "help" | "?" => {
            c.outln("commands:");
            c.outln("  list                 show paired remotes");
            c.outln("  pair                 learn a new remote (press speed 1,2,3,4 on it)");
            c.outln("  cancel               abort pairing");
            c.outln("  del <slot>           forget a remote");
            c.outln("  speed <slot> <1-4>   send a speed command");
            c.outln("  dir <slot> <in|out|auto>  send a direction command");
            c.outln("  power <slot> <on|off>");
            c.outln("  raw <hex>            transmit a raw 24-bit code");
            c.outln("  status               uptime, counters, mode, frequency");
            c.outln("  stat                 chip state + RSSI");
            c.outln("  debug [on|off]       noisy diagnostics (no argument: show state)");
            c.outln("  desc                 show the advertised endpoint descriptors");
            c.outln("  dump                 show the last captured edges");
            c.outln("  clock                toggle GDO0 = XOSC/192 clock (pin self-test)");
            c.outln("  save                 force save config");
            c.outln("  name [text]          show or set the device name");
            c.outln("  name <slot> [text]   show or set a fan name");
            c.outln("  txmode [packet|async] show or switch how frames are transmitted");
            c.outln("  repeats [n]          frames per command (and show gap)");
            c.outln("  slots                raw dump of all eight slots");
            c.outln("  reboot               restart (needed after pair/del: the endpoint set is fixed at boot)");
        }
        "list" => print_list(c),
        "pair" => {
            STATE.lock(|s| {
                let mut st = s.borrow_mut();
                st.pairing = true;
                st.pair_step = 0;
                st.pair_addr = 0;
                st.pair_last_ms = 0;
                st.stage = Stage::Idle;
                st.stage_until_ms = 0;
            });
            mark_pairing_switch();
            c.outln("pairing: press SPEED 1, then 2, then 3, then 4 on the remote");
        }
        "cancel" => {
            STATE.lock(|s| {
                let mut st = s.borrow_mut();
                st.pairing = false;
                st.stage = Stage::Idle;
                st.stage_until_ms = 0;
            });
            mark_pairing_switch();
            c.outln("pairing cancelled");
        }
        "del" => {
            if let Some(a) = it.next().and_then(|v| v.parse::<usize>().ok()) {
                if a < 8 {
                    let ok = STATE.lock(|s| {
                        let mut st = s.borrow_mut();
                        st.cfg.slots[a] = Slot::default();
                        save_config(&st.cfg)
                    });
                    c.outln(if ok { "deleted" } else { "deleted (save failed)" });
                    c.outln("reboot (or re-interview in Zigbee2MQTT) to drop the endpoint");
                }
            }
        }
        "speed" => {
            let a = it.next().and_then(|v| v.parse::<usize>().ok());
            let sp = it.next().and_then(|v| v.parse::<u8>().ok());
            if let (Some(a), Some(sp)) = (a, sp) {
                if let Some(btn) = speed_button(sp) {
                    // keep the stored state in step with what the fan will do,
                    // exactly like the Zigbee write does
                    let code = STATE.lock(|s| {
                        let mut st = s.borrow_mut();
                        let slot = &mut st.cfg.slots[a];
                        if !slot.used() {
                            return 0;
                        }
                        slot.speed = sp;
                        slot.power = true;
                        let snap = st.cfg;
                        drop(st);
                        save_config(&snap);
                        code_for(&snap.slots[a], btn)
                    });
                    if code != 0 {
                        TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
                        mark_fan_mode(a);
                        c.outln("sent");
                    } else {
                        c.outln("slot is empty");
                    }
                } else {
                    c.outln("speed must be 1..4");
                }
            } else {
                c.outln("usage: speed <slot> <1-4>");
            }
        }
        "dir" => {
            let a = it.next().and_then(|v| v.parse::<usize>().ok());
            let dv = it.next().unwrap_or("");
            if let Some(a) = a {
                let dir = match dv {
                    "in" => Some(0),
                    "out" => Some(1),
                    "auto" => Some(2),
                    _ => None,
                };
                if let (true, Some(dir)) = (a < 8, dir) {
                    let code = STATE.lock(|s| {
                        let mut st = s.borrow_mut();
                        let slot = &mut st.cfg.slots[a];
                        if !slot.used() {
                            return 0;
                        }
                        slot.dir = dir;
                        let snap = st.cfg;
                        drop(st);
                        save_config(&snap);
                        code_for(&snap.slots[a], dir_button(dir).unwrap())
                    });
                    if code != 0 {
                        TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
                        mark_direction(a);
                        c.outln("sent");
                    } else {
                        c.outln("slot is empty");
                    }
                } else {
                    c.outln("usage: dir <slot> <in|out|auto>");
                }
            }
        }
        "power" => {
            let a = it.next().and_then(|v| v.parse::<usize>().ok());
            let pv = it.next().unwrap_or("");
            if let Some(a) = a {
                if a < 8 && (pv == "on" || pv == "off") {
                    let wanted = pv == "on";
                    let code = STATE.lock(|s| {
                        let mut st = s.borrow_mut();
                        let slot = &mut st.cfg.slots[a];
                        if !slot.used() || slot.power == wanted {
                            return 0;
                        }
                        slot.power = wanted;
                        let snap = st.cfg;
                        drop(st);
                        save_config(&snap);
                        // turning on is done with a speed press, as the remote does
                        let btn = if wanted {
                            speed_button(if snap.slots[a].speed == 0 { 1 } else { snap.slots[a].speed }).unwrap_or(BTN_SPEED1)
                        } else {
                            BTN_POWER
                        };
                        code_for(&snap.slots[a], btn)
                    });
                    if code != 0 {
                        TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
                        mark_fan_mode(a);
                        c.outln("sent");
                    } else {
                        c.outln("slot is empty or already in that state");
                    }
                }
            }
        }
        "raw" => {
            if let Some(h) = it.next() {
                let h = h.trim_start_matches("0x");
                if let Ok(v) = u32::from_str_radix(h, 16) {
                    TX_REQ.store((v & 0x00FF_FFFF) | 0x8000_0000, Ordering::Relaxed);
                    c.outln("sent raw");
                }
            }
        }
        "dump" => {
            DUMP_REQ.store(true, Ordering::Relaxed);
        }
        "desc" => {
            for ep in advertised() {
                let mut b = [0u8; 80];
                {
                    use core::fmt::Write;
                    let mut w = Writer80 { buf: &mut b, pos: 0 };
                    let _ = write!(w, "ep {} device_id=0x{:04X} in=", ep.endpoint, ep.device_id);
                    for c in ep.input_clusters {
                        let _ = write!(w, "0x{:04X} ", c);
                    }
                }
                let t = core::str::from_utf8(&b).unwrap_or("");
                c.outln(t);
            }
        }
        "status" => {
            let mut b = [0u8; 96];
            let pos = {
                let mut w = Writer96 { buf: &mut b, pos: 0 };
                let _ = core::fmt::Write::write_fmt(
                    &mut w,
                    format_args!(
                        "up {}s | tx {} | rx {} | rearm {} | {} | {} kHz | debug {} | pairing {}",
                        UPTIME_MS.load(Ordering::Relaxed) / 1000,
                        TX_COUNT.load(Ordering::Relaxed),
                        RX_COUNT.load(Ordering::Relaxed),
                        REARM_COUNT.load(Ordering::Relaxed),
                        if TX_PACKET.load(Ordering::Relaxed) { "packet" } else { "async" },
                        FREQ_KHZ,
                        if DEBUG_ON.load(Ordering::Relaxed) { "on" } else { "off" },
                        if STATE.lock(|s| s.borrow().pairing) { "on" } else { "off" },
                    ),
                );
                w.pos
            };
            c.outln(core::str::from_utf8(&b[..pos]).unwrap_or(""));
        }
        "stat" => {
            STAT_REQ.store(true, Ordering::Relaxed);
        }
        "debug" => {
            match it.next() {
                Some("on") => {
                    DEBUG_ON.store(true, Ordering::Relaxed);
                    c.outln("debug on");
                }
                Some("off") => {
                    DEBUG_ON.store(false, Ordering::Relaxed);
                    c.outln("debug off");
                }
                None | Some("status") => c.outln(if DEBUG_ON.load(Ordering::Relaxed) {
                    "debug on"
                } else {
                    "debug off"
                }),
                Some(_) => c.outln("usage: debug [on|off]"),
            }
        }
        "clock" => {
            CLOCK_REQ.store(true, Ordering::Relaxed);
        }
        "save" => {
            let ok = STATE.lock(|s| save_config(&s.borrow().cfg));
            c.outln(if ok { "saved" } else { "save failed" });
        }
        "basic" => {
            // encode every Basic attribute the way a read response does
            for id in [0x0000u16, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x4000] {
                let mut buf = [0u8; 64];
                let mut off = 0usize;
                let st = BASIC_MASTER.encode_value(AttributeId(id), &mut buf, &mut off);
                let mut line = [0u8; 80];
                let pos = {
                    let mut w = Writer80 { buf: &mut line, pos: 0 };
                    let _ = core::fmt::Write::write_fmt(&mut w, format_args!("attr 0x{id:04X} status={st:?} len={off} data={:02X?}", &buf[..off]));
                    w.pos
                };
                let t = core::str::from_utf8(&line[..pos]).unwrap_or("");
                c.outln(t);
            }
        }
        "slots" => {
            // raw view: unlike `list` this also shows empty slots, which makes a
            // phantom entry (address left behind, count mismatch) visible
            let (count, raw) = STATE.lock(|s| {
                let st = s.borrow();
                let mut raw = [(0u32, 0u8, 0u8, false, Name::EMPTY); 8];
                for (i, sl) in st.cfg.slots.iter().enumerate() {
                    raw[i] = (sl.addr, sl.speed, sl.dir, sl.power, sl.name);
                }
                (st.paired_count(), raw)
            });
            for (i, (addr, speed, dir, power, name)) in raw.iter().enumerate() {
                let mut b = [0u8; 96];
                let pos = {
                    let mut w = Writer96 { buf: &mut b, pos: 0 };
                    let _ = core::fmt::Write::write_fmt(
                        &mut w,
                        format_args!(
                            "slot {i}: addr=0x{addr:05X} used={} speed={speed} dir={} power={} name=\"{}\"",
                            u8::from(*addr != 0),
                            dir_name(*dir),
                            u8::from(*power),
                            name.as_str()
                        ),
                    );
                    w.pos
                };
                let t = core::str::from_utf8(&b[..pos]).unwrap_or("");
                c.outln(t);
            }
            let mut b = [0u8; 64];
            let pos = {
                let mut w = Writer { buf: &mut b, pos: 0 };
                let _ = core::fmt::Write::write_fmt(&mut w, format_args!("paired_count={count}"));
                w.pos
            };
            c.outln(core::str::from_utf8(&b[..pos]).unwrap_or(""));
        }
        "repeats" => {
            match it.next().and_then(|v| v.parse::<u32>().ok()) {
                Some(n) if n > 0 && n <= 64 => {
                    TX_REPEATS.store(n, Ordering::Relaxed);
                    c.outln("repeats set");
                }
                _ => {
                    let mut b = [0u8; 64];
                    let pos = {
                        let mut w = Writer { buf: &mut b, pos: 0 };
                        let _ = core::fmt::Write::write_fmt(
                            &mut w,
                            format_args!(
                                "repeats={} gap={}ms",
                                TX_REPEATS.load(Ordering::Relaxed),
                                TX_GAP_MS.load(Ordering::Relaxed)
                            ),
                        );
                        w.pos
                    };
                    c.outln(core::str::from_utf8(&b[..pos]).unwrap_or(""));
                }
            }
        }
        "gap" => {
            match it.next().and_then(|v| v.parse::<u32>().ok()) {
                Some(ms) if ms <= 2000 => {
                    TX_GAP_MS.store(ms, Ordering::Relaxed);
                    c.outln("gap set");
                }
                _ => c.outln("usage: gap <ms>"),
            }
        }
        "txmode" => {
            match it.next() {
                Some("packet") => {
                    TX_PACKET.store(true, Ordering::Relaxed);
                    c.outln("tx mode: packet (radio sends the expanded bitstream)");
                }
                Some("async") => {
                    TX_PACKET.store(false, Ordering::Relaxed);
                    c.outln("tx mode: async (firmware drives GDO0 per edge)");
                }
                None => c.outln(if TX_PACKET.load(Ordering::Relaxed) {
                    "tx mode: packet (radio sends the expanded bitstream)"
                } else {
                    "tx mode: async (firmware drives GDO0 per edge)"
                }),
                Some(_) => c.outln("usage: txmode [packet|async]"),
            }
        }
        "reboot" => {
            c.outln("rebooting...");
            esp_hal::system::software_reset();
        }
        "name" => {
            // `name` -> device name, `name <slot> [text]` -> fan name
            let rest = line[cmd.len()..].trim();
            let (first, tail) = match rest.split_once(char::is_whitespace) {
                Some((a, b)) => (a, b.trim()),
                None => (rest, ""),
            };
            let slot = first.parse::<usize>().ok().filter(|s| *s < 8);

            if rest.is_empty() {
                let text = copy_name(STATE.lock(|s| s.borrow().cfg.device_name));
                c.outln(&fmt_name(&mut [0u8; 96], &text, None));
            } else if let Some(slot) = slot {
                if tail.is_empty() {
                    let text = copy_name(STATE.lock(|s| s.borrow().cfg.slots[slot].name));
                    c.outln(&fmt_name(&mut [0u8; 96], &text, Some(slot)));
                } else {
                    let ok = STATE.lock(|s| {
                        let mut st = s.borrow_mut();
                        if st.cfg.slots[slot].name.set(tail) {
                            save_config(&st.cfg)
                        } else {
                            false
                        }
                    });
                    c.outln(if ok {
                        "fan name saved"
                    } else {
                        "name too long (max 24 bytes) or save failed"
                    });
                }
            } else {
                let ok = STATE.lock(|s| {
                    let mut st = s.borrow_mut();
                    if st.cfg.device_name.set(rest) {
                        save_config(&st.cfg)
                    } else {
                        false
                    }
                });
                c.outln(if ok {
                    "device name saved"
                } else {
                    "name too long (max 24 bytes) or save failed"
                });
            }
        }
        "" => {}
        _ => c.outln("unknown command (type help)"),
    }
}

fn fmt_rx<'a>(b: &'a mut [u8; 80], addr: u32, button: u8, code: u32, rssi_dbm: i32) -> &'a str {
    use core::fmt::Write;
    let mut w = Writer80 { buf: b, pos: 0 };
    let _ = write!(
        w,
        "rx device=0x{addr:05X} button=0x{button:X} code=0x{code:06X} rssi={rssi_dbm}dBm"
    );
    let n = w.pos;
    core::str::from_utf8(&b[..n]).unwrap_or("")
}

/// CC1101 RSSI register (0x34) to dBm, as in the datasheet.
fn rssi_dbm(raw: u8) -> i32 {
    if raw >= 128 {
        (i32::from(raw) - 256) / 2 - 74
    } else {
        i32::from(raw) / 2 - 74
    }
}

/// Render a command that arrived over Zigbee, naming the slot it controls.
fn fmt_zb_event<'a>(b: &'a mut [u8; 80], ev: ZbEvent) -> &'a str {
    use core::fmt::Write;
    let (slot_name, slot_index) = STATE.lock(|s| {
        let st = s.borrow();
        if ev.slot == 0xFF {
            (Name::EMPTY, 0)
        } else {
            (st.cfg.slots[ev.slot as usize].name, ev.slot)
        }
    });
    let mut w = Writer80 { buf: b, pos: 0 };
    if ev.slot == 0xFF {
        let _ = write!(w, "zb: pairing -> {}", if ev.value != 0 { "on" } else { "off" });
    } else {
        let _ = write!(w, "zb: slot {slot_index}");
        if !slot_name.is_empty() {
            let _ = write!(w, " \"{}\"", slot_name.as_str());
        }
        match ev.kind {
            0 => match ev.value {
                0 => {
                    let _ = write!(w, " speed -> off");
                }
                speed => {
                    let _ = write!(w, " speed -> {speed}");
                }
            },
            1 => {
                let _ = write!(w, " power -> {}", if ev.value != 0 { "on" } else { "off" });
            }
            _ => {
                let _ = write!(w, " direction -> {}", dir_name(ev.value));
            }
        }
    }
    let n = w.pos;
    core::str::from_utf8(&b[..n]).unwrap_or("")
}

fn some_if(cond: bool, v: u8) -> Option<u8> {
    if cond { Some(v) } else { None }
}

enum Ev {
    None,
    Step(usize, u8, bool),
    Paired(u32, usize),
    NoSlot(u32),
}

/// Called for every decoded frame (addr, button).
fn on_frame(addr: u32, button: u8, now_ms: u64) -> Ev {
    STATE.lock(|s| {
        let mut st = s.borrow_mut();

        // ---- pairing state machine
        if st.pairing {
            if st.pair_last_ms != 0 && now_ms.saturating_sub(st.pair_last_ms) > PAIR_STEP_MS {
                st.pair_step = 0; // gap too long, restart
            }
            if st.pair_addr != addr {
                st.pair_addr = addr;
                st.pair_step = 0;
            }
            if button == PAIR_SEQ[st.pair_step] {
                st.pair_step += 1;
                st.pair_last_ms = now_ms;
                if st.pair_step == PAIR_SEQ.len() {
                    // learned! find a free slot
                    let existing = st.cfg.slots.iter().position(|s| s.used() && s.addr == addr);
                    let free = st.cfg.slots.iter().position(|s| !s.used());
                    let slot = existing.or(free);
                    if let Some(i) = slot {
                        let name = st.cfg.slots[i].name;
                        st.cfg.slots[i] = Slot {
                            addr,
                            speed: 0,
                            dir: 0,
                            power: false,
                            name,
                        };
                        save_config(&st.cfg);
                        st.pairing = false;
                        st.pair_step = 0;
                        st.pair_addr = 0;
                        st.stage = Stage::Paired;
                        st.stage_until_ms = now_ms + PAIR_RESULT_MS;
                        return Ev::Paired(addr, i);
                    } else {
                        st.pairing = false;
                        st.pair_step = 0;
                        st.pair_addr = 0;
                        st.stage = Stage::Failed;
                        st.stage_until_ms = now_ms + PAIR_RESULT_MS;
                        return Ev::NoSlot(addr);
                    }
                }
            } else if button == PAIR_SEQ[0] {
                st.pair_step = 1;
                st.pair_last_ms = now_ms;
            } else {
                st.pair_step = 0;
            }
            return Ev::Step(st.pair_step, button, true);
        }

        // ---- normal operation: track state of a known remote
        let mut changed = false;
        for (i, sl) in st.cfg.slots.iter_mut().enumerate() {
            if sl.used() && sl.addr == addr {
                apply_button(sl, button);
                changed = true;
                // the coordinator should see what the remote just did
                mark_fan_mode(i);
                mark_direction(i);
                break;
            }
        }
        if changed {
            let snapshot = st.cfg;
            drop(st);
            save_config(&snapshot);
        }
        Ev::None
    })
}

/// Format a line into `b`, return the &str.
fn fmt2<'a>(b: &'a mut [u8; 80], a: u32, x: u32) -> &'a str {
    use core::fmt::Write;
    let mut w = Writer80 { buf: b, pos: 0 };
    let _ = write!(w, "paired remote 0x{a:05X} into slot {x}");
    let n = w.pos;
    core::str::from_utf8(&b[..n]).unwrap_or("")
}

struct Writer96<'a> {
    buf: &'a mut [u8; 96],
    pos: usize,
}
impl core::fmt::Write for Writer96<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &c in s.as_bytes() {
            if self.pos < 96 {
                self.buf[self.pos] = c;
                self.pos += 1;
            }
        }
        Ok(())
    }
}

struct Writer80<'a> {
    buf: &'a mut [u8; 80],
    pos: usize,
}
impl core::fmt::Write for Writer80<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &c in s.as_bytes() {
            if self.pos < 80 {
                self.buf[self.pos] = c;
                self.pos += 1;
            }
        }
        Ok(())
    }
}


// ==========================================================================
// Zigbee layer
// ==========================================================================

const NWK_EPID: u64 = 0xd928_6c81_6768_f40b;
const NWK_CHANNEL: u8 = 11;
const ZIGBEE_FLASH: core::ops::Range<u32> = 0x3f_0000..0x3f_4000;

const EP_MASTER: u8 = 1;
const EP_FAN0: u8 = 2; // ..9

const CL_ONOFF: Cluster = Cluster::new(ClusterId(0x0006), "On/Off");
const CL_FAN: Cluster = Cluster::new(ClusterId(0x0202), "Fan Control");
const CL_LEVEL: Cluster = Cluster::new(ClusterId(0x0008), "Level Control");
/// Manufacturer-specific status cluster (no standard one carries this).
const CL_PAIRING: Cluster = Cluster::new(ClusterId(0xFC00), "espPairing");
/// Manufacturer-specific direction cluster: ZCL has no three-state direction.
const CL_DIRECTION: Cluster = Cluster::new(ClusterId(0xFC01), "espDirection");
const A_ONOFF: Attribute<Bool, ReadOnly, Reportable> =
    CL_ONOFF.attribute(AttributeId(0x0000), "OnOff");
const A_LEVEL: Attribute<Uint8, ReadWrite, Reportable> =
    CL_LEVEL.attribute(AttributeId(0x0000), "CurrentLevel");
const A_FANMODE: Attribute<Enum8<FanMode>, ReadWrite, Reportable> =
    CL_FAN.attribute(AttributeId(0x0000), "FanMode");
const A_STAGE: Attribute<Enum8<Stage>, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0000), "stage");
const A_PAIRED_COUNT: Attribute<Uint8, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0001), "pairedCount");
const A_DIRECTION: Attribute<Enum8<Direction>, ReadWrite, Reportable> =
    CL_DIRECTION.attribute(AttributeId(0x0000), "direction");
const A_EXPOSED_COUNT: Attribute<Uint8, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0002), "exposedCount");
const A_RESTART_REQUIRED: Attribute<Bool, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0003), "restartRequired");
static ONOFF_ATTRS: [AttrInfo; 1] = [A_ONOFF.attr_info()];
/// Shared reporting store: enough to accept Configure Reporting from the coordinator.
static REPORTING: ReportingTable<16> = ReportingTable::new();
static LEVEL_ATTRS: [AttrInfo; 1] = [A_LEVEL.attr_info()];
static FAN_ATTRS: [AttrInfo; 1] = [A_FANMODE.attr_info()];
static PAIRING_ATTRS: [AttrInfo; 4] = [
    A_STAGE.attr_info(),
    A_PAIRED_COUNT.attr_info(),
    A_EXPOSED_COUNT.attr_info(),
    A_RESTART_REQUIRED.attr_info(),
];
static DIRECTION_ATTRS: [AttrInfo; 1] = [A_DIRECTION.attr_info()];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FanMode {
    Off,
    Low,
    Medium,
    High,
    /// ZCL `on` (4): the remote's fourth speed.
    On,
}

impl ZclEnum8 for FanMode {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(FanMode::Off),
            1 => Some(FanMode::Low),
            2 => Some(FanMode::Medium),
            3 => Some(FanMode::High),
            4 => Some(FanMode::On),
            _ => None,
        }
    }
    fn into_raw(self) -> u8 {
        match self {
            FanMode::Off => 0,
            FanMode::Low => 1,
            FanMode::Medium => 2,
            FanMode::High => 3,
            FanMode::On => 4,
        }
    }
}

/// What the pairing state machine is doing, as reported to the coordinator:
/// which speed press it is waiting for, or how the last attempt ended.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    WaitingSpeed1,
    WaitingSpeed2,
    WaitingSpeed3,
    WaitingSpeed4,
    Paired,
    Failed,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Idle => "idle",
            Stage::WaitingSpeed1 => "waiting_speed_1",
            Stage::WaitingSpeed2 => "waiting_speed_2",
            Stage::WaitingSpeed3 => "waiting_speed_3",
            Stage::WaitingSpeed4 => "waiting_speed_4",
            Stage::Paired => "paired",
            Stage::Failed => "failed",
        }
    }
}

impl ZclEnum8 for Stage {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Stage::Idle),
            1 => Some(Stage::WaitingSpeed1),
            2 => Some(Stage::WaitingSpeed2),
            3 => Some(Stage::WaitingSpeed3),
            4 => Some(Stage::WaitingSpeed4),
            5 => Some(Stage::Paired),
            6 => Some(Stage::Failed),
            _ => None,
        }
    }
    fn into_raw(self) -> u8 {
        match self {
            Stage::Idle => 0,
            Stage::WaitingSpeed1 => 1,
            Stage::WaitingSpeed2 => 2,
            Stage::WaitingSpeed3 => 3,
            Stage::WaitingSpeed4 => 4,
            Stage::Paired => 5,
            Stage::Failed => 6,
        }
    }
}

/// Fan direction: the remote's two fixed ones plus the automatic mode.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
    Auto,
}

impl Direction {
    pub fn as_u8(self) -> u8 {
        match self {
            Direction::In => 0,
            Direction::Out => 1,
            Direction::Auto => 2,
        }
    }

    fn from_u8(value: u8) -> Direction {
        match value {
            1 => Direction::Out,
            2 => Direction::Auto,
            _ => Direction::In,
        }
    }
}

impl ZclEnum8 for Direction {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Direction::In),
            1 => Some(Direction::Out),
            2 => Some(Direction::Auto),
            _ => None,
        }
    }
    fn into_raw(self) -> u8 {
        self.as_u8()
    }
}

fn speed_of_mode(m: FanMode) -> u8 {
    match m {
        FanMode::Off => 0,
        FanMode::Low => 1,
        FanMode::Medium => 2,
        FanMode::High => 3,
        FanMode::On => 4,
    }
}

fn mode_of_speed(speed: u8) -> FanMode {
    match speed {
        1 => FanMode::Low,
        2 => FanMode::Medium,
        3 => FanMode::High,
        4 => FanMode::On,
        _ => FanMode::Off,
    }
}

fn level_of_speed(speed: u8) -> u8 {
    match speed {
        0 => 0,
        1 => 64,
        2 => 128,
        3 => 192,
        _ => 254,
    }
}

fn speed_of_level(level: u8) -> u8 {
    match level {
        0 => 0,
        1..=95 => 1,
        96..=159 => 2,
        160..=223 => 3,
        _ => 4,
    }
}

fn ok(r: byte::Result<()>) -> Status {
    match r {
        Ok(()) => Status::Success,
        Err(_) => Status::InsufficientSpace,
    }
}

/// On/Off server: which slot it drives and what the two states mean.
pub struct OnOffEp<const EP: u8, const SLOT: i8, const KIND: u8>; // KIND 0=power 1=pairing

impl<const EP: u8, const SLOT: i8, const KIND: u8> OnOffEp<EP, SLOT, KIND> {
    fn get(&self) -> bool {
        STATE.lock(|s| {
            let st = s.borrow();
            match KIND {
                0 => st.cfg.slots[SLOT as usize].power,
                _ => st.pairing,
            }
        })
    }
    fn set(&self, on: bool) {
        match KIND {
            1 => {
                STATE.lock(|s| {
                    let mut st = s.borrow_mut();
                    st.pairing = on;
                    st.pair_step = 0;
                    st.pair_addr = 0;
                });
            }
            _ => {
                let code = STATE.lock(|s| {
                    let mut st = s.borrow_mut();
                    let sl = &mut st.cfg.slots[SLOT as usize];
                    if !sl.used() {
                        return 0;
                    }
                    sl.power = on;
                    let snap = st.cfg;
                    drop(st);
                    save_config(&snap);
                    let sl = &snap.slots[SLOT as usize];
                    let btn = if on {
                        speed_button(if sl.speed == 0 { 1 } else { sl.speed }).unwrap_or(BTN_SPEED1)
                    } else {
                        BTN_POWER
                    };
                    code_for(sl, btn)
                });
                if code != 0 {
                    TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
                }
            }
        }
        match KIND {
            0 => {
                zb_event(1, SLOT.max(0) as u8, u8::from(on));
                mark_fan_mode(SLOT.max(0) as usize);
            }
            _ => {
                zb_event(3, 0xFF, u8::from(on));
                mark_pairing_switch();
            }
        }
    }
}

impl<const EP: u8, const SLOT: i8, const KIND: u8> ClusterServer for OnOffEp<EP, SLOT, KIND> {
    fn cluster(&self) -> Cluster {
        CL_ONOFF
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &ONOFF_ATTRS
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        ok(A_ONOFF.encode(self.get(), out, offset))
    }
    fn reporting(&self) -> Option<&dyn AttributeReporting> {
        Some(&REPORTING)
    }
    fn command(&self, c: ClusterCommand<'_>, _out: &mut [u8]) -> CommandOutcome {
        match c.id.0 {
            0x00 => {
                self.set(false);
                CommandOutcome::Status(Status::Success)
            }
            0x01 => {
                self.set(true);
                CommandOutcome::Status(Status::Success)
            }
            0x02 => {
                self.set(!self.get());
                CommandOutcome::Status(Status::Success)
            }
            _ => CommandOutcome::Status(Status::UnsupCommand),
        }
    }
}

impl<const EP: u8, const SLOT: i8, const KIND: u8> ClusterRequestHandler for OnOffEp<EP, SLOT, KIND> {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != EP {
            return None;
        }
        self.handle_request(r, out)
    }
}

/// Speed server on the ZCL Fan Control cluster (hvacFanCtrl), which Z2M turns
/// into a `fan` entity (with mode) per endpoint.
pub struct FanEp<const SLOT: usize>;

impl<const SLOT: usize> ClusterServer for FanEp<SLOT> {
    fn cluster(&self) -> Cluster {
        CL_FAN
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &FAN_ATTRS
    }
    fn reporting(&self) -> Option<&dyn AttributeReporting> {
        Some(&REPORTING)
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        let mode = STATE.lock(|s| {
            let st = s.borrow();
            let slot = &st.cfg.slots[SLOT];
            if slot.power { mode_of_speed(slot.speed) } else { FanMode::Off }
        });
        ok(A_FANMODE.encode(mode, out, offset))
    }
    fn decode_value(
        &self,
        id: AttributeId,
        ty: TypeId,
        bytes: &[u8],
        offset: &mut usize,
    ) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        let Ok(mode) = A_FANMODE.decode(ty, bytes, offset) else {
            return Status::InvalidDataType;
        };
        self.apply(speed_of_mode(mode));
        Status::Success
    }
}

impl<const SLOT: usize> FanEp<SLOT> {
    fn apply(&self, speed: u8) {
        let code = STATE.lock(|s| {
            let mut st = s.borrow_mut();
            let sl = &mut st.cfg.slots[SLOT];
            if !sl.used() {
                return 0;
            }
            if speed == 0 {
                if !sl.power {
                    return 0;
                }
                sl.power = false;
                let snap = st.cfg;
                drop(st);
                save_config(&snap);
                return code_for(&snap.slots[SLOT], BTN_POWER);
            }
            sl.speed = speed;
            sl.power = true;
            let snap = st.cfg;
            drop(st);
            save_config(&snap);
            code_for(&snap.slots[SLOT], speed_button(speed).unwrap())
        });
        if code != 0 {
            TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
        }
        zb_event(0, SLOT as u8, speed);
        mark_fan_mode(SLOT);
    }
}

/// Status server for the pairing state machine, on the pairing endpoint.
pub struct PairingEp;

impl ClusterServer for PairingEp {
    fn cluster(&self) -> Cluster {
        CL_PAIRING
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &PAIRING_ATTRS
    }
    fn reporting(&self) -> Option<&dyn AttributeReporting> {
        Some(&REPORTING)
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        let (count, exposed, restart) = STATE.lock(|s| {
            let st = s.borrow();
            let (count, exposed) = (st.paired_count(), st.exposed_count());
            (count, exposed, count != exposed)
        });
        let stage = Stage::from_raw(STAGE_NOW.load(Ordering::Relaxed) as u8).unwrap_or(Stage::Idle);
        ok(match id.0 {
            0x0000 => A_STAGE.encode(stage, out, offset),
            0x0001 => A_PAIRED_COUNT.encode(Uint8(count), out, offset),
            0x0002 => A_EXPOSED_COUNT.encode(Uint8(exposed), out, offset),
            0x0003 => A_RESTART_REQUIRED.encode(restart, out, offset),
            _ => return Status::UnsupportedAttribute,
        })
    }
}

impl ClusterRequestHandler for PairingEp {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != EP_MASTER {
            return None;
        }
        self.handle_request(r, out)
    }
}

/// The stage last published to the coordinator, as a raw enum value; a read
/// sees the same value a report carries.
static STAGE_NOW: AtomicU32 = AtomicU32::new(0);

/// Direction server, on the same endpoint as the fan it belongs to: ZCL has no
/// three-state direction attribute, so this is a manufacturer-specific cluster.
pub struct DirEp<const SLOT: usize>;

impl<const SLOT: usize> ClusterServer for DirEp<SLOT> {
    fn cluster(&self) -> Cluster {
        CL_DIRECTION
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &DIRECTION_ATTRS
    }
    fn reporting(&self) -> Option<&dyn AttributeReporting> {
        Some(&REPORTING)
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        let dir = STATE.lock(|s| Direction::from_u8(s.borrow().cfg.slots[SLOT].dir));
        ok(A_DIRECTION.encode(dir, out, offset))
    }
    fn decode_value(&self, id: AttributeId, ty: TypeId, bytes: &[u8], offset: &mut usize) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        let Ok(dir) = A_DIRECTION.decode(ty, bytes, offset) else {
            return Status::InvalidDataType;
        };
        self.apply(dir);
        Status::Success
    }
}

impl<const SLOT: usize> DirEp<SLOT> {
    fn apply(&self, dir: Direction) {
        let code = STATE.lock(|s| {
            let mut st = s.borrow_mut();
            let slot = &mut st.cfg.slots[SLOT];
            if !slot.used() {
                return 0;
            }
            slot.dir = dir.as_u8();
            let snap = st.cfg;
            drop(st);
            save_config(&snap);
            code_for(&snap.slots[SLOT], dir_button(dir.as_u8()).unwrap())
        });
        if code != 0 {
            TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
        }
        zb_event(2, SLOT as u8, dir.as_u8());
        mark_direction(SLOT);
    }
}

impl<const SLOT: usize> ClusterRequestHandler for DirEp<SLOT> {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != EP_FAN0 + SLOT as u8 {
            return None;
        }
        self.handle_request(r, out)
    }
}

impl<const SLOT: usize> ClusterRequestHandler for FanEp<SLOT> {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != EP_FAN0 + SLOT as u8 {
            return None;
        }
        self.handle_request(r, out)
    }
}

// ---- identity
/// What this build is. Printed on boot and reported as `SWBuildID`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("FANBRIDGE_GIT"));
/// Build date, `YYYYMMDD`, reported as `DateCode`.
const BUILD_DATE: &str = env!("FANBRIDGE_DATE");
/// What the Basic cluster reports as `SWBuildID`; ZCL limits it to 16 bytes,
/// so it is the tag when it fits and the commit otherwise.
const SW_BUILD_ID: &str = env!("FANBRIDGE_SW_BUILD_ID");

const VENDOR: &str = "locals.tf";
const MODEL_MASTER: &str = "esp-fanbridge";

/// `DateCode` (ZCL 3.2.2.2.8) and `SWBuildID` (ZCL 3.2.2.2.12) are not in the
/// minimal set the stack's `BasicServer` exposes, but they are what consumers
/// show as the firmware/software version.
const A_DATE_CODE: Attribute<ShortText, ReadOnly> =
    basic::CLUSTER.attribute(AttributeId(0x0006), "DateCode");
const A_SW_BUILD_ID: Attribute<ShortText, ReadOnly> =
    basic::CLUSTER.attribute(AttributeId(0x4000), "SWBuildID");

static BASIC_ATTRS: [AttrInfo; 9] = [
    basic::ZCL_VERSION.attr_info(),
    basic::APPLICATION_VERSION.attr_info(),
    basic::STACK_VERSION.attr_info(),
    basic::HW_VERSION.attr_info(),
    basic::MANUFACTURER_NAME.attr_info(),
    basic::MODEL_IDENTIFIER.attr_info(),
    A_DATE_CODE.attr_info(),
    basic::POWER_SOURCE.attr_info(),
    A_SW_BUILD_ID.attr_info(),
];

/// `Basic` server with a build identity on top of the mandatory attributes.
pub struct Basic {
    /// Endpoint this identity belongs to.
    pub ep: u8,
    pub manufacturer: &'static str,
    pub model: &'static str,
}

/// Encode a `CharacterString` attribute, or bail out if it does not fit.
macro_rules! text_attr {
    ($attr:expr, $text:expr, $out:expr, $off:expr) => {{
        let Some(value) = ShortStr::new($text) else {
            return Status::InvalidValue;
        };
        $attr.encode(value, $out, $off)
    }};
}

impl ClusterRequestHandler for Basic {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != self.ep {
            return None;
        }
        self.handle_request(r, out)
    }
}

impl ClusterServer for Basic {
    fn cluster(&self) -> Cluster {
        basic::CLUSTER
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &BASIC_ATTRS
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        ok(match id.0 {
            0x0000 => basic::ZCL_VERSION.encode(Uint8(8), out, offset),
            0x0001 => basic::APPLICATION_VERSION.encode(Uint8(1), out, offset),
            0x0002 => basic::STACK_VERSION.encode(Uint8(0), out, offset),
            0x0003 => basic::HW_VERSION.encode(Uint8(1), out, offset),
            0x0004 => text_attr!(basic::MANUFACTURER_NAME, self.manufacturer, out, offset),
            0x0005 => text_attr!(basic::MODEL_IDENTIFIER, self.model, out, offset),
            0x0006 => text_attr!(A_DATE_CODE, BUILD_DATE, out, offset),
            0x0007 => basic::POWER_SOURCE.encode(basic::PowerSource(0x01), out, offset),
            0x4000 => text_attr!(A_SW_BUILD_ID, SW_BUILD_ID, out, offset),
            _ => return Status::UnsupportedAttribute,
        })
    }
}

pub static BASIC_MASTER: Basic = Basic {
    ep: EP_MASTER,
    manufacturer: VENDOR,
    model: MODEL_MASTER,
};
pub static BASIC_F0: Basic = Basic {
    ep: EP_FAN0 + 0,
    manufacturer: VENDOR,
    model: "fan-1",
};
pub static BASIC_F1: Basic = Basic {
    ep: EP_FAN0 + 1,
    manufacturer: VENDOR,
    model: "fan-2",
};
pub static BASIC_F2: Basic = Basic {
    ep: EP_FAN0 + 2,
    manufacturer: VENDOR,
    model: "fan-3",
};
pub static BASIC_F3: Basic = Basic {
    ep: EP_FAN0 + 3,
    manufacturer: VENDOR,
    model: "fan-4",
};
pub static BASIC_F4: Basic = Basic {
    ep: EP_FAN0 + 4,
    manufacturer: VENDOR,
    model: "fan-5",
};
pub static BASIC_F5: Basic = Basic {
    ep: EP_FAN0 + 5,
    manufacturer: VENDOR,
    model: "fan-6",
};
pub static BASIC_F6: Basic = Basic {
    ep: EP_FAN0 + 6,
    manufacturer: VENDOR,
    model: "fan-7",
};
pub static BASIC_F7: Basic = Basic {
    ep: EP_FAN0 + 7,
    manufacturer: VENDOR,
    model: "fan-8",
};

pub static IDENTIFY: IdentifyServer = IdentifyServer::new();

// ---- endpoint table
static IN_MASTER: [u16; 3] = [basic::CLUSTER_ID, identify::CLUSTER_ID, 0x0006];
static IN_FAN: [u16; 3] = [basic::CLUSTER_ID, 0x0202, 0xFC01];
static NO_CLUSTERS: [u16; 0] = [];

/// Endpoint table built at boot from the config: only paired remotes get
/// endpoints, so a coordinator sees exactly the fans that exist.
static EP_BUF: static_cell::StaticCell<[EndpointDescriptor<'static>; 9]> =
    static_cell::StaticCell::new();

/// The slice handed to the stack, kept so `desc` can print it.
static EP_LIST: CsMutex<CriticalSectionRawMutex, RefCell<Option<&'static [EndpointDescriptor<'static>]>>> =
    CsMutex::new(RefCell::new(None));

fn master_descriptor() -> EndpointDescriptor<'static> {
    EndpointDescriptor {
        endpoint: EP_MASTER,
        profile_id: profile::HOME_AUTOMATION,
        device_id: 0x0000,
        device_version: 1,
        input_clusters: &IN_MASTER,
        output_clusters: &NO_CLUSTERS,
    }
}

fn fan_descriptor(slot: usize) -> EndpointDescriptor<'static> {
    EndpointDescriptor {
        endpoint: EP_FAN0 + slot as u8,
        profile_id: profile::HOME_AUTOMATION,
        device_id: 0x002B,
        device_version: 1,
        input_clusters: &IN_FAN,
        output_clusters: &NO_CLUSTERS,
    }
}

/// Build the advertised endpoint list: endpoint 1 always, then one endpoint per
/// paired slot (endpoints stay tied to their slot, so a removed slot leaves a
/// gap rather than renumbering the others).
fn build_endpoints(cfg: &Config) -> &'static [EndpointDescriptor<'static>] {
    let blank = EndpointDescriptor {
        endpoint: 0,
        profile_id: profile::HOME_AUTOMATION,
        device_id: 0,
        device_version: 1,
        input_clusters: &NO_CLUSTERS,
        output_clusters: &NO_CLUSTERS,
    };
    let buf = EP_BUF.init([blank; 9]);
    let mut n = 0;
    buf[n] = master_descriptor();
    n += 1;
    for i in 0..8 {
        if cfg.slots[i].used() {
            buf[n] = fan_descriptor(i);
            n += 1;
        }
    }
    let list: &'static [EndpointDescriptor<'static>] = &buf[..n];
    EP_LIST.lock(|l| *l.borrow_mut() = Some(list));
    list
}

/// The advertised endpoints (empty before the table is built).
fn advertised() -> &'static [EndpointDescriptor<'static>] {
    EP_LIST.lock(|l| *l.borrow()).unwrap_or(&[])
}

pub static PAIR_SW: OnOffEp<1, -1, 1> = OnOffEp;
pub static PAIRING: PairingEp = PairingEp;
pub static PWR0: OnOffEp<2, 0, 0> = OnOffEp;
pub static PWR1: OnOffEp<3, 1, 0> = OnOffEp;
pub static PWR2: OnOffEp<4, 2, 0> = OnOffEp;
pub static PWR3: OnOffEp<5, 3, 0> = OnOffEp;
pub static PWR4: OnOffEp<6, 4, 0> = OnOffEp;
pub static PWR5: OnOffEp<7, 5, 0> = OnOffEp;
pub static PWR6: OnOffEp<8, 6, 0> = OnOffEp;
pub static PWR7: OnOffEp<9, 7, 0> = OnOffEp;
pub static FANDIR0: DirEp<0> = DirEp;
pub static FANDIR1: DirEp<1> = DirEp;
pub static FANDIR2: DirEp<2> = DirEp;
pub static FANDIR3: DirEp<3> = DirEp;
pub static FANDIR4: DirEp<4> = DirEp;
pub static FANDIR5: DirEp<5> = DirEp;
pub static FANDIR6: DirEp<6> = DirEp;
pub static FANDIR7: DirEp<7> = DirEp;
pub static FAN0: FanEp<0> = FanEp;
pub static FAN1: FanEp<1> = FanEp;
pub static FAN2: FanEp<2> = FanEp;
pub static FAN3: FanEp<3> = FanEp;
pub static FAN4: FanEp<4> = FanEp;
pub static FAN5: FanEp<5> = FanEp;
pub static FAN6: FanEp<6> = FanEp;
pub static FAN7: FanEp<7> = FanEp;

/// Answers `UNSUPPORTED_CLUSTER` to every request no server claimed.
static UNSUPPORTED: UnsupportedClusterResponder<'static> = UnsupportedClusterResponder::new(&[]);

/// Dispatches to all cluster servers in turn (trait objects avoid a 40-deep tuple).
pub struct Chain;

impl ClusterRequestHandler for Chain {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        const SRV: [&dyn ClusterRequestHandler; 37] = [
            &BASIC_MASTER, &IDENTIFY, &PAIR_SW, &PAIRING,
            &BASIC_F0, &FAN0, &PWR0, &FANDIR0,
            &BASIC_F1, &FAN1, &PWR1, &FANDIR1,
            &BASIC_F2, &FAN2, &PWR2, &FANDIR2,
            &BASIC_F3, &FAN3, &PWR3, &FANDIR3,
            &BASIC_F4, &FAN4, &PWR4, &FANDIR4,
            &BASIC_F5, &FAN5, &PWR5, &FANDIR5,
            &BASIC_F6, &FAN6, &PWR6, &FANDIR6,
            &BASIC_F7, &FAN7, &PWR7, &FANDIR7,
            // last: anything nobody handled still gets an answer, so a
            // coordinator does not wait for a response that never comes
            &UNSUPPORTED,
        ];
        SRV.iter().find_map(|h| h.handle(r, out))
    }
}

pub type ZbFlash = zigbee::storage::FlashStorage<embassy_embedded_hal::adapter::BlockingAsync<esp_storage::FlashStorage<'static>>>;
pub type ZbStack = zigbee::Stack<'static, zigbee_mac::esp::EspMlme<'static>, Chain, ZbFlash>;
pub static STACK: static_cell::StaticCell<ZbStack> = static_cell::StaticCell::new();

fn stack_config(endpoints: &'static [EndpointDescriptor<'static>]) -> StackConfig<'static> {
    StackConfig::new(
        NetworkConfig {
            extended_pan_id: IeeeAddress(NWK_EPID),
            channels: NWK_CHANNEL..NWK_CHANNEL + 1,
            scan_duration: 5,
        },
        DeviceConfig {
            logical_type: LogicalType::EndDevice,
            // bit7 allocate address + bit3 rx-on-when-idle: mains powered, stay awake
            capability_information: CapabilityInformation(0x80),
            discovery_type: DiscoveryType::default(),
            tc_link_key_exchange: true,
        },
        TimingConfig { poll_interval_ms: 100, ..TimingConfig::default() },
        DeviceDescriptorConfig {
            node: NodeDescriptorConfig {
                frequency_band: 0x08,
                manufacturer_code: 0x1037,
                maximum_buffer_size: 80,
                maximum_incoming_transfer_size: 128,
                server_mask: 0,
                maximum_outgoing_transfer_size: 128,
                descriptor_capability_field: 0,
                ..NodeDescriptorConfig::default()
            },
            power: PowerDescriptorConfig {
                current_power_mode: CurrentPowerMode::Stimulated,
                available_power_sources: &[PowerSource::ConstantMainPower],
                current_power_source: PowerSource::ConstantMainPower,
                current_power_source_level: CurrentPowerSourceLevel::Full,
            },
            endpoints,
        },
    )
}

/// Send reports from their own task.
///
/// A report waits for an APS acknowledgement; doing that inline in the main loop
/// would stall RF reception and the console whenever the coordinator is slow or
/// unreachable. Here a failed send just leaves the bit set for the next pass.
#[embassy_executor::task]
async fn report_task(stack: &'static ZbStack) {
    loop {
        if PAIRING_REPORT.load(Ordering::Relaxed) {
            if send_pairing_report(stack).await {
                PAIRING_REPORT.store(false, Ordering::Relaxed);
            }
        } else {
            let dirty = REPORT_DIRTY.load(Ordering::Relaxed);
            if dirty != 0 {
                let bit = dirty.trailing_zeros();
                if send_state_report(stack, bit).await {
                    REPORT_DIRTY.fetch_and(!(1 << bit), Ordering::Relaxed);
                }
            }
        }
        Timer::after_millis(50).await;
    }
}

/// Announce the device once it is on the network.
///
/// A resume after a reset keeps the stored short address but does not announce
/// it, so a coordinator that lost the route only rediscovers it after its writes
/// fail (Zigbee2MQTT logs `MAC_TRANSACTION_EXPIRED`, then does a `nwkAddrReq`).
/// Three quick broadcasts right after joining close that gap; they are cheap and
/// harmless when the coordinator already knows us.
#[embassy_executor::task]
async fn announce_task(stack: &'static ZbStack) {
    stack.wait_until_joined().await;
    // the coordinator starts from defaults: give it the real state once
    mark_all_state();
    for _ in 0..3 {
        match stack.device().announce_self().await {
            Ok(()) => println!("zigbee: announced"),
            Err(e) => println!("zigbee: announce failed: {e:?}"),
        }
        Timer::after_millis(400).await;
    }
}

#[embassy_executor::task]
async fn stack_task(stack: &'static ZbStack) {
    let outcome = stack.run(AsyncDelay).await;
    println!("zigbee stack stopped: {outcome:?}");
    esp_hal::system::software_reset();
}

/// Send one pending state report (see [`REPORT_DIRTY`]).
///
/// Returns whether the coordinator accepted it, so a failed attempt keeps the
/// bit set and is retried.
async fn send_state_report(stack: &ZbStack, bit: u32) -> bool {
    use core::sync::atomic::AtomicU8;
    static SEQ: AtomicU8 = AtomicU8::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let mut buf = [0u8; 32];

    let (src_endpoint, frame) = if bit < 8 {
        let slot = bit as usize;
        let (used, mode) = STATE.lock(|s| {
            let st = s.borrow();
            let sl = &st.cfg.slots[slot];
            (sl.used(), if sl.power { mode_of_speed(sl.speed) } else { FanMode::Off })
        });
        if !used {
            return true; // nothing behind this bit, drop it
        }
        let Ok(frame) = AttributeReportBuilder::new(&mut buf, seq)
            .and_then(|b| b.push(&A_FANMODE, mode))
            .and_then(AttributeReportBuilder::finish)
        else {
            return false;
        };
        (EP_FAN0 + slot as u8, frame)
    } else if bit < 16 {
        let slot = (bit - 8) as usize;
        let (used, dir) = STATE.lock(|s| {
            let st = s.borrow();
            (st.cfg.slots[slot].used(), Direction::from_u8(st.cfg.slots[slot].dir))
        });
        if !used {
            return true;
        }
        let Ok(frame) = AttributeReportBuilder::new(&mut buf, seq)
            .and_then(|b| b.push(&A_DIRECTION, dir))
            .and_then(AttributeReportBuilder::finish)
        else {
            return false;
        };
        (EP_FAN0 + slot as u8, frame)
    } else {
        let pairing = STATE.lock(|s| s.borrow().pairing);
        let Ok(frame) = AttributeReportBuilder::new(&mut buf, seq)
            .and_then(|b| b.push(&A_ONOFF, pairing))
            .and_then(AttributeReportBuilder::finish)
        else {
            return false;
        };
        (EP_MASTER, frame)
    };

    matches!(
        stack
            .device()
            .send_attribute_report(
                ZclReportTarget {
                    dst_short: 0,
                    src_endpoint,
                    dst_endpoint: 1,
                    profile_id: profile::HOME_AUTOMATION,
                },
                frame,
                &mut AsyncDelay,
            )
            .await,
        Ok(confirm) if confirm.status == zigbee::aps::apsde::ApsdeSapConfirmStatus::Success
    )
}

/// Send the pairing stage + paired count to the coordinator.
///
/// Returns whether it was accepted, so a failed attempt can be retried instead
/// of leaving the coordinator with a stale value forever.
async fn send_pairing_report(stack: &ZbStack) -> bool {
    use core::sync::atomic::AtomicU8;
    static SEQ: AtomicU8 = AtomicU8::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let stage = Stage::from_raw(STAGE_NOW.load(Ordering::Relaxed) as u8).unwrap_or(Stage::Idle);
    let (count, exposed) = STATE.lock(|s| {
        let st = s.borrow();
        (st.paired_count(), st.exposed_count())
    });
    let mut buf = [0u8; 32];
    let Ok(report) = AttributeReportBuilder::new(&mut buf, seq)
        .and_then(|b| b.push(&A_STAGE, stage))
        .and_then(|b| b.push(&A_PAIRED_COUNT, Uint8(count)))
        .and_then(|b| b.push(&A_EXPOSED_COUNT, Uint8(exposed)))
        .and_then(|b| b.push(&A_RESTART_REQUIRED, count != exposed))
        .and_then(AttributeReportBuilder::finish)
    else {
        return false;
    };
    matches!(stack
        .device()
        .send_attribute_report(
            ZclReportTarget {
                dst_short: 0,
                src_endpoint: EP_MASTER,
                dst_endpoint: 1,
                profile_id: profile::HOME_AUTOMATION,
            },
            report,
            &mut AsyncDelay,
        )
        .await,
        Ok(confirm) if confirm.status == zigbee::aps::apsde::ApsdeSapConfirmStatus::Success
    )
}

// ----------------------------------------------------------------------- main
#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default());

    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);
    esp_alloc::heap_allocator!(size: 12 * 1024);

    let mut d = Delay::new();
    let mut spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_khz(2000))
            .with_mode(Mode::_0),
    )
    .expect("spi")
    .with_sck(peripherals.GPIO23)
    .with_mosi(peripherals.GPIO22)
    .with_miso(peripherals.GPIO21);
    let mut cs = Output::new(peripherals.GPIO4, Level::High, OutputConfig::default());
    let mut gdo0 = Flex::new(peripherals.GPIO5);
    gdo0.apply_input_config(&InputConfig::default());
    gdo0.set_output_enable(false);
    gdo0.set_input_enable(true);

    let mut led = Output::new(peripherals.GPIO15, Level::Low, OutputConfig::default());

    let cfg = load_config();
    let paired = cfg.slots.iter().filter(|s| s.used()).count();
    let endpoints = build_endpoints(&cfg);
    STATE.lock(|s| s.borrow_mut().cfg = cfg);

    println!("=== esp-fanbridge {VERSION} (built {BUILD_DATE}) ===");
    let version = cc1101_init(&mut spi, &mut cs, &mut d);
    println!("CC1101 VERSION=0x{version:02x} (want 0x14) at {FREQ_KHZ} kHz");
    enter_rx(&mut spi, &mut cs);
    println!("{paired} remote(s) configured, {n_endpoints} endpoint(s) advertised", n_endpoints = endpoints.len());
    if !cfg.device_name.is_empty() {
        println!("device name: {}", cfg.device_name.as_str());
    }

    // ---- Zigbee
    let flash = embassy_embedded_hal::adapter::BlockingAsync::new(esp_storage::FlashStorage::new(peripherals.FLASH));
    let storage = zigbee::storage::init_with_flash(flash, ZIGBEE_FLASH).await;
    let ieee = esp_radio::ieee802154::Ieee802154::new(peripherals.IEEE802154);
    let mac = zigbee_mac::esp::EspMlme::new(ieee, esp_radio::ieee802154::Config::default());
    println!("Zigbee IEEE address: {:#018x}", mac.ieee_address());
    let stack: &'static ZbStack = STACK.init(zigbee::Stack::new(mac, stack_config(endpoints), Chain, storage));

    spawner.spawn(stack_task(stack).expect("spawn stack_task"));
    spawner.spawn(announce_task(stack).expect("spawn announce_task"));
    spawner.spawn(report_task(stack).expect("spawn report_task"));

    let mut console = Console::new(UsbSerialJtag::new(peripherals.USB_DEVICE));
    console.outln("fan-bridge ready - type 'help'");

    let t0 = Instant::now();
    let mut segs = [Seg { dt: 0, high: false }; MAX_SEG];
    let mut n = 0usize;
    let mut logical = gdo0.is_high();
    let mut seg_start = Instant::now();
    let mut last_code: Option<u32> = None;
    let mut last_ms: u64 = 0;
    let mut led_blink = 0u32;
    let mut last_rx_check_ms: u64 = 0;
    // set when a pairing changed the endpoint set and a restart is scheduled
    let mut restart_at_ms: Option<u64> = None;
    let mut linebuf = [0u8; 80];
    let mut clock_on = false;
    let mut raw_prev = gdo0.is_high();

    loop {
        // ---- console
        if let Some(line) = console.poll() {
            let s = cmd_str(&line);
            handle_command(&mut console, s);
        }

        let now_ms = t0.elapsed().as_millis();
        UPTIME_MS.store(now_ms as u32, Ordering::Relaxed);

        // ---- RX watchdog: the radio can end up in IDLE (after a burst, an
        // underflow or an RX termination) and then silently misses everything.
        // Re-arm it whenever it is not actually receiving.
        if now_ms.saturating_sub(last_rx_check_ms) > 500 {
            last_rx_check_ms = now_ms;
            let marc = rstat(&mut spi, &mut cs, 0x35) & 0x1F;
            if marc != 0x0D {
                if DEBUG_ON.load(Ordering::Relaxed) {
                    let mut b = [0u8; 64];
                    let pos = {
                        let mut w = Writer { buf: &mut b, pos: 0 };
                        let _ = core::fmt::Write::write_fmt(
                            &mut w,
                            format_args!("rx rearm (marcstate=0x{marc:02x})"),
                        );
                        w.pos
                    };
                    let t = core::str::from_utf8(&b[..pos]).unwrap_or("");
                    console.outln(t);
                }
                enter_rx(&mut spi, &mut cs);
                REARM_COUNT.fetch_add(1, Ordering::Relaxed);
                logical = gdo0.is_high();
                seg_start = Instant::now();
                n = 0;
            }
        }

        // ---- pairing status: publish it when it changes
        {
            let (stage, count, exposed) = STATE.lock(|s| {
                let st = s.borrow();
                (
                    st.stage_at(now_ms),
                    u32::from(st.paired_count()),
                    u32::from(st.exposed_count()),
                )
            });
            let raw = u32::from(stage.into_raw());
            // `exposed` only changes after a reboot, but the pairing status is
            // what tells the user a restart is pending
            if STAGE_NOW.load(Ordering::Relaxed) != raw
                || PAIRED_NOW.load(Ordering::Relaxed) != count
                || EXPOSED_NOW.load(Ordering::Relaxed) != exposed
            {
                STAGE_NOW.store(raw, Ordering::Relaxed);
                PAIRED_NOW.store(count, Ordering::Relaxed);
                EXPOSED_NOW.store(exposed, Ordering::Relaxed);
                PAIRING_REPORT.store(true, Ordering::Relaxed);
            }
        }

        // ---- commands received over Zigbee
        while let Ok(ev) = ZB_EVENTS.try_receive() {
            let text = fmt_zb_event(&mut linebuf, ev);
            console.outln(text);
        }

        // ---- pending transmit
        let req = TX_REQ.swap(0, Ordering::Relaxed);
        if req != 0 {
            let code = req & 0x00FF_FFFF;
            TX_COUNT.fetch_add(1, Ordering::Relaxed);
            let took = if TX_PACKET.load(Ordering::Relaxed) {
                send_code_packet(&mut spi, &mut cs, code).await
            } else {
                send_code(&mut spi, &mut cs, &mut gdo0, code).await;
                0
            };
            if DEBUG_ON.load(Ordering::Relaxed) {
                let mut b = [0u8; 80];
                let pos = {
                    let mut w = Writer80 { buf: &mut b, pos: 0 };
                    let _ = core::fmt::Write::write_fmt(
                        &mut w,
                        format_args!(
                            "tx 0x{code:06X} via {} took {took} ms",
                            if TX_PACKET.load(Ordering::Relaxed) { "packet" } else { "bit-bang" }
                        ),
                    );
                    w.pos
                };
                let t = core::str::from_utf8(&b[..pos]).unwrap_or("");
                console.outln(t);
            }
            logical = gdo0.is_high();
            seg_start = Instant::now();
            n = 0;
        }

        // ---- debug requests
        if STAT_REQ.swap(false, Ordering::Relaxed) {
            let marc = rstat(&mut spi, &mut cs, 0x35);
            let rssi = rstat(&mut spi, &mut cs, 0x34);
            let mut b = [0u8; 80];
            {
                use core::fmt::Write;
                let mut w = Writer80 { buf: &mut b, pos: 0 };
                let dbm = rssi_dbm(rssi);
                let iocfg0 = rreg(&mut spi, &mut cs, IOCFG0);
                let pktctl0 = rreg(&mut spi, &mut cs, PKTCTRL0);
                let mdmcfg2 = rreg(&mut spi, &mut cs, MDMCFG2);
                let raw = RAW_EDGES.swap(0, Ordering::Relaxed);
                let _ = write!(w, "marcstate=0x{marc:02x} rssi={dbm}dBm iocfg0=0x{iocfg0:02x} pktctrl0=0x{pktctl0:02x} mdmcfg2=0x{mdmcfg2:02x} segs={n} raw={raw} gdo0={}", 
                    u8::from(gdo0.is_high()));
            }
            let txt = core::str::from_utf8(&b).unwrap_or("");
            console.outln(txt);
        }
        if CLOCK_REQ.swap(false, Ordering::Relaxed) {
            clock_on = !clock_on;
            strobe(&mut spi, &mut cs, SIDLE);
            wreg(&mut spi, &mut cs, IOCFG0, if clock_on { 0x39 } else { 0x0D });
            enter_rx(&mut spi, &mut cs);
            console.outln(if clock_on {
                "GDO0 = XOSC/192 clock (~135 kHz) - raw edges should explode"
            } else {
                "GDO0 = serial data out (normal)"
            });
        }
        if DUMP_REQ.swap(false, Ordering::Relaxed) {
            let mut b = [0u8; 80];
            {
                use core::fmt::Write;
                let mut w = Writer80 { buf: &mut b, pos: 0 };
                let _ = write!(w, "segs n={n}");
            }
            console.outln(core::str::from_utf8(&b).unwrap_or(""));
            for i in 0..n.min(24) {
                let mut b2 = [0u8; 80];
                {
                    use core::fmt::Write;
                    let mut w = Writer80 { buf: &mut b2, pos: 0 };
                    let _ = write!(w, "  [{i}] {}us {}", segs[i].dt, if segs[i].high { "H" } else { "L" });
                }
                let t = core::str::from_utf8(&b2).unwrap_or("");
                console.outln(t);
            }
        }

        // ---- RF receive / decode
        let raw = gdo0.is_high();
        if raw != raw_prev {
            RAW_EDGES.fetch_add(1, Ordering::Relaxed);
            raw_prev = raw;
        }
        if raw != logical {
            let dt = seg_start.elapsed().as_micros() as u32;
            if dt >= NOISE_US {
                if n < MAX_SEG {
                    segs[n] = Seg { dt, high: logical };
                    n += 1;
                } else {
                    n = 0;
                }
                logical = raw;
                seg_start = Instant::now();
            }
        } else if n > 0 && seg_start.elapsed().as_micros() as u32 > FRAME_GAP_US {
            let decoded = decode_frame(&segs[..n]);
            if decoded.is_none() && n >= 16 && DEBUG_ON.load(Ordering::Relaxed) {
                let mut b = [0u8; 80];
                {
                    use core::fmt::Write;
                    let mut w = Writer80 { buf: &mut b, pos: 0 };
                    let total: u32 = segs[..n].iter().map(|x| x.dt).sum();
                    let _ = write!(w, "junk: n={n} total={total}us first={}us", segs[0].dt);
                }
                let t = core::str::from_utf8(&b).unwrap_or("");
                console.outln(t);
            }
            if let Some(bits) = decoded {
                let now = t0.elapsed().as_millis();
                if last_code != Some(bits) || now.saturating_sub(last_ms) > 800 {
                    let addr = bits >> 4;
                    let button = (bits & 0xF) as u8;
                    // the level the remote arrived at: the first thing to look
                    // at when presses start going missing
                    RX_COUNT.fetch_add(1, Ordering::Relaxed);
                    let rssi = rssi_dbm(rstat(&mut spi, &mut cs, 0x34));
                    console.outln(fmt_rx(&mut linebuf, addr, button, bits, rssi));
                    match on_frame(addr, button, now) {
                        Ev::Paired(a, i) => {
                            console.outln(fmt2(&mut linebuf, a, i as u32));
                            // The endpoint table is built at boot, so a remote
                            // learned now has no endpoint until a restart. Do it
                            // automatically instead of asking the user to.
                            console.outln("restarting in 3 s to expose the new endpoint");
                            restart_at_ms = Some(now + 3_000);
                        }
                        Ev::NoSlot(a) => {
                            console.outln(fmt2(&mut linebuf, a, 99));
                        }
                        Ev::Step(step, btn, _) => {
                            let mut b = [0u8; 80];
                            {
                                use core::fmt::Write;
                                let mut w = Writer80 { buf: &mut b, pos: 0 };
                                let _ = write!(w, "pair: got button=0x{btn:X} -> step now {step}");
                            }
                            let t = core::str::from_utf8(&b).unwrap_or("");
                            console.outln(t);
                        }
                        Ev::None => {}
                    }
                    last_code = Some(bits);
                    last_ms = now;
                }
            }
            n = 0;
        }

        // ---- apply a pending endpoint change
        if let Some(at) = restart_at_ms {
            if t0.elapsed().as_millis() >= at {
                console.outln("restarting...");
                esp_hal::system::software_reset();
            }
        }

        // ---- LED: heartbeat / pairing indicator
        led_blink = led_blink.wrapping_add(1);
        let pairing = STATE.lock(|s| s.borrow().pairing);
        let on = if pairing {
            (led_blink / 4000) % 2 == 0 // fast blink while pairing
        } else {
            (led_blink / 40_000) % 2 == 0 // slow heartbeat
        };
        led.set_level(if on { Level::High } else { Level::Low });

        if n == 0 {
            Timer::after_micros(30).await;
        }
    }
}
