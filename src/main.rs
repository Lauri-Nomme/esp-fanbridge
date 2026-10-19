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
use zigbee_cluster_library::clusters::general::basic::BasicServer;
use zigbee_cluster_library::clusters::general::identify;
use zigbee_cluster_library::clusters::general::identify::IdentifyServer;
use zigbee_cluster_library::frame::Status;
use zigbee_cluster_library::profile;
use zigbee_cluster_library::reporting::AttributeReporting;
use zigbee_cluster_library::reporting::ReportingTable;
use zigbee_cluster_library::server::ClusterCommand;
use zigbee_cluster_library::server::ClusterServer;
use zigbee_cluster_library::server::CommandOutcome;
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
use zigbee_types::IeeeAddress;

esp_bootloader_esp_idf::esp_app_desc!();

// ------------------------------------------------------------ CC1101 registers
const READ_BURST: u8 = 0xC0;
const WRITE_BURST: u8 = 0x40;

const IOCFG2: u8 = 0x00;
const IOCFG0: u8 = 0x02;
const FIFOTHR: u8 = 0x03;
const PKTCTRL1: u8 = 0x07;
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
    (AGCCTRL1, 0x40),
    (AGCCTRL0, 0x91),
    (FREND1, 0x56),
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

#[derive(Clone, Copy, Default)]
pub struct Slot {
    /// 20-bit device address; 0 means empty
    pub addr: u32,
    /// 0 = unknown/off, 1..=4
    pub speed: u8,
    /// 0 = in, 1 = out
    pub dir: u8,
    pub power: bool,
}

impl Slot {
    pub fn used(&self) -> bool {
        self.addr != 0
    }
}

#[derive(Clone, Copy)]
pub struct Config {
    pub magic: u32,
    pub version: u32,
    pub slots: [Slot; 8],
    pub checksum: u32,
}

impl Config {
    pub const EMPTY: Config = Config {
        magic: CFG_MAGIC,
        version: 1,
        slots: [Slot {
            addr: 0,
            speed: 0,
            dir: 0,
            power: false,
        }; 8],
        checksum: 0,
    };

    fn bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..4].copy_from_slice(&self.magic.to_le_bytes());
        b[4..8].copy_from_slice(&self.version.to_le_bytes());
        let mut o = 8;
        for s in &self.slots {
            b[o..o + 4].copy_from_slice(&s.addr.to_le_bytes());
            b[o + 4] = s.speed;
            b[o + 5] = s.dir;
            b[o + 6] = u8::from(s.power);
            b[o + 7] = 0;
            o += 8;
        }
        b[72..76].copy_from_slice(&self.checksum.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; 128]) -> Option<Config> {
        let magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if magic != CFG_MAGIC {
            return None;
        }
        let version = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let mut slots = [Slot::default(); 8];
        let mut o = 8;
        for s in slots.iter_mut() {
            s.addr = u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) & 0x000F_FFFF;
            s.speed = b[o + 4];
            s.dir = b[o + 5];
            s.power = b[o + 6] != 0;
            o += 8;
        }
        let stored = u32::from_le_bytes([b[72], b[73], b[74], b[75]]);
        let mut c = Config {
            magic,
            version,
            slots,
            checksum: 0,
        };
        if c.compute_checksum() != stored {
            return None;
        }
        c.checksum = stored;
        Some(c)
    }

    fn compute_checksum(&self) -> u32 {
        let b = self.bytes();
        let mut sum = 0u32;
        for (i, v) in b[..72].iter().enumerate() {
            sum = sum.wrapping_mul(31).wrapping_add(u32::from(*v) + i as u32);
        }
        sum ^ 0xA5A5_5A5A
    }
}

/// Read the config sector. Returns the empty config if nothing valid is stored.
fn load_config() -> Config {
    let mut buf = [0u32; 32];
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
    let bytes: &[u8; 128] = unsafe { &*(buf.as_ptr() as *const [u8; 128]) };
    Config::from_bytes(bytes).unwrap_or(Config::EMPTY)
}

/// Erase the sector and write the config back.
fn save_config(cfg: &Config) -> bool {
    let mut c = *cfg;
    c.magic = CFG_MAGIC;
    c.checksum = c.compute_checksum();
    let mut buf = [0u32; 32];
    {
        let bytes: &mut [u8; 128] = unsafe { &mut *(buf.as_mut_ptr() as *mut [u8; 128]) };
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
}

static STATE: CsMutex<CriticalSectionRawMutex, RefCell<State>> =
    CsMutex::new(RefCell::new(State {
        cfg: Config::EMPTY,
        pairing: false,
        pair_addr: 0,
        pair_step: 0,
        pair_last_ms: 0,
    }));

/// Pending transmit request: 24-bit code, 0 = none.
static TX_REQ: AtomicU32 = AtomicU32::new(0);
/// Debug requests handled by the main loop (which owns the SPI + segments).
static DUMP_REQ: AtomicBool = AtomicBool::new(false);
static STAT_REQ: AtomicBool = AtomicBool::new(false);
static CLOCK_REQ: AtomicBool = AtomicBool::new(false);
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

fn send_code(
    spi: &mut Spi<'_, esp_hal::Blocking>,
    cs: &mut Output<'_>,
    gdo0: &mut Flex<'_>,
    d: &mut Delay,
    code: u32,
) {
    // GDO0 becomes the TX data input in async mode
    gdo0.set_low();
    gdo0.set_input_enable(false);
    gdo0.set_output_enable(true);

    strobe(spi, cs, SIDLE);
    write_patable(spi, cs, [0x00, 0xC0]);
    strobe(spi, cs, STX);
    d.delay_millis(2);

    for _ in 0..REPEATS {
        for i in (0..24).rev() {
            if (code >> i) & 1 == 1 {
                gdo0.set_high();
                d.delay_micros(T_LONG);
                gdo0.set_low();
                d.delay_micros(T_SHORT);
            } else {
                gdo0.set_high();
                d.delay_micros(T_SHORT);
                gdo0.set_low();
                d.delay_micros(T_LONG);
            }
        }
        gdo0.set_high();
        d.delay_micros(T_SHORT);
        gdo0.set_low();
        d.delay_millis(FRAME_GAP_MS);
    }

    // back to receive
    gdo0.set_output_enable(false);
    gdo0.set_input_enable(true);
    enter_rx(spi, cs);
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
        BTN_DIR_TOGGLE => slot.dir ^= 1,
        BTN_POWER => slot.power = !slot.power,
        _ => {}
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

fn print_list(c: &mut Console<'_>) {
    STATE.lock(|s| {
        let st = s.borrow();
        c.outln("slot  address    speed  dir    power");
        for i in 0..8 {
            let sl = &st.cfg.slots[i];
            if sl.used() {
                let mut b = [0u8; 64];
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

fn format_slot<'a>(b: &'a mut [u8; 64], i: usize, sl: &Slot) -> &'a str {
    use core::fmt::Write;
    let mut w = Writer { buf: b, pos: 0 };
    let _ = write!(
        w,
        "  {}   0x{:05X}    {}     {}     {}",
        i,
        sl.addr,
        sl.speed,
        if sl.dir == 0 { "in" } else { "out" },
        if sl.power { "on" } else { "off" }
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
            c.outln("  dir <slot> <in|out>  send a direction command");
            c.outln("  power <slot> <on|off>");
            c.outln("  raw <hex>            transmit a raw 24-bit code");
            c.outln("  stat                 chip state + RSSI");
            c.outln("  desc                 show the advertised endpoint descriptors");
            c.outln("  dump                 show the last captured edges");
            c.outln("  clock                toggle GDO0 = XOSC/192 clock (pin self-test)");
            c.outln("  save                 force save config");
        }
        "list" => print_list(c),
        "pair" => {
            STATE.lock(|s| {
                let mut st = s.borrow_mut();
                st.pairing = true;
                st.pair_step = 0;
                st.pair_addr = 0;
                st.pair_last_ms = 0;
            });
            c.outln("pairing: press SPEED 1, then 2, then 3, then 4 on the remote");
        }
        "cancel" => {
            STATE.lock(|s| s.borrow_mut().pairing = false);
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
                }
            }
        }
        "speed" => {
            let a = it.next().and_then(|v| v.parse::<usize>().ok());
            let sp = it.next().and_then(|v| v.parse::<u8>().ok());
            if let (Some(a), Some(sp)) = (a, sp) {
                if a < 8 {
                    if let Some(btn) = speed_button(sp) {
                        let code = STATE.lock(|s| {
                            let st = s.borrow();
                            code_for(&st.cfg.slots[a], btn)
                        });
                        TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
                        c.outln("sent");
                    } else {
                        c.outln("speed must be 1..4");
                    }
                }
            }
        }
        "dir" => {
            let a = it.next().and_then(|v| v.parse::<usize>().ok());
            let dv = it.next().unwrap_or("");
            if let Some(a) = a {
                let btn = match dv {
                    "in" => Some(BTN_DIR_IN),
                    "out" => Some(BTN_DIR_OUT),
                    _ => None,
                };
                if let (true, Some(btn)) = (a < 8, btn) {
                    let code = STATE.lock(|s| {
                        let st = s.borrow();
                        code_for(&st.cfg.slots[a], btn)
                    });
                    TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
                    c.outln("sent");
                }
            }
        }
        "power" => {
            let a = it.next().and_then(|v| v.parse::<usize>().ok());
            let pv = it.next().unwrap_or("");
            if let Some(a) = a {
                if a < 8 && (pv == "on" || pv == "off") {
                    let btn = STATE.lock(|s| {
                        let st = s.borrow();
                        let sl = &st.cfg.slots[a];
                        if pv == "off" {
                            some_if(sl.power, BTN_POWER)
                        } else {
                            some_if(!sl.power, speed_button(if sl.speed == 0 { 1 } else { sl.speed }).unwrap_or(BTN_SPEED1))
                        }
                    });
                    if let Some(b) = btn {
                        let code = STATE.lock(|s| code_for(&s.borrow().cfg.slots[a], b));
                        TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
                        c.outln("sent");
                    } else {
                        c.outln("already in that state");
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
            for ep in ENDPOINTS.iter() {
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
        "stat" => {
            STAT_REQ.store(true, Ordering::Relaxed);
        }
        "debug" => {
            let on = !DEBUG_ON.load(Ordering::Relaxed);
            DEBUG_ON.store(on, Ordering::Relaxed);
            c.outln(if on { "debug on" } else { "debug off" });
        }
        "clock" => {
            CLOCK_REQ.store(true, Ordering::Relaxed);
        }
        "save" => {
            let ok = STATE.lock(|s| save_config(&s.borrow().cfg));
            c.outln(if ok { "saved" } else { "save failed" });
        }
        "" => {}
        _ => c.outln("unknown command (type help)"),
    }
}

fn fmt_rx<'a>(b: &'a mut [u8; 80], addr: u32, button: u8, code: u32) -> &'a str {
    use core::fmt::Write;
    let mut w = Writer80 { buf: b, pos: 0 };
    let _ = write!(w, "rx device=0x{addr:05X} button=0x{button:X} code=0x{code:06X}");
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
                        st.cfg.slots[i] = Slot {
                            addr,
                            speed: 0,
                            dir: 0,
                            power: false,
                        };
                        save_config(&st.cfg);
                        st.pairing = false;
                        st.pair_step = 0;
                        st.pair_addr = 0;
                        return Ev::Paired(addr, i);
                    } else {
                        st.pairing = false;
                        st.pair_step = 0;
                        st.pair_addr = 0;
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
        for sl in st.cfg.slots.iter_mut() {
            if sl.used() && sl.addr == addr {
                apply_button(sl, button);
                changed = true;
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
const EP_DIR0: u8 = 10; // ..17

const CL_ONOFF: Cluster = Cluster::new(ClusterId(0x0006), "On/Off");
const CL_FAN: Cluster = Cluster::new(ClusterId(0x0202), "Fan Control");
const CL_LEVEL: Cluster = Cluster::new(ClusterId(0x0008), "Level Control");
const A_ONOFF: Attribute<Bool, ReadOnly, Reportable> =
    CL_ONOFF.attribute(AttributeId(0x0000), "OnOff");
const A_LEVEL: Attribute<Uint8, ReadWrite, Reportable> =
    CL_LEVEL.attribute(AttributeId(0x0000), "CurrentLevel");
const A_FANMODE: Attribute<Enum8<FanMode>, ReadWrite, Reportable> =
    CL_FAN.attribute(AttributeId(0x0000), "FanMode");
static ONOFF_ATTRS: [AttrInfo; 1] = [A_ONOFF.attr_info()];
/// Shared reporting store: enough to accept Configure Reporting from the coordinator.
static REPORTING: ReportingTable<16> = ReportingTable::new();
static LEVEL_ATTRS: [AttrInfo; 1] = [A_LEVEL.attr_info()];
static FAN_ATTRS: [AttrInfo; 1] = [A_FANMODE.attr_info()];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FanMode {
    Off,
    Low,
    Medium,
    High,
}

impl ZclEnum8 for FanMode {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(FanMode::Off),
            1 => Some(FanMode::Low),
            2 => Some(FanMode::Medium),
            3 => Some(FanMode::High),
            _ => None,
        }
    }
    fn into_raw(self) -> u8 {
        match self {
            FanMode::Off => 0,
            FanMode::Low => 1,
            FanMode::Medium => 2,
            FanMode::High => 3,
        }
    }
}

fn speed_of_mode(m: FanMode) -> u8 {
    match m {
        FanMode::Off => 0,
        FanMode::Low => 1,
        FanMode::Medium => 2,
        FanMode::High => 3,
    }
}

fn mode_of_speed(speed: u8) -> FanMode {
    match speed {
        1 => FanMode::Low,
        2 => FanMode::Medium,
        3 | 4 => FanMode::High,
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
pub struct OnOffEp<const EP: u8, const SLOT: i8, const KIND: u8>; // KIND 0=power 1=direction 2=pair

impl<const EP: u8, const SLOT: i8, const KIND: u8> OnOffEp<EP, SLOT, KIND> {
    fn get(&self) -> bool {
        STATE.lock(|s| {
            let st = s.borrow();
            match KIND {
                0 => st.cfg.slots[SLOT as usize].power,
                1 => st.cfg.slots[SLOT as usize].dir == 1,
                _ => st.pairing,
            }
        })
    }
    fn set(&self, on: bool) {
        match KIND {
            2 => {
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
                    sl.power = KIND == 0 && on;
                    if KIND == 1 {
                        sl.dir = u8::from(on);
                    }
                    let snap = st.cfg;
                    drop(st);
                    save_config(&snap);
                    let sl = &snap.slots[SLOT as usize];
                    let btn = if KIND == 0 {
                        if on {
                            speed_button(if sl.speed == 0 { 1 } else { sl.speed }).unwrap_or(BTN_SPEED1)
                        } else {
                            BTN_POWER
                        }
                    } else if on {
                        BTN_DIR_OUT
                    } else {
                        BTN_DIR_IN
                    };
                    code_for(sl, btn)
                });
                if code != 0 {
                    TX_REQ.store(code | 0x8000_0000, Ordering::Relaxed);
                }
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
        let mode = STATE.lock(|s| mode_of_speed(s.borrow().cfg.slots[SLOT].speed));
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

// ---- identity per endpoint
macro_rules! basic_for {
    ($name:ident, $model:literal) => {
        pub static $name: BasicServer<'static> = BasicServer {
            zcl_version: 8,
            application_version: 1,
            stack_version: 0,
            hw_version: 1,
            manufacturer_name: "zigbee-rs",
            model_identifier: $model,
            power_source: 0x01,
        };
    };
}
basic_for!(BASIC_MASTER, "fan-bridge");
basic_for!(BASIC_F0, "fan-1");
basic_for!(BASIC_F1, "fan-2");
basic_for!(BASIC_F2, "fan-3");
basic_for!(BASIC_F3, "fan-4");
basic_for!(BASIC_F4, "fan-5");
basic_for!(BASIC_F5, "fan-6");
basic_for!(BASIC_F6, "fan-7");
basic_for!(BASIC_F7, "fan-8");

pub static IDENTIFY: IdentifyServer = IdentifyServer::new();

// ---- endpoint table
static IN_MASTER: [u16; 3] = [basic::CLUSTER_ID, identify::CLUSTER_ID, 0x0006];
static IN_FAN: [u16; 2] = [basic::CLUSTER_ID, 0x0202];
static IN_DIR: [u16; 1] = [0x0006];
static NO_CLUSTERS: [u16; 0] = [];

static ENDPOINTS: [EndpointDescriptor; 17] = [
    EndpointDescriptor { endpoint: 1, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_MASTER, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 2, profile_id: profile::HOME_AUTOMATION, device_id: 0x002B, device_version: 1, input_clusters: &IN_FAN, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 3, profile_id: profile::HOME_AUTOMATION, device_id: 0x002B, device_version: 1, input_clusters: &IN_FAN, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 4, profile_id: profile::HOME_AUTOMATION, device_id: 0x002B, device_version: 1, input_clusters: &IN_FAN, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 5, profile_id: profile::HOME_AUTOMATION, device_id: 0x002B, device_version: 1, input_clusters: &IN_FAN, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 6, profile_id: profile::HOME_AUTOMATION, device_id: 0x002B, device_version: 1, input_clusters: &IN_FAN, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 7, profile_id: profile::HOME_AUTOMATION, device_id: 0x002B, device_version: 1, input_clusters: &IN_FAN, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 8, profile_id: profile::HOME_AUTOMATION, device_id: 0x002B, device_version: 1, input_clusters: &IN_FAN, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 9, profile_id: profile::HOME_AUTOMATION, device_id: 0x002B, device_version: 1, input_clusters: &IN_FAN, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 10, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_DIR, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 11, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_DIR, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 12, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_DIR, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 13, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_DIR, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 14, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_DIR, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 15, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_DIR, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 16, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_DIR, output_clusters: &NO_CLUSTERS },
    EndpointDescriptor { endpoint: 17, profile_id: profile::HOME_AUTOMATION, device_id: 0x0000, device_version: 1, input_clusters: &IN_DIR, output_clusters: &NO_CLUSTERS },
];

pub static PAIR_SW: OnOffEp<1, -1, 2> = OnOffEp;
pub static PWR0: OnOffEp<2, 0, 0> = OnOffEp;
pub static PWR1: OnOffEp<3, 1, 0> = OnOffEp;
pub static PWR2: OnOffEp<4, 2, 0> = OnOffEp;
pub static PWR3: OnOffEp<5, 3, 0> = OnOffEp;
pub static PWR4: OnOffEp<6, 4, 0> = OnOffEp;
pub static PWR5: OnOffEp<7, 5, 0> = OnOffEp;
pub static PWR6: OnOffEp<8, 6, 0> = OnOffEp;
pub static PWR7: OnOffEp<9, 7, 0> = OnOffEp;
pub static DIR0: OnOffEp<10, 0, 1> = OnOffEp;
pub static DIR1: OnOffEp<11, 1, 1> = OnOffEp;
pub static DIR2: OnOffEp<12, 2, 1> = OnOffEp;
pub static DIR3: OnOffEp<13, 3, 1> = OnOffEp;
pub static DIR4: OnOffEp<14, 4, 1> = OnOffEp;
pub static DIR5: OnOffEp<15, 5, 1> = OnOffEp;
pub static DIR6: OnOffEp<16, 6, 1> = OnOffEp;
pub static DIR7: OnOffEp<17, 7, 1> = OnOffEp;
pub static FAN0: FanEp<0> = FanEp;
pub static FAN1: FanEp<1> = FanEp;
pub static FAN2: FanEp<2> = FanEp;
pub static FAN3: FanEp<3> = FanEp;
pub static FAN4: FanEp<4> = FanEp;
pub static FAN5: FanEp<5> = FanEp;
pub static FAN6: FanEp<6> = FanEp;
pub static FAN7: FanEp<7> = FanEp;

/// Dispatches to all cluster servers in turn (trait objects avoid a 40-deep tuple).
pub struct Chain;

impl ClusterRequestHandler for Chain {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        const SRV: [&dyn ClusterRequestHandler; 35] = [
            &BASIC_MASTER, &IDENTIFY, &PAIR_SW,
            &BASIC_F0, &FAN0, &PWR0, &DIR0,
            &BASIC_F1, &FAN1, &PWR1, &DIR1,
            &BASIC_F2, &FAN2, &PWR2, &DIR2,
            &BASIC_F3, &FAN3, &PWR3, &DIR3,
            &BASIC_F4, &FAN4, &PWR4, &DIR4,
            &BASIC_F5, &FAN5, &PWR5, &DIR5,
            &BASIC_F6, &FAN6, &PWR6, &DIR6,
            &BASIC_F7, &FAN7, &PWR7, &DIR7,
        ];
        SRV.iter().find_map(|h| h.handle(r, out))
    }
}

pub type ZbFlash = zigbee::storage::FlashStorage<embassy_embedded_hal::adapter::BlockingAsync<esp_storage::FlashStorage<'static>>>;
pub type ZbStack = zigbee::Stack<'static, zigbee_mac::esp::EspMlme<'static>, Chain, ZbFlash>;
pub static STACK: static_cell::StaticCell<ZbStack> = static_cell::StaticCell::new();

fn stack_config() -> StackConfig<'static> {
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
            endpoints: &ENDPOINTS,
        },
    )
}

#[embassy_executor::task]
async fn stack_task(stack: &'static ZbStack) {
    let outcome = stack.run(AsyncDelay).await;
    println!("zigbee stack stopped: {outcome:?}");
    esp_hal::system::software_reset();
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
    STATE.lock(|s| s.borrow_mut().cfg = cfg);

    println!("=== esp-fanbridge ===");
    let version = cc1101_init(&mut spi, &mut cs, &mut d);
    println!("CC1101 VERSION=0x{version:02x} (want 0x14) at {FREQ_KHZ} kHz");
    enter_rx(&mut spi, &mut cs);
    println!("{paired} remote(s) configured");

    // ---- Zigbee
    let flash = embassy_embedded_hal::adapter::BlockingAsync::new(esp_storage::FlashStorage::new(peripherals.FLASH));
    let storage = zigbee::storage::init_with_flash(flash, ZIGBEE_FLASH).await;
    let ieee = esp_radio::ieee802154::Ieee802154::new(peripherals.IEEE802154);
    let mac = zigbee_mac::esp::EspMlme::new(ieee, esp_radio::ieee802154::Config::default());
    println!("Zigbee IEEE address: {:#018x}", mac.ieee_address());
    let stack: &'static ZbStack = STACK.init(zigbee::Stack::new(mac, stack_config(), Chain, storage));

    spawner.spawn(stack_task(stack).expect("spawn stack_task"));

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
    let mut linebuf = [0u8; 80];
    let mut clock_on = false;
    let mut raw_prev = gdo0.is_high();

    loop {
        // ---- console
        if let Some(line) = console.poll() {
            let s = cmd_str(&line);
            handle_command(&mut console, s);
        }

        // ---- pending transmit
        let req = TX_REQ.swap(0, Ordering::Relaxed);
        if req != 0 {
            let code = req & 0x00FF_FFFF;
            send_code(&mut spi, &mut cs, &mut gdo0, &mut d, code);
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
                let dbm = if rssi >= 128 { (rssi as i32 - 256) / 2 - 74 } else { (rssi as i32) / 2 - 74 };
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
                    console.outln(fmt_rx(&mut linebuf, addr, button, bits));
                    match on_frame(addr, button, now) {
                        Ev::Paired(a, i) => {
                            console.outln(fmt2(&mut linebuf, a, i as u32));
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
