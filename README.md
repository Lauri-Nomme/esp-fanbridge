# esp-fanbridge

A 433 MHz ceiling-fan remote bridge for the ESP32-C6 with a CC1101 transceiver.

It learns up to 8 VT100-style fan remotes and exposes each of them to Zigbee as a
separate device, so Home Assistant gets a controllable *and* readable fan entity
per remote. State is tracked from both sides — a press on the physical remote
updates Home Assistant, and a command from Home Assistant is transmitted back to
the fan — and pairing/state survives reboots.

* Stack: `esp-hal` 1.1 + [`zigbee-rs`](https://github.com/zigbee-rs/zigbee-rs)
  (pure-Rust Zigbee, pinned by `rev` in `Cargo.toml`)
* Radio: CC1101 on SPI, 433.92 MHz ASK/OOK
* Device: sleepy end device, ZCL `Fan Control (0x0202)` per learned remote

Design notes and the full reverse-engineering story: [`docs/DESIGN.md`](docs/DESIGN.md)
and [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

## Build and flash

```sh
rustup toolchain install nightly --component rust-src --target riscv32imac-unknown-none-elf
cargo install espflash --version 4.6.0      # provides the `runner` in .cargo/config.toml

cargo run --release                          # builds and flashes, then opens the monitor
cargo run --release -- --no-reset            # if the board is already in a good state
```

The `espflash` runner in `.cargo/config.toml` does the flashing and monitoring, so
`cargo run` is all you need once the toolchain is in place. `Cargo.lock` is
committed: the Zigbee crates are pulled from git by revision, so pinning matters.

## Console

```sh
sudo minicom -D /dev/ttyACM0 -b 115200      # Ctrl-A W to wrap, Ctrl-A X to exit
```

| Command | What it does |
|---|---|
| `list` | show paired remotes (slot, address, speed, direction, power, name) |
| `pair` | learn a new remote: press **speed 1 → 2 → 3 → 4** on it |
| `cancel` | abort pairing |
| `del <slot>` | forget a remote |
| `speed <slot> <1-4>` | transmit a speed command |
| `dir <slot> <in\|out\|auto>` | transmit a direction command |
| `power <slot> <on\|off>` | transmit power |
| `raw <hex>` | transmit an arbitrary 24-bit code |
| `name [text]` | show or set the device name (stored in flash) |
| `name <slot> [text]` | show or set a fan's name |
| `reboot` | restart the bridge (applies a changed endpoint set) |
| `save` | force a config save |
| `stat` | chip state: MARCSTATE, RSSI, IOCFG0/PKTCTRL0/MDMCFG2, captured segments, raw edge count, GDO0 level |
| `dump` | the last captured edge timings |
| `desc` | the endpoint descriptors this device advertises (what Z2M will read) |
| `debug` | toggle the noisy `junk:` diagnostics |
| `clock` | route the CC1101's XOSC/192 clock to GDO0 — a **pin self-test**: if `raw` in `stat` explodes, the GDO0 wiring is good |

## Zigbee model (what Home Assistant sees)

| Endpoint | Clusters | HA entity |
|---|---|---|
| 1 | Basic, Identify, On/Off, `espPairing` `0xFC00` | `switch.…_1` (pairing mode) + pairing stage/count sensors |
| 2..9 | Basic, Fan Control `0x0202`, `espDirection` `0xFC01` | `fan.…_2` … `fan.…_9` (four speeds) + a `direction` select each |

The endpoint set is built from the configured remotes at boot: a slot that is
not paired has no endpoint at all, so the coordinator only sees the fans that
actually exist. After `pair` or `del`, restart the bridge (`reboot`) and
re-interview it in Zigbee2MQTT — the endpoint list is part of the simple
descriptors, which are only read during an interview.

Requires Zigbee2MQTT **2.15.0 or newer**, or a patched 2.14.x: upstream HA
discovery used to refuse `fan` exposes on non-primary endpoints and, because the
assert fires while the payloads are being built, it dropped *every* entity of the
device. The fix is [Koenkk/zigbee2mqtt#33435](https://github.com/Koenkk/zigbee2mqtt/pull/33435).


## Home Assistant / Zigbee2MQTT

Copy [`z2m/esp-fanbridge.js`](z2m/esp-fanbridge.js) into Zigbee2MQTT's
`data/external_converters/` directory and restart it. The converter registers the
firmware's manufacturer-specific `espPairing` cluster, exposes the fan and
switch endpoints, and adds:

| Entity | Meaning |
|---|---|
| `switch.…_1` | pairing mode: on starts learning, off cancels |
| `sensor.…_pairing_stage` | `idle`, `waiting_speed_1`..`waiting_speed_4`, `paired`, `failed` |
| `sensor.…_paired_count` | how many remotes are stored |
| `fan.…_<endpoint>` | the remote's fan: four speeds, on/off |
| `select.…_direction_<endpoint>` | `in`, `out` or `auto` (the fan alternates by itself) |

The stage is reported by the device as soon as it changes (including the
`paired`/`failed` result, which stays visible for 10 seconds), so the user gets
feedback while pressing the four speed buttons on the remote.

## Diagnostics cheat-sheet

| `stat` shows | Meaning |
|---|---|
| `raw=0`, `segs=0` | no activity at all: antenna/sensitivity problem, or nothing transmitting nearby |
| `raw` huge, `segs=0` | demodulator noise only (sub-100 µs), still no carrier |
| `segs>0`, no `rx` line | hearing something, but it doesn't pass the frame validator (→ `junk:` lines) |
| `rx device=… button=…` | a valid frame decoded |
| `iocfg0≠0x0d` | GDO0 is not in serial-data mode (the `clock` self-test must be turned off) |

A healthy baseline here had RSSI around **−74 dBm**; after the reception degraded
it sat at **−81 dBm** with a static GDO0. Check the SMA antenna first.

## Protocol

Reverse engineered — see [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

* 433.92 MHz, ASK/OOK, 24-bit PWM, MSB first
* `0` = 400 µs high + 1200 µs low, `1` = 1200 µs high + 400 µs low
* frame = 24 bits + 400 µs trailing high, ≈ 38.7 ms
* `code = address << 4 | button`, address 20 bits
* speed 1/2/3/4 = `0x2/0x1/0xC/0x7`, dir in/out/toggle = `0x8/0x4/0x3`, power = `0xA`

## Storage

Config lives raw in flash sector `0x3E0000` (4 KiB), written via
`esp_storage::ll` (erase + program), with a magic + checksum. The Zigbee stack
keeps its own region at `0x3F0000..0x3F4000`, so the two don't collide.

## State model

* speed buttons set `speed` and force `power = true`
* the direction buttons set `in`, `out` or `auto` (the third button); auto makes
  the fan alternate between the two directions on its own
* `power` toggles
* every change is written back to flash
