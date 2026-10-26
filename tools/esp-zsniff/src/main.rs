//! `esp-zsniff` — a minimal IEEE 802.15.4 sniffer for Zigbee debugging.
//!
//! Prints one line per frame with what can be read without the keys: the frame
//! length, RSSI, MAC header (sequence number, destination/source) and, for
//! NWK-secured frames, the security frame counter — which is what a coordinator
//! uses to reject replays. Payloads are encrypted and stay opaque.
//!
//! Used to answer questions like "is that reply frame on the air at all?".
//!
//! Usage: flash, then set `CHANNEL` (default 25) to the network's channel.

#![no_std]
#![no_main]

use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::ieee802154::Config;
use esp_radio::ieee802154::Ieee802154;

esp_bootloader_esp_idf::esp_app_desc!();

/// The Zigbee channel to listen on.
const CHANNEL: u8 = 25;

#[esp_rtos::main]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);
    // the radio driver allocates its queue
    esp_alloc::heap_allocator!(size: 16 * 1024);

    let mut radio = Ieee802154::new(peripherals.IEEE802154);
    radio.set_config(Config {
        channel: CHANNEL,
        promiscuous: true, // every frame on the channel, not just ours
        auto_ack_rx: false, // a sniffer must not acknowledge
        auto_ack_tx: false,
        rx_when_idle: true,
        rx_queue_size: 32,
        ..Default::default()
    });
    radio.start_receive();
    println!("zsniff on channel {CHANNEL}");

    loop {
        let Some(raw) = radio.raw_received() else {
            core::hint::spin_loop();
            continue;
        };
        // raw.data[0] is the length, the last byte is the RSSI (see
        // `esp_radio`'s `received()` for the same convention)
        let data = &raw.data;
        let len = (data[0] as usize).min(data.len() - 2);
        if len < 5 {
            continue;
        }
        let frame = &data[1..][..len];
        let rssi = data[data.len() - 1] as i8;

        // 802.15.4 data frame, no PAN-ID compression assumed:
        //   fcf(2) seq(1) dstpan(2) dst(2) [srcpan(2)] src(2) ...
        let fcf = u16::from_le_bytes([frame[0], frame[1]]);
        let seq = frame[2];
        let dst = u16::from_le_bytes([frame[5], frame[6]]);
        let src = u16::from_le_bytes([frame[7], frame[8]]);

        // NWK layer: fcf(2) dst(2) src(2) radius(1) seq(1), then the security
        // header when the security bit is set: control(1) frame counter(4)
        // key sequence(1)
        let nwk = &frame[9..];
        let mut counter = 0u32;
        let mut nwk_seq = 0u8;
        let mut secured = false;
        if nwk.len() >= 8 {
            let nwk_fcf = u16::from_le_bytes([nwk[0], nwk[1]]);
            nwk_seq = nwk[6];
            secured = nwk_fcf & 0x0200 != 0;
            if secured && nwk.len() >= 9 {
                counter = u32::from_le_bytes([nwk[7], nwk[8], nwk[9], nwk[10]]);
            }
        }
        let nwk_fcf = if nwk.len() >= 2 {
            u16::from_le_bytes([nwk[0], nwk[1]])
        } else {
            0
        };
        println!(
            "rx len={len} fcf={fcf:04x} seq={seq} dst={dst:04x} src={src:04x} nwkfcf={nwk_fcf:04x} srcref={} nwkseq={nwk_seq} sec={} fc={counter} rssi={rssi}",
            u8::from(nwk_fcf & 0x0400 != 0),
            u8::from(secured)
        );
    }
}
