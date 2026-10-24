# Wiring

One ESP32-C6, one CC1101 module, ten jumpers. Everything here is about not
destroying the radio on the first try - the firmware side is the easy part.

## Parts

| Part | Notes |
|---|---|
| DFRobot Beetle ESP32-C6 (SKU DFR1117) | 13 I/O, LED on `IO15`, BOOT on `IO9`, USB-C |
| CC1101 module, blue PCB, marked "43M", 26.000 MHz crystal | the **433 MHz** variant, SMA + antenna. The 868/915 MHz variants exist and are the wrong part |
| 10× male-female and/or female-female Dupont jumpers | see the breadboard trap below |
| Soldering iron | the Beetle ships with **bare plated through-holes** - see [contact](#the-contact-problem) |

## Pin map

Pin numbers are the ones silkscreened on the CC1101 module. The two header
columns are numbered `2,4,6,8` and `1,3,5,7`, so **count, do not guess from
position**.

| CC1101 pin | Signal | ESP32-C6 (Beetle) | Required |
|---|---|---|---|
| 1 | `GND` | `GND` | yes |
| 2 | `VCC` | **`3V3`** | yes |
| 3 | `GDO0` | `IO5` | yes (data in/out) |
| 4 | `CSN` | `IO4` | yes |
| 5 | `SCK` | `IO23` | yes |
| 6 | `MOSI` | `IO22` | yes |
| 7 | `MISO` | `IO21` | yes |
| 8 | `GDO2` | `IO6` | optional, only for `clock`/debug |

```
        CC1101                         Beetle ESP32-C6
   ┌───────────────┐                   ┌───────────────┐
   │ 1  GND        ├──── GND ─────────┤ GND           │
   │ 2  VCC        ├──── 3V3 ─────────┤ 3V3   (never  │
   │ 3  GDO0       ├──── data ────────┤ IO5    VIN!)  │
   │ 4  CSN        ├──── select ──────┤ IO4           │
   │ 5  SCK        ├──── clock ───────┤ IO23          │
   │ 6  MOSI       ├──── mosi ────────┤ IO22          │
   │ 7  MISO       ├──── miso ────────┤ IO21          │
   │ 8  GDO2       ├──── optional ────┤ IO6           │
   └───────────────┘                   └───────────────┘
          │ antenna (SMA)
```

The Beetle's `IO4/IO5` are Arduino `A3/A4`, and `IO21/IO22/IO23` print as
`MISO/MOSI/SCK`. Only the `IOxx` number matters here.

## The two ways to destroy it

**1. 5 V.** `VCC` (pin 2) goes to `3V3`, never `VIN` - the CC1101 is not 5 V
tolerant on any pin, and `VIN` is the USB rail. Check this first, every time.

**2. The breadboard trap.** A breadboard joins all five holes of a column into
one node. The CC1101's 2×4 header has its two rows one pitch apart, so both rows
land in the *same* five-hole group: `VCC` gets tied to `CSN`, `GND` to `SCK` and
so on. Plug it in only if you have a **2×4 female socket**; otherwise run
female-female jumpers straight from the Beetle's pins to the module's pins and
let the breadboard hold nothing but the Beetle.

The safe alternative: solder wires to *one* 4-pin row and insert only the other
row, provided the four inserted pins sit in the same breadboard row but four
different columns. Support the module - four pins are not enough to hold it.

## The contact problem

The Beetle ships with bare plated through-holes, and a 0.64 mm header pin in a
1.0 mm hole is a *loose* fit: it looks connected and is not. The symptoms are
maddening - a pad that reads floating, one line that flickers, a chip that never
answers SPI. Bending the pin strip so the pins wedge against the hole walls gets
you through bring-up; soldering is the real fix. This is the single biggest time
sink in the whole build - [`PROTOCOL.md`](PROTOCOL.md#3-hardware) has the story.

## Power-up checklist

1. Board unplugged. Every jumper checked against the table above.
2. `VCC` → `3V3`, `GND` → `GND`; no jumper bridging two pads; nothing touching
   the antenna side.
3. Antenna screwed on before transmitting. Without it the PA has nowhere to put
   its power.
4. Plug USB in and watch the console (115200 baud).
5. `stat` is the bring-up check - these are the values the firmware programs at
   boot:

   ```
   marcstate=0x0d rssi=-95dBm iocfg0=0x0d pktctrl0=0x32 mdmcfg2=0x30 segs=0 raw=0 gdo0=1
   ```

   `marcstate=0x0d` is RX, `iocfg0=0x0d` is "GDO0 carries serial data", and
   `mdmcfg2=0x30` is ASK/OOK with no sync word. If `marcstate` sits at `0x00` or
   `mdmcfg2` reads back something else, the chip is not answering - go back to
   the contact problem and the `VCC` check. A garbage `iocfg0` with a plausible
   `marcstate` is usually one marginal SPI line (normally `MISO`).
6. `status` should show `433920 kHz`, the transmit mode and the watchdog
   counters. `rearm` climbing while nothing is being received is normal; it means
   the radio dropped out of RX and was put back.

## Antenna and range

A 17 cm piece of wire is a quarter-wave on 433 MHz and is enough for a fan
remote across a room; the SMA whip that comes with the module is better. Keep the
antenna vertical and away from the ESP32's own antenna - the CC1101 will happily
hear its own board otherwise, and a burst of 2.4 GHz noise next to the receiver
shows up as garbage edges.
