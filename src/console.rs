//! The USB serial console: the operator interface.
//!
//! Everything a human can type lives here - listing remotes, pairing, sending
//! raw codes, the diagnostics - so the rest of the firmware can stay quiet about
//! how it is driven.

use core::sync::atomic::Ordering;

use esp_hal::usb_serial_jtag::UsbSerialJtag;

use crate::zigbee::*;
use zigbee_cluster_library::server::ClusterServer;
use zigbee_cluster_library::types::AttributeId;
use crate::config::save_config;
use crate::config::Name;
use crate::config::NAME_MAX;
use crate::config::Slot;
use crate::pairing::*;
use crate::radio::*;
use crate::state::*;

pub struct Console<'d> {
    usb: UsbSerialJtag<'d, esp_hal::Blocking>,
    line: [u8; 64],
    n: usize,
}

impl<'d> Console<'d> {
    pub fn new(usb: UsbSerialJtag<'d, esp_hal::Blocking>) -> Self {
        Self {
            usb,
            line: [0; 64],
            n: 0,
        }
    }

    pub fn out(&mut self, s: &str) {
        let _ = self.usb.write(s.as_bytes());
    }

    pub fn outln(&mut self, s: &str) {
        self.out(s);
        self.out("\r\n");
    }

    pub fn poll(&mut self) -> Option<[u8; 64]> {
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

pub fn cmd_str(line: &[u8; 64]) -> &str {
    let end = line.iter().position(|&c| c == 0).unwrap_or(64);
    core::str::from_utf8(&line[..end]).unwrap_or("")
}

/// Copy a name out of the shared state (a `&str` can not escape the lock).
pub fn copy_name(name: Name) -> [u8; NAME_MAX] {
    let mut buf = [0u8; NAME_MAX];
    let bytes = name.as_str().as_bytes();
    buf[..bytes.len()].copy_from_slice(bytes);
    buf
}

pub fn fmt_name<'a>(b: &'a mut [u8; 96], text: &[u8; NAME_MAX], slot: Option<usize>) -> &'a str {
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

pub fn print_list(c: &mut Console<'_>) {
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

pub struct Writer<'a> {
    pub buf: &'a mut [u8; 64],
    pub pos: usize,
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

pub fn format_slot<'a>(b: &'a mut [u8; 96], i: usize, sl: &Slot) -> &'a str {
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

pub fn handle_command(c: &mut Console<'_>, line: &str) {
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
            c.outln("  identify <secs>      blink the LED like the coordinator's identify");
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
        "identify" => {
            // start the same identification a coordinator can request
            match it.next().and_then(|v| v.parse::<u16>().ok()) {
                Some(secs) if secs > 0 && secs <= 300 => {
                    IDENTIFY.set_identify_time(secs);
                    c.outln("identifying (LED blinks fast)");
                }
                Some(0) => {
                    IDENTIFY.set_identify_time(0);
                    c.outln("identify stopped");
                }
                _ => c.outln("usage: identify <seconds> (0 stops)"),
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

pub fn fmt_rx<'a>(b: &'a mut [u8; 80], addr: u32, button: u8, code: u32, rssi_dbm: i32) -> &'a str {
    use core::fmt::Write;
    let mut w = Writer80 { buf: b, pos: 0 };
    let _ = write!(
        w,
        "rx device=0x{addr:05X} button=0x{button:X} code=0x{code:06X} rssi={rssi_dbm}dBm"
    );
    let n = w.pos;
    core::str::from_utf8(&b[..n]).unwrap_or("")
}


/// Render a command that arrived over Zigbee, naming the slot it controls.
pub fn fmt_zb_event<'a>(b: &'a mut [u8; 80], ev: ZbEvent) -> &'a str {
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

pub fn fmt2<'a>(b: &'a mut [u8; 80], a: u32, x: u32) -> &'a str {
    use core::fmt::Write;
    let mut w = Writer80 { buf: b, pos: 0 };
    let _ = write!(w, "paired remote 0x{a:05X} into slot {x}");
    let n = w.pos;
    core::str::from_utf8(&b[..n]).unwrap_or("")
}

pub struct Writer96<'a> {
    pub buf: &'a mut [u8; 96],
    pub pos: usize,
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

pub struct Writer80<'a> {
    pub buf: &'a mut [u8; 80],
    pub pos: usize,
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


