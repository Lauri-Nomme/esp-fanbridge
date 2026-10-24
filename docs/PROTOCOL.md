# VT100 fan remote — reverse engineering & clone

Complete record of how three 433 MHz ceiling-fan remotes were captured, decoded
and (partly) cloned with an **ESP32-C6 + CC1101**, written so the whole process
can be repeated for any similar remote.

**Status: fully decoded, replay verified as a technique, all 3 remotes
identified.** The fan itself responded to replayed frames.

---

## 1. TL;DR / the result

| Remote | 20-bit device address | Notes |
|--------|----------------------|-------|
| Remote 1 | **`0x1A5F0`** | |
| Remote 2 | **`0x06800`** | |
| Remote 3 | **`0x191D0`** | |

**All three remotes use the identical button mapping.** Only the address
differs, so one ESP32-C6 + CC1101 can drive all three fans by sending the right
address with the right button nibble.

| Function | Button nibble | Remote 1 code | Remote 2 code | Remote 3 code |
|----------|---------------|---------------|---------------|---------------|
| speed 1 | `0x2` | `0x1A5F02` | `0x068002` | `0x191D02` |
| speed 2 | `0x1` | `0x1A5F01` | `0x068001` | `0x191D01` |
| speed 3 | `0xC` | `0x1A5F0C` | `0x06800C` | `0x191D0C` |
| speed 4 | `0x7` | `0x1A5F07` | `0x068007` | `0x191D07` |
| direction in | `0x8` | `0x1A5F08` | `0x068008` | `0x191D08` |
| direction out | `0x4` | `0x1A5F04` | `0x068004` | `0x191D04` |
| direction in/out | `0x3` | `0x1A5F03` | `0x068003` | `0x191D03` |
| power | `0xA` | `0x1A5F0A` | `0x06800A` | `0x191D0A` |

Codes used: `1 2 3 4 7 8 A C`. Unused: `0 5 6 9 B D E F`.

---

## 2. The protocol (complete spec)

Everything needed to decode or transmit:

| Property | Value |
|----------|-------|
| Carrier | **433.92 MHz** |
| Modulation | **ASK / OOK** |
| Encoding | **PWM, 24 bits**, MSB first |
| Symbol "0" | high **400 µs** + low **1200 µs** |
| Symbol "1" | high **1200 µs** + low **400 µs** |
| Symbol ratio | 1 : 3 |
| Bit period | ~1.6 ms |
| Frame | 24 bits + a **400 µs trailing high** |
| Frame length | **~38.7 ms** |
| Frame repeat | repeated continuously while a button is held (~8–15×) |
| Inter-frame gap | **~12 ms** (not measured exactly — see §7) |
| Payload split | **20-bit address + 4-bit button code** |
| Code type | **fixed / non-rolling** → replayable |

This is the classic **EV1527** family layout (also seen as PT2262-style PWM).
Decoded word = `(address << 4) | button`.

Raw example — one captured frame (durations in µs):

```
384,1190,399,1193,413,1190,1187,397,1219,402,390,1193,1207,397,397,1207,
397,1197,1210,375,406,1210,1207,384,1194,407,1216,381,1194,413,1187,406,
402,1187,424,1187,397,1206,407,1184,406,1183,425,1191,1209,375,419,1193,403
```

24 pairs + trailing 403 µs = `0x1A5F02` (speed 1, remote 1).

### Reference waveform

The numbers above are nominal. This is what a real remote put on the air, from
the A/B capture (202 frames, `n=49` segments each, sampled by `esp-ookcapture`
at 8 µs):

| | nominal | measured | spread over 24 pulses | mean |
|---|---|---|---|---|
| short pulse | 400 µs | **375–425 µs** | 50 µs | 400.1 µs |
| long pulse | 1200 µs | **1183–1219 µs** | 36 µs | 1197.5 µs |
| trailing high | 400 µs | 403 µs | - | - |
| frame (49 segments) | 38.8 ms | **38.75 ms** | - | - |
| inter-frame gap | ~12 ms | ~12 ms | - | - |

So the remote is within ±6 % of nominal and the *ratio* is what matters, not the
absolute width. Our own transmitter is within a few microseconds of these values
(bit-bang: the same delays in software; packet mode: 100.07 µs per bit, see
[`PACKET-TX.md`](PACKET-TX.md)), and the decoder's acceptance window is
deliberately wide around them:

| Check | Window |
|---|---|
| short symbol | 250–650 µs |
| long symbol | 850–1800 µs |
| pairs per frame | exactly 24, each one short and one long |
| frame total | 33–45 ms |

A frame that decodes but whose pulses sit near a window edge is a receiver
problem (level, distance, antenna), not a decoding one. `dump` prints the last
capture's raw edges, and `rx …` prints the decoded code with its RSSI - comparing
those two against this table is the fastest way to tell "our TX is wrong" from
"the fan is not listening".

---

## 3. Hardware

| Part | Notes |
|------|-------|
| **DFRobot Beetle ESP32-C6** (SKU DFR1117) | ESP32-C6FH4, 13 IO, castellated/through-hole pads |
| **CC1101 module**, blue PCB, "43M", 26.000 MHz xtal, SMA | 433 MHz variant |
| 7× male–female Dupont jumpers | |

### Wiring (verified working)

| CC1101 pin | Signal | Beetle pad |
|---|---|---|
| 1 | `GND` | `GND` (left edge 1st) |
| 2 | `VCC` | `3V3` (left edge 2nd) |
| 3 | `GDO0` | `IO5` |
| 4 | `CSN` | `IO4` |
| 5 | `SCK` | `IO23` |
| 6 | `MOSI` | `IO22` |
| 7 | `MISO` | `IO21` |
| 8 | `GDO2` | *not connected* |

Pin numbers are the ones silkscreened on the CC1101 module; the two header
columns are numbered `2,4,6,8` and `1,3,5,7`. **`VCC` is pin 2 → `3V3`, never
`VIN` (5 V) — the CC1101 is not 5 V tolerant.**

### The contact problem (the single biggest time sink)

The Beetle ships with **bare plated through-holes**. A 0.64 mm header pin in a
~1.0 mm hole is a *loose* fit — it looks connected but isn't. Symptoms seen:
every pad reading "floating", one pin flickering 0/1, bridges producing no
effect, and the chip never answering.

**Fix that actually worked:** bending/angling the pin strip so the pins wedge
against the hole walls (increased friction). Later: proper soldering.

Lesson: *plated through-holes are for soldering to, not for friction-fit.*

---

## 4. The tools we built (all in `firmware/examples/`)

| Example | Purpose | Why it exists |
|---------|---------|---------------|
| `esp-cc1101-test` | Reads `PARTNUM`/`VERSION`, write-readback on `SYNC1` | proves power + SPI + CS wiring |
| `esp-wirecheck` | Rail scan (pull-up/pull-down per pad), **brute-forces all pin→role assignments**, then a bridge test | diagnoses wiring with **no multimeter** |
| `esp-rfscan` | RSSI sweep 430–435 MHz | first attempt at finding the remote |
| `esp-ookcapture` | Raw edge-timestamped OOK capture, glitch-absorbing filter, burst splitting | to see the actual waveform |
| `esp-fandecode` | **Live decoder** — prints one line per physical press | the tool you actually want day-to-day |
| `esp-rfplay` | OOK transmitter — replays a decoded code | cloning |

Key techniques inside them:

* **`esp-wirecheck` brute force** — instead of trusting a wiring table, it tries
  every 4-of-N assignment of pads to `CSN/SCK/MOSI/MISO` with bit-banged SPI and
  reports which one makes the chip answer. This proves the wiring *whatever* got
  swapped, and separates "wrong mapping" from "no contact / no power".
* **Pull test with a control pin** — read each pad with an internal pull-up then
  pull-down. A pin that reads low *both* ways is grounded; high both ways is tied
  to 3V3; anything else is high-Z. **A control pin that cannot be wired (GPIO2,
  not broken out on the Beetle) is essential** — without it, a broken pull-up
  config looks like "everything is grounded".
* **Glitch absorption** — the CC1101's async output chatters with no carrier.
  Instead of resetting on short pulses, transitions shorter than `NOISE_US` are
  ignored and the segment keeps running, so real pulse widths survive.
* **Frame splitting by silence** — a gap longer than `FRAME_GAP_US` ends a frame.
  Too small → frames chopped; too large → frames merge across seconds. 6 ms was
  right here.

---

## 5. How the decode was done, step by step

### 5.1 Prove the radio works
`esp-cc1101-test` → `PARTNUM=0x00 VERSION=0x14 SYNC1 0x5a -> 0x5a`.
**Always do this first.** `VERSION=0x14` = healthy CC1101. Anything else is
wiring/power, not protocol.

### 5.2 Find the frequency — what did *not* work
An RSSI sweep changing frequency every 1.5 ms produced only noise-looking peaks
(−64…−67 dBm scattered over 430–435 MHz). **The RSSI never settles when the
sweep keeps retuning the PLL/AGC**, so that method was abandoned.

### 5.3 Park on 433.92 MHz and watch raw data
`esp-ookcapture` with `GDO0` as *serial data output* turned the CC1101 into a
simple OOK demodulator. Heartbeats (`N raw edges/s, M of them >100us`) proved the
receiver was alive and distinguished real pulses from noise.

### 5.4 The **A/B method** (the trick that solved it)
Capturing blindly was hopeless: there is a **strong ambient 433 MHz device**
transmitting every 1–2 s (`n=46` segments, `total≈36.7 ms`) plus other traffic.

Solution — one capture, two halves:

1. **First ~35 s: touch nothing** → baseline of ambient traffic (11 bursts).
2. **Then press buttons with the remote ~5 cm from the antenna** → (303 bursts).

Diffing the halves made the remote's frames pop out instantly: 202 frames of
`n=49, total≈38.7 ms`, and their contents were two-valued (**~400 µs** and
**~1200 µs**) — a textbook OOK PWM code.

### 5.5 Decode
Pairing the segments (`high, low`) and classifying each pair as "short then long"
= `0` or "long then short" = `1` gave 24 well-formed bits per frame. Across 202
frames only **4 distinct values**, all sharing `0x1A5F0` → a **20-bit address +
4-bit button**, fixed (not rolling).

### 5.6 Confirm button order
Collapsing consecutive repeats and looking at the time order:

```
t=43s 0x1A5F02   t=44s 0x1A5F01   t=45s 0x1A5F0C   t=45s 0x1A5F07
t=46s 0x1A5F02   t=47s 0x1A5F01   ...
```

The 1,2,3,4 press sequence mapped exactly onto the four codes.

### 5.7 Live decoder
`esp-fandecode` validates every frame (24 pairs, each exactly one short + one
long, symbol bounds 250–650 / 850–1800 µs, total 33–45 ms), dedupes repeats over
800 ms, and prints **one line per press**:

```
[   12.345] device=0x1A5F0 button=0x2 code=0x1A5F02
```

That produced the full table for all three remotes in one session.

### 5.8 Replay
`esp-rfplay` drives `GDO0` as an output, puts the CC1101 in async TX
(`PKTCTRL0=0x32`, `MDMCFG2=0x30` ASK/OOK, `PATABLE[0]=0x00`, `PATABLE[1]=0xC0`),
strobes `STX`, then bit-bangs 24 symbols with 400/1200 µs delays, a 400 µs
trailing pulse, and a 12 ms gap, repeated 8×.

---

## 6. Gotchas worth remembering

* **Plated through-holes ≠ sockets.** Friction-fit is unreliable; symptom is
  "everything reads floating".
* **The CC1101's `GDO0` is noisy with no carrier.** Always filter by pulse width,
  never by edge count alone.
* **RSSI is useless during a fast sweep** — park on a frequency instead.
* **Frames get chopped/merged by the wrong gap threshold.** Look for gaps of
  multi-second length in the output = threshold too big.
* **A tty splits bytes between readers.** If a `python` capture and `minicom` are
  open at once, neither sees everything. One reader at a time.
* **Ambient 433 MHz traffic is everywhere** (sensors, remotes). The before/after
  diff, or a control experiment, is the only reliable way to attribute a burst.
* **The BOOT button worked as a marker in theory** (`esp-ookcapture` prints
  `##### MARK`), but it wasn't pressed in the test — the A/B method needs no
  cooperation from the hardware.
* **Printing stalls the capture loop.** A burst print can take milliseconds at
  115200 baud; that shows up as absurd "durations" (e.g. `25817032`). Always
  discard the first measurement after a print (a `resync` flag).

---

## 7. What is still open

1. **Exact inter-frame gap** — 12 ms is an estimate. Re-capture with a larger
   `BURST_GAP_US` to measure it; only matters if the fan is picky.
2. **Only button 1 was replayed** so far; the other seven are trivially added
   (`CODES[]` in `esp-rfplay`).
3. **All three fans from one chip** — send `address[i] | button` for the target
   fan. This is the obvious next feature.
4. **Home Assistant integration** — the C6 already runs the Zigbee light
   (`esp-led`); merge the CC1101 TX into it and expose "fan N: speed/direction/
   power" as Zigbee clusters, or bridge via MQTT.

---

## 8. Reproduce it quickly

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cd ~/c6/zigbee-rs/examples/<example>
CARGO_TARGET_DIR=$HOME/c6/shared-target cargo build --release --features esp32c6
sudo espflash flash --port /dev/ttyACM0 \
     $HOME/c6/shared-target/riscv32imac-unknown-none-elf/release/<example>
```

Order of operations: `esp-cc1101-test` → (`esp-wirecheck` if anything misbehaves)
→ `esp-fandecode` → `esp-rfplay`.

To watch the decoder yourself:

```sh
sudo minicom -D /dev/ttyACM0 -b 115200     # Ctrl-A W to wrap, Ctrl-A X to exit
```
