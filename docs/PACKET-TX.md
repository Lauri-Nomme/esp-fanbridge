# Transmitting the fan protocol with the CC1101's packet engine

The firmware started out bit-banging the 433 MHz waveform: the MCU toggled
`GDO0` for every edge and timed the pulses with a busy loop. This note explains
what the radio can do instead, why the protocol does not map onto it directly,
and how the two are bridged with a bit-level re-encoding.

Everything below is one branch: `feature/cc1101-packet-tx`. The path is opt-in
(`txmode packet`, default is still the bit-bang transmit) until it has been
confirmed against the physical fans.

---

## 1. The protocol, as the remotes speak it

433.92 MHz, ASK/OOK, 24 bits, fixed code (`address << 4 | button`). There is no
preamble, no sync word, no CRC, no length field — just a burst of pulse-width
coded symbols and then silence.

A symbol is **one high pulse followed by one low pulse**:

```
    bit 0                                   bit 1
    ┌───┐                                   ┌───────────┐
    │   │                                   │           │
────┘   └───────────────              ──────┘           └───
    400µs      1200µs                      1200µs    400µs
    └──────── 1600 µs ────────┘             └────── 1600 µs ──────┘
```

24 symbols = 38.4 ms, plus one trailing 400 µs pulse = **38.8 ms** per frame.
The firmware repeats the frame 8 times with a 12 ms gap:

```
┌─────────┐ 12ms ┌─────────┐ 12ms ┌─────────┐            ┌─────────┐
│ frame 1 │      │ frame 2 │      │ frame 3 │  ......    │ frame 8 │
└─────────┘      └─────────┘      └─────────┘            └─────────┘
 38.8ms           38.8ms           38.8ms                 38.8ms
```

≈ 400 ms of activity per button press.

---

## 2. What the CC1101 does on its own

The CC1101 has two very different ways to get bits on the air.

### 2.1 Asynchronous serial mode — what the firmware uses today

```
   MCU                                   CC1101
    │                                      │
    │  GDO0 ────── raw OOK envelope ─────▶ │ modulator → PA → antenna
    │  (firmware toggles it; widths are     │
    │   whatever the firmware manages)      │
```

`PKTCTRL0 = 0x32` selects `PKT_FORMAT = 3` (async serial). In this mode the
packet engine, the FIFO, the sync-word matcher and the CRC are **all bypassed**:
the chip only keys the PA from the level on `GDO0`. Every microsecond of timing
is the MCU's problem, which is why the transmit used to be a ~400 ms busy loop
(and why the Zigbee task starved — see commit `63977d4`).

The same pin and the same mode are used for RX: the firmware samples `GDO0`
every ~30 µs and rebuilds the edges in software.

### 2.2 Packet mode — what the chip can do by itself

```
   MCU                                   CC1101
    │  15/49 bytes over SPI into TXFIFO   │
    │ ──────────────────────────────────▶ │
    │                                     │ adds preamble?/sync?/CRC?
    │   STX                               │ clocks the bits out at the
    │ ──────────────────────────────────▶ │ configured data rate, OOK
    │                                     │ modulates, keys the PA
    │   (MCU is free for ~39 ms)          │
    │                                     │
    │  MARCSTATE == IDLE                  │
    │ ◀────────────────────────────────── │
```

This is the mode the question is about: you hand the radio bytes, it does the
timing. The catch is **what those bytes mean**:

* The radio sends them as **NRZ at a fixed bit rate** — a `1` is carrier on for
  one bit period, a `0` is carrier off.
* It expects an optional preamble and a 16/32-bit sync word in front (both
  switchable off), and can add CRC and whitening.
* It has no notion of "pulse width". Our `0`/`1` are *widths*, not levels.

So a straight hand-off is impossible: 400/1200 µs pulses are not bits.

---

## 3. The bridge: sample the waveform on a 100 µs grid

The trick is that the protocol's two pulses are integer multiples of a common
unit:

```
400 µs  = 4 units
1200 µs = 12 units
symbol  = 16 units        (1600 µs)
```

Pick **one bit = one unit = 100 µs** (≈ 10 kbaud) and a PWM symbol becomes 16
plain NRZ bits:

```
 bit 0  (400 high, 1200 low)          bit 1  (1200 high, 400 low)
 ┌───┐                                ┌───────────┐
 │   │                                │           │
─┘   └───────────────                 ┘           └────
 1111 000000000000                    111111111111 0000
 └──┬──┘└─────┬─────┘                 └─────┬──────┘└─┬──┘
   4 bits    12 bits                       12 bits   4 bits
```

The mapping is exact — not an approximation. The fan sees the same 400/1200 µs
pulses it always did, because 4 × 100 µs = 400 µs and 12 × 100 µs = 1200 µs.

### 3.1 Frame layout

```
 24 symbols × 16 bits                      + trailing pulse
┌────────────────────────────────────────┐ ┌────────┐
│ 384 bits (48 bytes)                    │ │ 1111   │  4 bits
└────────────────────────────────────────┘ └────────┘
                                             padding to a byte boundary
                                             (zeros = silence)
total: 392 bits = 49 bytes
```

Each remote bit, MSB first, becomes 16 stream bits:

| remote bit | stream bits | on air |
|---|---|---|
| `0` | `1111000000000000` | 400 µs on, 1200 µs off |
| `1` | `1111111111110000` | 1200 µs on, 400 µs off |

### 3.2 Worked example

Code `0x068002` (address `0x06800`, speed-1 button `0x2`), 24 bits MSB first:

```
0x068002 = 0000 0110 1000 0000 0000 0010
           ││││ ││││ ││││ ││││ ││││ ││││
           └─┬─┘ └─┬─┘ ...
             │     │
             │     └─ 1 ─▶ 1111111111110000
             └─────── 0 ─▶ 1111000000000000
```

The resulting 49-byte packet (this is what goes into the TX FIFO):

```
F0 00 F0 00 F0 00 F0 00 F0 00 FF F0 FF F0 F0 00 FF F0 F0 00
F0 00 F0 00 F0 00 F0 00 F0 00 F0 00 F0 00 F0 00 F0 00 F0 00
F0 00 F0 00 F0 00 F0 00 FF F0 F0 00 F0
```

Read the first two bytes: `F0 00` = `1111 0000 0000 0000` = 4 on, 12 off = the
remote's `0` symbol. Each byte boundary lands exactly on a 100 µs unit, so the
stream never needs to be byte-aligned to a symbol — it just is.

---

## 4. Registers

Only six registers differ between "async RX" and "packet TX":

| register | async RX (and bit-bang TX) | packet TX | why |
|---|---|---|---|
| `PKTCTRL0` | `0x32` (`PKT_FORMAT=3` async, infinite length) | `0x00` (packet, fixed length, no CRC, no whitening) | use the FIFO/engine |
| `PKTCTRL1` | `0x04` | `0x00` | no append-status, no autoflush |
| `PKTLEN` | `0xFF` | `49` | fixed packet length |
| `MDMCFG4` | `0x87` (`DRATE_E=7`) | `0x88` (`DRATE_E=8`) | bit rate →
| `MDMCFG3` | `0x83` (`DRATE_M=131`) | `147` (`0x93`) | 100 µs per bit |
| `MDMCFG2` | `0x30` | `0x30` (`MOD_FORMAT=ASK/OOK`, `SYNC_MODE=0`, no Manchester) | OOK, **no preamble/sync** |

`FREND0 = 0x11` and `PATABLE = [0x00, 0xC0]` are unchanged: for OOK the PA
power index selects the "carrier on" level, and the "off" level is no carrier.

Everything else (frequency, AGC, filter, output power) is untouched.

### 4.1 Data rate

The CC1101 computes the bit rate from its 26 MHz crystal:

```
R = (256 + DRATE_M) · 2^DRATE_E · f_XTAL / 2^28
  = (256 + 147) · 2^8 · 26e6 / 2^28
  = 403 · 256 · 26e6 / 268435456
  = 9992.6 baud
```

→ **100.07 µs per bit**, 0.07 % slower than nominal. Over a 49-byte frame that
is a 0.03 ms error on 39.2 ms — far inside the wide decoder window of both the
fans and the firmware's own receiver (250–650 µs / 850–1800 µs).

### 4.2 Timing budget

| step | duration |
|---|---|
| one frame (392 bits × 100.07 µs) | 39.2 ms |
| inter-frame gap | 12 ms |
| 8 frames + 7 gaps | 8·39.2 + 7·12 = **397.6 ms** |
| CC1101 hands-on time (MCU blocked) | ~0 |

Compare with the bit-bang path: the same ~398 ms of wall clock, but the MCU is
busy for all of it, and the symbol edges are at the mercy of the scheduler.

---

## 5. The transmit sequence

```
            ┌──────────────────────────────────────────────┐
            │                 packet TX                    │
  async RX  │                                              │ async RX
 ────┬──────┴──┬───────┬────────┬────────┬────────┬────────┴──┬─────
     │ SIDLE   │ regs  │ SFTX   │ FIFO   │  STX   │  ×8       │ regs
     │         │ OOK   │ flush  │ write  │        │  + 12 ms  │ async
     │         │ 49B   │        │ 49 B   │        │  gaps     │ + SRX
     └──────────────────────────────────────────────────────────
                                    │
                       wait 40 ms ──┘
                       poll MARCSTATE until IDLE (≤50 ms)
```

Per repeat the firmware writes the 49 bytes as one SPI burst into `TXFIFO`
(`0x3F | WRITE_BURST`), strokes `STX`, awaits 40 ms, then waits for `MARCSTATE`
to return to `IDLE` before the next frame. After the eighth frame the async RX
registers are restored and the radio returns to `SRX`.

The MCU is idle during each 39 ms transmission, so the Zigbee task keeps running
— the reason `tx-done timeout` warnings appear on the bit-bang path is gone
entirely.

---

## 6. Status: transmits correctly, opt-in

The packet path is verified on air against a second C6+CC1101 listening in async
mode:

```
packet:   406,1162,448,1158,444,1137,442,1174,425,1156,1249,347,1252,366,...
bit-bang: 449,1207,484,1186,481,1186,478,1213,...
```

Same symbols, same order, same 24-bit code; the packet frame is ~38.8 ms versus
~40.6 ms for the bit-bang frame, because the radio clocks it from its crystal
instead of inheriting the scheduler's jitter.

Getting there required one register fix. The first attempts produced a 44 ms
*constant carrier* for every payload:

```
MDMCFG2 (0x12)   7   6..4        3             2..0
                DCFILT  MOD_FORMAT  MANCHESTER_EN  SYNC_MODE
```

`MOD_FORMAT` is **bits 6:4**, not 2:0. Writing `0x02` therefore selected
*2-FSK with a 16/16 sync word*: a constant-envelope carrier, plus a frame 48
bits longer than the payload (4-byte preamble + 2-byte sync) — both symptoms
match exactly. `0x30` is what this firmware already uses for async OOK and is
also correct here: `MOD_FORMAT=011`, no Manchester, `SYNC_MODE=000`.

### 6.1 Known limitation: the first packet after a mode switch is lost

The radio does not transmit the first `STX` after switching from async RX to
packet TX; `SCAL` and a 1 ms settle do not change that. The firmware therefore
sends one extra frame per command (`REPEATS + 1`), which is why `txmode packet`
still delivers the intended number of frames.

### 6.2 Why it is still opt-in

The frames are correct and the CPU is free while they are on air, but they have
not been confirmed against every fan: one fan accepted a packet-mode command,
the other never did (its own remote works, so its receiver is simply pickier).
Until that is settled, `txmode packet` switches at runtime and the default stays
the proven bit-bang path.

## 7. Why this is opt-in

* The bit-bang path is proven against three physical fans; the packet path is
  verified only by timing (the radio reports the frame finished in the expected
  ~39 ms) until someone confirms a fan actually reacts.
* `txmode` switches at runtime, so the two can be compared without a reflash:

```
> txmode
tx mode: async (firmware drives GDO0 per edge)
> txmode packet
tx mode: packet (radio sends the expanded bitstream)
> speed 0 3
sent
```

With `debug on`, each transmit also logs what it did:

```
tx 0x068002 via packet took 424 ms
tx 0x068002 via bit-bang took 0 ms
```

(the bit-bang path reports 0 because it does not measure itself).

---

## 8. Can the radio also *receive* this way?

Short answer: **not in packet mode, but yes in a different hardware mode.**

### 7.1 Why packet-mode RX does not work here

Packet RX needs to know where a packet starts. It gets that from the sync word:

```
   ...silence...  preamble  sync  payload  CRC   →  FIFO
                     ▲       ▲
                     └───────┴── the chip can only frame on these
```

Our remotes send neither preamble nor sync word — the frame starts with the
first address bit. With `SYNC_MODE=0` the chip begins filling the FIFO the
moment RX is enabled, i.e. at an arbitrary bit offset, and because the demodulator
has no signal to lock onto during the 6+ ms of silence before a press, the bits
it produces there are noise, not zeros.

So every captured "packet" would be shifted by an unknown number of bits and
padded with garbage. You *could* hunt for the frame afterwards (the frame is
self-delimiting once you have the right alignment), but you would be doing
software framing anyway — with the extra cost of losing the raw edges.

### 7.2 What would work: synchronous serial RX

`PKT_FORMAT = 1` (synchronous serial) makes the chip output the demodulated bit
stream **with a clock**:

```
   antenna → demodulator ─┬─ GDO2: bit clock (~10 kHz)
                          └─ GDO0: data
                                  │
   MCU: on every clock edge, shift one bit into a buffer
```

That is still software framing (no sync word), but the timing comes from the
crystal instead of a 30 µs polling loop: bit-accurate sampling, no jitter, and
the MCU only wakes on the clock edges. It is the natural next step for the
receive path, and it pairs with the packet transmitter since both are "the radio
does the bit timing, the firmware does the framing".

It is *not* implemented here: the current receive path is proven, and the
benefit (marginal sensitivity/robustness) has not been demonstrated to be worth
the rewrite. The important part is that the answer is not "never" — it is "not
with the packet engine, because this protocol has no sync word to frame on".

---

## 8. Running the experiment: one fan at a time

The mode used to be global, which made every packet-mode test also a test of
"what breaks when the other two fans lose their working path". It is per slot now:

```
txmode                    default and the eight overrides (def/pkt/asy)
txmode packet             make packet the default (as before)
txmode 3 packet           slot 3 -> packet engine, this boot only
txmode 3 async            slot 3 -> bit-bang, this boot only
```

Procedure, per fan, ten seconds each:

1. `txmode <slot> async` and send a speed the fan is *not* on. It moves - that
   is the control.
2. `txmode <slot> packet` and send another speed. Write down whether it moves.
3. Repeat each three times. One success is not "works": `lr-r` accepted a
   packet-mode command once and never again.

A power cycle puts every override back to `def`, so a fan left in a broken mode
cannot survive a reboot.

### 8.1 What actually differs between the two paths

| | bit-bang (async) | packet engine |
|---|---|---|
| symbol timing | `Timer::after_micros` per edge, scheduler jitter of tens of us | 10 kbaud from the crystal, 100.07 us per unit |
| before the first bit | GDO0 held low for 2 ms with the PA already up | starts right after the TX ramp (tens of us) |
| repeats | 8 frames, 12 ms gap | 9 frames (`REPEATS + 1`: the first `STX` after the switch never goes out), ~40 ms wait + 12 ms gap |
| framing | none | none (`SYNC_MODE=0`), but the chip's own PA ramp and FIFO timing shape the burst |

The timing is the *better* one in packet mode, and `lr-r` still refuses it - so
timing accuracy is not the variable. The strongest remaining explanation is the
second row: `lr-r`'s receiver may need a quiet period (or a first "wake" pulse)
before the code, and the packet path cannot produce one - the OOK framing this
protocol uses has no preamble, and `SYNC_MODE` has nothing to hang one on.

If that is the cause, packet mode is simply not available for that receiver, and
the honest answer is the per-fan profile (#18) rather than a fix: keep the
bit-bang path for the picky fan and use the packet engine where it works.

### 8.2 Result (2026-10-30): `lr-r` refuses the packet engine, `lr-l` accepts it

Run with the per-slot switch, same command (`speed <slot> 4`), same radio, same
repeat count:

| slot | fan | mode | console | fan obeyed |
|---|---|---|---|---|
| 0 | `lr-r` | bit-bang | `tx 0x191D02 via bit-bang` | yes (control, 2/2) |
| 0 | `lr-r` | packet | `tx 0x191D07 via packet took 480 ms` | **no (0/2)** |
| 1 | `lr-l` | packet | `tx 0x068007 via packet took 480 ms` | **yes (1/1)** |

Three independent `lr-r` packet attempts across two builds, zero reactions, while
the bit-bang path works every time and `lr-l` accepts the identical packet frame.

So the firmware, the per-slot flag and the radio are not the problem: that
receiver does not accept the packet engine's waveform. Timing accuracy is
excluded as the variable (packet mode is the *more* accurate of the two), which
leaves the burst envelope - most plausibly the first symbol, where the packet
path has the PA ramping during the first 400 us pulse while the bit-bang path
holds the PA up for 2 ms first. Every packet frame starts cold; every bit-bang
frame starts with the PA already on.

Confirming that would need an RF measurement of the first pulse at the receiver,
and it would change nothing: packet mode offers no measured benefit over the
proven path, so the answer is **keep `async`** and do not build the per-fan
profile (#18) for its own sake. The switch stays because it is how this question
gets re-asked cheaply if a receiver is ever replaced.

### 8.2.1 The repeat run that nearly produced a false conclusion

One async command at 25 % -> 100 % was **also** ignored that night: `speed_2`
became 4 on the device, `tx_dropped` stayed 0, and the fan did not move. That made
the mode attribution unsafe, so both paths were re-run as repeat batches six
seconds apart, same command each time:

| mode | batches | landed |
|---|---|---|
| packet | 6 (2 + 4) | **0** - 54 frames |
| async | 5 | 4 (the fifth was sent while the fan was already at the target) |

Six packet batches and not one reaction confirms the mode difference. But the
single async failure is a second, independent problem: an occasional burst is
simply not obeyed. That one matters more, because nothing in the system can see
it - the device transmits, the queue is healthy, and the fan does not listen.
Tracked in backlog #59.

### 8.3 Recording the result

The console prints one line per transmission:

```
tx 0x191D07 via packet took 39 ms
tx 0x191D07 via bit-bang took 0 ms
```

so the mode is visible next to every command, and `rx` lines from our own burst
(inside the self-reception window) now carry the level we arrived at, which is
the only measurement of our own transmit power the device can make.
