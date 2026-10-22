//! The CC1101: register access, receive mode and the two transmitters.
//!
//! Timing-sensitive work (driving `GDO0` per edge) lives here next to the
//! packet-mode transmitter so the main loop stays readable. Why both exist is
//! explained in `docs/PACKET-TX.md`.

use core::sync::atomic::Ordering;

use embassy_time::Timer;
use esp_hal::delay::Delay;
use esp_hal::gpio::Flex;
use esp_hal::gpio::Output;
use esp_hal::spi::master::Spi;
use esp_hal::time::Instant;

use esp_fanbridge_protocol::expand_code;
use esp_fanbridge_protocol::TX_PACKET_LEN;

use crate::state::TX_GAP_MS;
use crate::state::TX_REPEATS;

pub const READ_BURST: u8 = 0xC0;
pub const WRITE_BURST: u8 = 0x40;

pub const IOCFG2: u8 = 0x00;
pub const IOCFG0: u8 = 0x02;
pub const FIFOTHR: u8 = 0x03;
pub const PKTCTRL1: u8 = 0x07;
pub const PKTLEN: u8 = 0x06;
pub const SFTX: u8 = 0x3B;
pub const SCAL: u8 = 0x33;
pub const PKTCTRL0: u8 = 0x08;
pub const FSCTRL1: u8 = 0x0B;
pub const FREQ2: u8 = 0x0D;
pub const FREQ1: u8 = 0x0E;
pub const FREQ0: u8 = 0x0F;
pub const MDMCFG4: u8 = 0x10;
pub const MDMCFG3: u8 = 0x11;
pub const MDMCFG2: u8 = 0x12;
pub const MDMCFG1: u8 = 0x13;
pub const MDMCFG0: u8 = 0x14;
pub const DEVIATN: u8 = 0x15;
pub const MCSM2: u8 = 0x16;
pub const MCSM1: u8 = 0x17;
pub const MCSM0: u8 = 0x18;
pub const FOCCFG: u8 = 0x19;
pub const BSCFG: u8 = 0x1A;
pub const AGCCTRL2: u8 = 0x1B;
pub const AGCCTRL1: u8 = 0x1C;
pub const AGCCTRL0: u8 = 0x1D;
pub const FREND1: u8 = 0x21;
pub const FREND0: u8 = 0x22;
pub const FSCAL3: u8 = 0x23;
pub const FSCAL2: u8 = 0x24;
pub const FSCAL1: u8 = 0x25;
pub const FSCAL0: u8 = 0x26;
pub const PATABLE: u8 = 0x3E;

pub const SRES: u8 = 0x30;
pub const SIDLE: u8 = 0x36;
pub const SRX: u8 = 0x34;
pub const STX: u8 = 0x35;

pub const FREQ_KHZ: u32 = 433_920;


/// Symbol widths used for transmit.
pub const T_SHORT: u32 = 400;
pub const T_LONG: u32 = 1200;
pub const FRAME_GAP_MS: u32 = 12;
pub const REPEATS: u32 = 8;


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


pub fn wreg(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, addr: u8, val: u8) {
    cs.set_low();
    let mut b = [addr & 0x3F, val];
    let _ = spi.transfer(&mut b);
    cs.set_high();
}

pub fn rstat(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, addr: u8) -> u8 {
    cs.set_low();
    let mut b = [READ_BURST | (addr & 0x3F), 0x00];
    let _ = spi.transfer(&mut b);
    cs.set_high();
    b[1]
}

pub fn rreg(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, addr: u8) -> u8 {
    cs.set_low();
    let mut b = [0x80 | (addr & 0x3F), 0x00];
    let _ = spi.transfer(&mut b);
    cs.set_high();
    b[1]
}

pub fn strobe(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, cmd: u8) {
    cs.set_low();
    let mut b = [cmd, 0x00];
    let _ = spi.transfer(&mut b);
    cs.set_high();
}

pub fn set_freq_khz(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, khz: u32) {
    let f = ((khz as u64) * 65536) / 26_000;
    wreg(spi, cs, FREQ2, (f >> 16) as u8);
    wreg(spi, cs, FREQ1, (f >> 8) as u8);
    wreg(spi, cs, FREQ0, f as u8);
}

pub fn write_patable(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, p: [u8; 2]) {
    cs.set_low();
    let mut b = [WRITE_BURST | (PATABLE & 0x3F), p[0], p[1]];
    let _ = spi.transfer(&mut b);
    cs.set_high();
}

pub fn cc1101_init(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, d: &mut Delay) -> u8 {
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

pub fn enter_rx(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>) {
    strobe(spi, cs, SIDLE);
    strobe(spi, cs, SRX);
}

pub async fn send_code(
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

/// Data rate for 100 us per bit (26 MHz crystal): 403 * 256 * 26e6 / 2^28.
pub const TX_DRATE_E: u8 = 8;
pub const TX_DRATE_M: u8 = 147;

/// Registers for packet transmission: OOK, no preamble/sync, fixed length,
/// no CRC, no whitening.
pub const PKT_TX_REGS: &[(u8, u8)] = &[
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
pub const ASYNC_RX_REGS: &[(u8, u8)] = &[
    (PKTCTRL1, 0x04),
    (PKTCTRL0, 0x32),
    (PKTLEN, 0xFF),
    (MDMCFG4, 0x87),
    (MDMCFG3, 0x83),
    (MDMCFG2, 0x30),
];



pub fn apply_regs(spi: &mut Spi<'_, esp_hal::Blocking>, cs: &mut Output<'_>, regs: &[(u8, u8)]) {
    for &(a, v) in regs {
        wreg(spi, cs, a, v);
    }
}

/// Transmit `code` eight times using the radio's own packet engine.
///
/// Returns how long the whole burst took, for the debug log.
pub async fn send_code_packet(
    spi: &mut Spi<'_, esp_hal::Blocking>,
    cs: &mut Output<'_>,
    code: u32,
) -> u32 {
    let frame = expand_code(code);
    // +1: the first STX after the async→packet switch does not go out
    send_packet_frame(spi, cs, &frame, REPEATS + 1).await
}

/// Enter packet mode, send `frame` `repeats` times, restore async RX.
pub async fn send_packet_frame(
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


/// CC1101 RSSI register (0x34) to dBm, as in the datasheet.
pub fn rssi_dbm(raw: u8) -> i32 {
    if raw >= 128 {
        (i32::from(raw) - 256) / 2 - 74
    } else {
        i32::from(raw) / 2 - 74
    }
}
