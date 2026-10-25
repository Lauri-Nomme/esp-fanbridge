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

use core::sync::atomic::Ordering;

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

mod config;
mod console;
mod led;
mod pairing;
mod panic_log;
mod radio;
mod state;
mod zigbee;

// The application is the only place that needs everything at once: the console
// drives the radio and the state, the Zigbee layer mirrors the state, and main
// wires them together.
use crate::config::*;
use crate::console::*;
use crate::pairing::*;
use crate::radio::*;
use crate::state::*;
use crate::zigbee::*;
use zigbee_cluster_library::types::ZclEnum8;
// `::zigbee` is the upstream stack crate; `crate::zigbee` is our layer on top.
use ::zigbee as zb_stack;

/// A panic is stored before the device stops, so the next boot can report it.
///
/// The console is often the only thing that knows a device in a cabinet rebooted
/// at all, and by the time someone looks the explanation would be gone.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    panic_log::record(UPTIME_MS.load(Ordering::Relaxed), info);
    println!();
    println!("====================== PANIC ======================");
    println!("{info}");
    println!();
    println!("Backtrace:");
    for frame in esp_backtrace::Backtrace::capture().frames() {
        println!("0x{:x}", frame.program_counter());
    }
    loop {
        core::hint::spin_loop();
    }
}
use esp_fanbridge_protocol::FRAME_GAP_US;
use esp_fanbridge_protocol::MAX_SEG;
use esp_fanbridge_protocol::NOISE_US;
use esp_fanbridge_protocol::Seg;
use esp_fanbridge_protocol::decode_frame;

esp_bootloader_esp_idf::esp_app_desc!();

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

    let mut status_led = Output::new(peripherals.GPIO15, Level::Low, OutputConfig::default());

    let cfg = load_config();
    let paired = cfg.slots.iter().filter(|s| s.used()).count();
    let endpoints = build_endpoints(&cfg);
    STATE.lock(|s| s.borrow_mut().cfg = cfg);

    println!("=== esp-fanbridge {VERSION} (built {BUILD_DATE}) ===");
    // the one thing worth knowing after an unplanned reset, printed before
    // anything that could hang. The record is erased from the loop below, not
    // here: an erase this early aborts the chip.
    let mut panic_reported = false;
    {
        let mut text = [0u8; panic_log::TEXT_MAX];
        if let Some((uptime_ms, len)) = panic_log::take(&mut text) {
            // the stored text is "panicked at <where>:\n<message>"; one console
            // line is easier to read and to copy into a bug report
            for b in &mut text[..len] {
                if *b == b'\n' || *b == b'\r' {
                    *b = b' ';
                }
            }
            let reason = core::str::from_utf8(&text[..len]).unwrap_or("<unreadable>");
            println!("last panic after {}s: {reason}", uptime_ms / 1000);
            panic_reported = true;
        }
    }
    let version = cc1101_init(&mut spi, &mut cs, &mut d);
    println!("CC1101 VERSION=0x{version:02x} (want 0x14) at {FREQ_KHZ} kHz");
    enter_rx(&mut spi, &mut cs);
    println!(
        "{paired} remote(s) configured, {n_endpoints} endpoint(s) advertised",
        n_endpoints = endpoints.len()
    );
    if !cfg.device_name.is_empty() {
        println!("device name: {}", cfg.device_name.as_str());
    }

    // ---- Zigbee
    let flash = embassy_embedded_hal::adapter::BlockingAsync::new(esp_storage::FlashStorage::new(
        peripherals.FLASH,
    ));
    let storage = zb_stack::storage::init_with_flash(flash, ZIGBEE_FLASH).await;
    let ieee = esp_radio::ieee802154::Ieee802154::new(peripherals.IEEE802154);
    // `Config::default()` has both acknowledgements off. The association path
    // sets them, but a *resume* - what happens on every normal boot - does not,
    // so the device received and applied commands without ever acknowledging
    // them: the coordinator retried, gave up (`MAC_NO_ACK`) and let the frame
    // expire in its indirect queue, while the command had in fact been executed.
    // That is the "it bounces back" symptom, and it survived every other fix.
    let radio = esp_radio::ieee802154::Config {
        auto_ack_tx: true,
        auto_ack_rx: true,
        ..Default::default()
    };
    let mac = zigbee_mac::esp::EspMlme::new(ieee, radio);
    println!("Zigbee IEEE address: {:#018x}", mac.ieee_address());
    let stack: &'static ZbStack = STACK.init(zb_stack::Stack::new(
        mac,
        stack_config(endpoints),
        Chain,
        storage,
    ));

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
    let mut last_identify_tick_ms: u64 = 0;
    let mut last_rx_check_ms: u64 = 0;
    // set when a pairing changed the endpoint set and a restart is scheduled;
    // a coordinator can also ask for one (the `reboot` command)
    let mut restart_at_ms: Option<u64> = None;
    let mut linebuf = [0u8; 80];
    let mut clock_on = false;
    let mut raw_prev = gdo0.is_high();

    loop {
        // ---- retire the panic record once the scheduler is up
        if panic_reported {
            panic_reported = false;
            panic_log::clear();
        }

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
                        let mut w = Writer {
                            buf: &mut b,
                            pos: 0,
                        };
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
            let rearms = u32::from(REARM_COUNT.load(Ordering::Relaxed) as u16);
            if STAGE_NOW.load(Ordering::Relaxed) != raw
                || PAIRED_NOW.load(Ordering::Relaxed) != count
                || EXPOSED_NOW.load(Ordering::Relaxed) != exposed
                || REARMS_NOW.load(Ordering::Relaxed) != rearms
            {
                REARMS_NOW.store(rearms, Ordering::Relaxed);
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
                    let mut w = Writer80 {
                        buf: &mut b,
                        pos: 0,
                    };
                    let _ = core::fmt::Write::write_fmt(
                        &mut w,
                        format_args!(
                            "tx 0x{code:06X} via {} took {took} ms",
                            if TX_PACKET.load(Ordering::Relaxed) {
                                "packet"
                            } else {
                                "bit-bang"
                            }
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
                let mut w = Writer80 {
                    buf: &mut b,
                    pos: 0,
                };
                let dbm = rssi_dbm(rssi);
                let iocfg0 = rreg(&mut spi, &mut cs, IOCFG0);
                let pktctl0 = rreg(&mut spi, &mut cs, PKTCTRL0);
                let mdmcfg2 = rreg(&mut spi, &mut cs, MDMCFG2);
                let raw = RAW_EDGES.swap(0, Ordering::Relaxed);
                let _ = write!(
                    w,
                    "marcstate=0x{marc:02x} rssi={dbm}dBm iocfg0=0x{iocfg0:02x} pktctrl0=0x{pktctl0:02x} mdmcfg2=0x{mdmcfg2:02x} segs={n} raw={raw} gdo0={}",
                    u8::from(gdo0.is_high())
                );
            }
            let txt = core::str::from_utf8(&b).unwrap_or("");
            console.outln(txt);
        }
        if CLOCK_REQ.swap(false, Ordering::Relaxed) {
            clock_on = !clock_on;
            strobe(&mut spi, &mut cs, SIDLE);
            wreg(
                &mut spi,
                &mut cs,
                IOCFG0,
                if clock_on { 0x39 } else { 0x0D },
            );
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
                let mut w = Writer80 {
                    buf: &mut b,
                    pos: 0,
                };
                let _ = write!(w, "segs n={n}");
            }
            console.outln(core::str::from_utf8(&b).unwrap_or(""));
            for (i, seg) in segs.iter().take(n.min(24)).enumerate() {
                let mut b2 = [0u8; 80];
                {
                    use core::fmt::Write;
                    let mut w = Writer80 {
                        buf: &mut b2,
                        pos: 0,
                    };
                    let _ = write!(
                        w,
                        "  [{i}] {}us {}",
                        seg.dt,
                        if seg.high { "H" } else { "L" }
                    );
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
                    let mut w = Writer80 {
                        buf: &mut b,
                        pos: 0,
                    };
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
                        Ev::Step(step, btn) => {
                            let mut b = [0u8; 80];
                            {
                                use core::fmt::Write;
                                let mut w = Writer80 {
                                    buf: &mut b,
                                    pos: 0,
                                };
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

        // ---- pairing session: start the clock, and give up eventually
        {
            let (pairing, started) = STATE.lock(|s| {
                let st = s.borrow();
                (st.pairing, st.pair_start_ms)
            });
            if pairing && started == 0 {
                STATE.lock(|s| s.borrow_mut().pair_start_ms = now_ms);
            } else if pairing && now_ms.saturating_sub(started) > PAIR_SESSION_MS {
                STATE.lock(|s| {
                    let mut st = s.borrow_mut();
                    st.pairing = false;
                    st.pair_step = 0;
                    st.pair_addr = 0;
                    st.pair_start_ms = 0;
                    st.stage = Stage::Failed;
                    st.stage_until_ms = now_ms + PAIR_RESULT_MS;
                });
                mark_pairing_switch();
                console.outln("pairing timed out (nothing learned)");
            } else if !pairing {
                STATE.lock(|s| s.borrow_mut().pair_start_ms = 0);
            }
        }

        // ---- forget the network on request: the only way to get a fresh
        // child-table entry when a rejoin is refused (DESIGN §9, in one command)
        if LEAVE_REQ.swap(false, Ordering::Relaxed) {
            console.outln("forgetting the network...");
            stack.forget_network().await;
            console.outln("forgotten; rebooting to commission from scratch");
            esp_hal::system::software_reset();
        }

        // ---- re-associate on request: a capability change (e.g. gaining
        // rx-on-when-idle) is only visible to the parent on a fresh association
        if REJOIN_REQ.swap(false, Ordering::Relaxed) {
            console.outln("rejoining: fresh association, same keys...");
            // a rejoin before the device is on the network cannot succeed, and
            // the MacError it returns says nothing useful
            stack.wait_until_joined().await;
            let epid = zigbee_types::IeeeAddress(*zb_stack::nwk::nib::get_ref().extended_panid());
            let outcome = stack.device().rejoin(epid, 11..27, 3).await;
            let mut b = [0u8; 80];
            let pos = {
                use core::fmt::Write;
                let mut w = Writer80 {
                    buf: &mut b,
                    pos: 0,
                };
                let _ = match &outcome {
                    Ok(confirm) => write!(w, "rejoin: status={:?}", confirm.status),
                    Err(e) => write!(w, "rejoin failed: {e:?}"),
                };
                w.pos
            };
            let text = core::str::from_utf8(&b[..pos]).unwrap_or("");
            console.outln(text);
        }

        // ---- apply a pending endpoint change (console pairing or a command)
        let asked = REBOOT_AT_MS.load(Ordering::Relaxed);
        if asked != 0 && now_ms >= u64::from(asked) {
            REBOOT_AT_MS.store(0, Ordering::Relaxed);
            console.outln("restarting (requested)...");
            esp_hal::system::software_reset();
        }
        if let Some(at) = restart_at_ms
            && t0.elapsed().as_millis() >= at
        {
            console.outln("restarting...");
            esp_hal::system::software_reset();
        }

        // ---- LED: heartbeat, pairing, learned/failed, identify
        if now_ms.saturating_sub(last_identify_tick_ms) >= 1_000 {
            last_identify_tick_ms = now_ms;
            IDENTIFY.tick(1);
        }
        let (stage, pairing) = STATE.lock(|s| {
            let st = s.borrow();
            (st.stage_at(now_ms), st.pairing)
        });
        let pattern = led::pattern(stage, pairing, IDENTIFY.is_identifying());
        status_led.set_level(if led::is_on(pattern, now_ms) {
            Level::High
        } else {
            Level::Low
        });

        if n == 0 {
            Timer::after_micros(30).await;
        }
    }
}
