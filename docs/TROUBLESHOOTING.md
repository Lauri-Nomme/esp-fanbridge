# Troubleshooting

Debugging order that actually works: **is the bridge alive → is the radio alive →
does the RF part work → does Zigbee reach it**. Each step is one command on the
console, and each has a value you can point at.

```
status    uptime, counters, mode, frequency, pairing
stat      CC1101 registers, RSSI, edge counts
desc      the endpoints this device advertises
list      the learned remotes
slots     the raw slots (also shows a half-empty one)
dump      the last captured edge timings
```

Symptom → section:

| What you see | Go to |
|---|---|
| nothing on the console | [1](#1-no-console-output) |
| console fine, `stat` looks wrong | [2](#2-the-radio-does-not-answer) |
| a press on the remote prints nothing | [3](#3-a-remote-press-is-not-seen) |
| `pair` never leaves `waiting_speed_1` | [4](#4-pairing-does-not-complete) |
| the fan ignores a command the device sent | [5](#5-the-fan-ignores-the-command) |
| no device, or too few entities in Home Assistant | [6](#6-home-assistant-shows-the-wrong-entities) |
| a command from Home Assistant does nothing | [7](#7-a-command-from-home-assistant-does-nothing) |
| a value in Home Assistant flips back | [7](#7-a-command-from-home-assistant-does-nothing) |
| the LED does not do what the table says | [8](#8-the-led) |

## 1. No console output

* Board plugged into USB, console at **115200 baud**, and the port not already
  held by another process - only one reader at a time. `fuser /dev/ttyACM0` or
  `lsof /dev/ttyACM0` shows a stray `python3`/`minicom`.
* No banner at all: the firmware is not running. `espflash flash` it again and
  watch for `Flashing has completed!`.
* Banner, then nothing: it is running, it is just not printing. Type `help`.

## 2. The radio does not answer

`stat` prints what the firmware programmed instead of reading it back, so the
interesting part is that the chip *accepts* SPI at all:

```
marcstate=0x0d rssi=-95dBm iocfg0=0x0d pktctrl0=0x32 mdmcfg2=0x30 segs=0 raw=0 gdo0=1
```

| Field | Good | If not |
|---|---|---|
| `marcstate` | `0x0d` (RX) | `0x00`/`0x01` that never changes: SPI or power. Check `VCC`→`3V3`, `CSN`, `SCK`/`MOSI`/`MISO`, and the [contact problem](WIRING.md#the-contact-problem) |
| `iocfg0` | `0x0d` | random values while `marcstate` looks sane: usually one marginal line, most often `MISO` |
| `mdmcfg2` | `0x30` | `0x02`-style values mean a 2-FSK configuration; the OOK build writes `0x30` |
| `rssi` | −100…−60 dBm | `0` or `-128`: the chip is not returning RSSI, same as above |
| `gdo0` | toggles when a remote is pressed | stuck: `GDO0` is not reaching `IO5`, or nothing is being received |

`status` adds the frequency (`433920 kHz`) and the watchdog: `rearm` climbing
while nothing is received is normal - the radio sometimes drops out of RX after a
transmit and is put back. It is worth a look only if it climbs during a burst of
known-good presses.

## 3. A remote press is not seen

* Watch the console while pressing. Every frame prints
  `rx device=0x… button=0x… code=0x…… rssi=-… dBm`. Nothing at all means the
  radio is not hearing it: [2](#2-the-radio-does-not-answer), then the antenna,
  then distance. Press the remote from 10 cm away to separate "RF" from "range".
* `segs=…` in `stat` climbing but no `rx` line: the edges arrive but the frame
  does not validate. `dump` shows the last capture - 24 pairs of ~400/1200 µs is
  a frame, a wall of similar timings is noise from another 433 MHz device.
* Weak signal: `rssi` in the `rx` line. Above −80 dBm is comfortable, below
  −95 dBm is marginal and the code will start missing.
* The same remote twice in one second only prints once: repeats are collapsed on
  purpose.

## 4. Pairing does not complete

`pair` starts a 90-second session: the LED blinks at 4 Hz and `pairing_stage`
goes `waiting_speed_1` → `_2` → `_3` → `_4` → `paired`.

* The stage does not move at all: the presses are not being decoded - see [3](#3-a-remote-press-is-not-seen).
  A press must be one of the remote's **speed buttons 1–4**, in that order.
* The stage stops advancing where you expect: a gap longer than 15 s restarts
  the sequence, and a press from a *different* remote restarts it too (the stage
  then shows what the device expects next). The session itself lasts 90 s from
  `pair`, after which it times out and the LED double-blinks.
* It ends in `failed` (double blink): one of the four buttons was not from the
  same remote.
* It ends in `paired` but `restart_required` stays on and no new fan appears:
  the endpoint set is built at boot - `reboot`, or use
  `apply_pairing_change` in Home Assistant, then let the converter re-interview.
* `slots` already shows an address you did not pair: `del <slot>` and pair again.
  A phantom slot makes the counts disagree with reality.

## 5. The fan ignores the command

The device says `sent` and the fan does nothing. In order of likelihood:

1. **The command was never transmitted.** The device answers the ZCL write before
   the RF frame goes out (~400 ms per command), so a "success" only says the
   command was accepted. `debug on` prints one `tx 0x...` line per frame: if a
   `zb: slot ...` line appears and no `tx` line follows it, the command died
   between the reply and the air. `status` shows `txq <waiting>/<dropped>`, and
   the same number is the `tx_dropped` sensor in Home Assistant - anything above
   zero is a command that was thrown away because the queue was full (backlog
   #54). This was the noon-schedule bug: three fans, six writes, two frames.
2. **Wrong remote for that fan.** Slot 0 is whatever you paired first; `list`
   prints the address and name. A slot is one physical remote, not one fan.
3. **Transmit mode.** `txmode` shows `async` or `packet`. Async drives
   `GDO0` from the firmware and is the proven path. Packet mode is faster and
   cleaner but one fan (`lr-r`) has never accepted it - if that fan is the silent
   one, `txmode async`.
4. **Repeats and gap.** `repeats` shows frames per command and the gap between
   them; a receiver needs a couple of frames to wake up. 8 frames / 12 ms is the
   default and what the remotes do.
5. **Antenna and range.** A fan across the flat is fine with a proper 433 MHz
   antenna; with a 5 cm wire it is not.
6. **Battery in the remote** - if the *original* remote still works and the
   bridge does not with the same code on air, the difference is timing or level,
   not the code. See [`docs/PROTOCOL.md`](PROTOCOL.md) for the measured waveform
   and [`docs/PACKET-TX.md`](PACKET-TX.md) for what packet mode changes.

## 6. Home Assistant shows the wrong entities

* **Fewer fans than remotes.** The endpoint set is built at boot; a pairing
  change needs a restart *and* an interview. `desc` lists what the device
  advertises right now - compare it with the fan entities. `restart_required`
  in Home Assistant says the same thing.
* **The whole device has no entities.** Zigbee2MQTT 2.14.2 needs
  Koenkk/zigbee2mqtt#33435 (fan exposes on non-primary endpoints); 2.15.0 has it.
  Without the fix the assertion drops every entity of the device, not just the
  fan.
* **The converter is not loaded.** `external_converters: [esp-fanbridge.js]` in
  `configuration.yaml`, the file in `data/external_converters/`, and a
  restart. Without it the fans and the direction selects do not exist at all.
* **`bridge/devices` looks stale.** It refreshes on device-list changes only; a
  Zigbee2MQTT restart shows the true endpoint set.
* **Stale keys** like `state_10`, `speed_2_2`, `direction_5` are left in
  Zigbee2MQTT's state from older definitions. Harmless; they disappear when the
  device is removed and re-paired, not worth chasing.

## 7. A command from Home Assistant does nothing

The device console prints every command it receives:

```
zb: slot 1 "lr-l" speed -> 2
```

* **No `zb:` line and the Zigbee2MQTT log says `MAC_TRANSACTION_EXPIRED`**: the
  frame never reached the device in time. Zigbee2MQTT publishes the optimistic
  value, its 10-second ZCL timeout expires, and the device's next report restores
  reality - that is the "it bounces back" symptom.

  This device is an end device, so the coordinator delivers to it through an
  *indirect* queue with a persistence timeout - and it hands frames over when the
  device polls. The device polls every 100 ms, which is why commands normally
  answer in well under a second.

  If they stop doing that:

  1. `status`: `rep <ok>/<fail>` shows whether the device's own reports are being
     acknowledged. A run of failures means the *link*, not the queue.
  2. `rejoin` if the parent's child entry looks stale (it re-associates, keys
     kept), `leave` if it is worse than that (rebuilds network membership; needs
     permit-join open).
  3. Check the Zigbee channel against the WiFi in the area. It was moved from 11
     to 25 for this reason (WiFi 6 has three APs here, the strongest one at
     -38 dBm). The firmware scans 11..26, so a coordinator channel change only
     needs a `leave`, not a reflash.
  4. The structural alternative is a **router** role (mains powered), which is
     addressed directly and has no indirect queue at all; see the backlog.
* **`UNSUPPORTED_CLUSTER` or `UNSUPPORTED_ATTRIBUTE`**: the converter is writing
  the wrong cluster or attribute for that endpoint - typically an old converter
  file. Compare with `z2m/esp-fanbridge.js` in this repository.
* **A `zb:` line appears but the fan does nothing**: the Zigbee side is fine,
  go to [5](#5-the-fan-ignores-the-command).
* **Is the device reporting at all?** `status` ends with `rep <ok>/<fail>`: reports
  the coordinator acknowledged and attempts that had to be retried. `report`
  queues a full state flush on demand - if `rep` climbs and nothing changes in HA,
  the problem is downstream (converter, Zigbee2MQTT, HA), not the device. A
  healthy device has one failure at boot (the first report races the join) and
  then climbs only in `ok`.
* **Value flips back with no error**: a report arrived carrying the old value -
  check `list` for what the device actually stored. If `list` shows the new
  value and HA does not, the report or the converter is at fault. `debug on`
  prints what the device transmits (`tx 0x… via bit-bang took … ms`) and every
  time the watchdog has to re-arm the receiver.

## 10. Nothing works until a serial console is attached

If the device behaves only while someone watches the console, the console is the
cause: until now every write waited for room in the USB FIFO, so a host that was
enumerated but not reading blocked whatever task was printing - the Zigbee stack
included. Fixed by dropping output instead of waiting (backlog #51). A build from
before that fix can be wedged the same way by `ESP_LOG=debug` with no reader.

A build from before backlog #53 also *cuts* lines: the 64-byte FIFO takes the
first 64 bytes and the rest never leaves, so `status` ends mid-word and a `slots`
dump loses entries. That is truncation, not corruption - and on a current build
`status` ends with `out N`, the number of output chunks lost because nobody was
reading. A healthy console has `out` climbing only at boot, before a reader
attaches.

## 9. The device stops accepting commands, but reports still arrive

Symptom: Home Assistant publishes, the state flips back, and the device keeps
sending reports - so the radio is clearly alive. On the console the tell is `tx 0`
after commands that should have transmitted, and `rep` failing.

Two causes were found and fixed in the stack (see backlog #50 and the
`fanbridge-fixes` branch of `Lauri-Nomme/zigbee-rs`):

* **The boot log says `resuming on network: addr=… ch N` with the wrong `N`.** The
  stack resumed on `channels.start` instead of the channel the network is on, so
  the rejoin went out on the wrong channel. A network on channel 25 must show
  `ch 25`.
* **`rejoin failed (…), forgetting network`.** A single refused rejoin used to
  wipe the network membership, leaving the device offline until somebody opened a
  join window. It now says `keeping the network (retrying)` and keeps trying.

If it happens on a build without the fix:

```sh
leave                      # forget the membership and re-commission (permit-join open)
```

and, if `leave` does not stick, erase the stack's store and flash again:

```sh
espflash erase-region --port /dev/ttyACM0 0x3F0000 0x4000
espflash flash --port /dev/ttyACM0 esp-fanbridge-<version>.elf
```

## 8. The LED

| Pattern | Meaning |
|---|---|
| brief blink every 2 s | heartbeat, everything is fine |
| fast blink (4 Hz) | pairing session running |
| solid | a remote was just learned (`paired`, 10 s) |
| double blink | pairing failed (`failed`, 10 s) |
| blinking for the identify duration | a coordinator asked the device to identify |

No light at all: `identify 5` should blink regardless of Zigbee, which separates
"LED broken" from "state machine quiet". The LED is on `IO15` and is driven by
the same task as the console, so a hung console loop means no LED either.
