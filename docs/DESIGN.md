# fan-bridge — ESP32-C6 + CC1101 Zigbee fan-remote bridge

Status: **working.** The C6 learns the 433 MHz VT100 fan remotes and exposes them
to Home Assistant as Zigbee fan entities, one per remote, controllable and
state-readable, with config persisted across reboots.

Builds on two earlier pieces:

* **[FAN_REMOTE.md](FAN_REMOTE.md)** — the 433 MHz protocol, reverse engineered.
* **[DEMO.md](DEMO.md)** — the Zigbee light, which is where the Zigbee stack
  (`esp-hal` + `zigbee-rs`) and the Home Assistant wiring were first worked out.

---

## 1. What it does

The device is a **Zigbee end device** with 17 endpoints:

| Endpoint | Device | Clusters | Home Assistant |
|---|---|---|---|
| **1** | master | Basic, Identify, On/Off | **pairing switch** (`switch.…_1`) |
| **2..9** | remote slot 1..8 | Basic, **Fan Control (0x0202)** | **fan** (`fan.…_2` … `fan.…_9`) with off/low/medium/high |
| **10..17** | remote slot 1..8 | On/Off | **direction switch** (`switch.…_10` … `_17`, off = in, on = out) |

* Turning a fan's switch on and picking a mode sends the matching 433 MHz button
  to that fan's remote address.
* Toggling a direction switch sends the explicit `dir in` / `dir out` button.
* Flipping the master switch puts the device into **learning mode**.
* The device also *listens* continuously: pressing the physical remote updates
  the tracked state (`fan_state_2`, `fan_mode_2`, …), so HA reflects reality even
  when the physical remote is used.

Verified live with three remotes (addresses `0x1A5F0`, `0x06800`, `0x191D0`).

## 2. Architecture

```
  physical remote ──433MHz──► CC1101 ──SPI──┐
                                            ├─► ESP32-C6 (esp-hal + zigbee-rs)
  HA ◄── MQTT ◄── Zigbee2MQTT ◄── Zigbee ───┘        │
                                                     └──433MHz──► fan receiver
```

* **ESP32-C6 (DFRobot Beetle)** — one firmware does RF RX/TX, the pairing state
  machine, flash config, the TTY menu and the Zigbee endpoint model.
* **CC1101** — 433.92 MHz ASK/OOK front end. See FAN_REMOTE.md §3 for wiring.
* **Zigbee2MQTT** — auto-generates the definition from the endpoints. One
  **local patch** is required (see §6).
* **Home Assistant** — MQTT discovery from Z2M.

## 3. Firmware

`firmware/examples/esp-fanbridge/` — see its `README.md` for the console
reference. Modules are all in `main.rs`:

| Area | Notes |
|---|---|
| `cc1101_init` / `enter_rx` / `send_code` | SPI + radio. RX/TX share `GDO0` (`IO5`), flipped between input and output. |
| `decode_frame` | validates 24-bit EV1527-style PWM frames (400/1200 µs, 33–45 ms total). |
| `Config` / `Slot` | 8 slots × `{addr, speed, dir, power}`, magic + checksum. |
| `panic_log` | the last panic, at **`0x3D0000`** (its own sector, next to the config): the handler formats the panic into a bounded text buffer, erases and writes one record; the next boot prints it and the main loop erases it again. The erase must happen *after* the scheduler starts - doing it during early boot aborts the chip. |
| `load_config` / `save_config` | raw flash at **`0x3E0000`**, sector erase + program via `esp_storage::ll`. The Zigbee stack keeps `0x3F0000..0x3F4000`, so they don't collide. Both are inside the `factory` partition (`0x10000..0x400000`), but past the application image: an application-only flash keeps them, a merged full-flash image wipes them, and an application that ever grows past ~3.8 MB would write over them. |
| `Chain` | one `ClusterRequestHandler` that dispatches to 35 servers held as `dyn` trait objects (a 35-deep tuple would blow up, and the library's tuple impls stop at 5). |
| servers | `OnOffEp<EP, SLOT, KIND>` (KIND 0 = power, 1 = direction, 2 = pairing), `FanEp<SLOT>` (Fan Control). Each checks `request.dst_endpoint` before delegating to the library's default `handle_request`, which is what makes per-endpoint clustering work. |

Two non-obvious details worth keeping:

* **Reporting must be declared.** `Attribute<Bool, ReadOnly, Reportable>` — with
  the default `NotReportable` the stack answers `UNREPORTABLE_ATTRIBUTE` and Z2M
  logs a configure failure forever.
* **`ENDPOINTS` is what Z2M sees.** Change a cluster list there, not just in the
  server, or the descriptor and the implementation disagree.

## 4. Pairing (learn a remote)

1. Flip the **pairing switch** in HA (or type `pair` on the console).
2. Press **speed 1 → 2 → 3 → 4** on the remote, within 15 s per step.
3. The device matches the sequence against frames from one address, stores it in
   the first free slot, saves the config and pairs into that slot's endpoint.

The 4-button sequence is what proves it is *that* remote rather than a stray
433 MHz device; the address is what gets stored.

## 5. Console

```sh
sudo minicom -D /dev/ttyACM0 -b 115200     # Ctrl-A W to wrap, Ctrl-A X to exit
```

`list`, `pair`, `cancel`, `del <slot>`, `speed <slot> <1-4>`, `dir <slot> <in|out>`,
`power <slot> <on|off>`, `raw <hex>`, `save`, plus diagnostics `stat`, `dump`,
`desc`, `clock`, `debug`. The `stat`/`desc`/`clock` additions are what made this
debuggable — see `firmware/examples/esp-fanbridge/README.md`.

## 6. The Zigbee2MQTT patch

**Why:** Z2M's HA discovery cannot map a `fan` expose that lives on a
non-primary endpoint — `lib/extension/homeassistant.ts`:

```ts
assert(!endpointName, "Endpoint not supported for fan type");
```

and because the assert fires while building the payloads, **the whole device's
discovery fails** — not just the fan.

* Upstream issue: **[Koenkk/zigbee2mqtt#33434](https://github.com/Koenkk/zigbee2mqtt/issues/33434)**
* Upstream PR: **[Koenkk/zigbee2mqtt#33435](https://github.com/Koenkk/zigbee2mqtt/pull/33435)**
  (`biome` + `tsc` clean, **107 tests pass**, including a new
  `Should discover fan on a non-first endpoint`; single-endpoint payloads
  unchanged)

**Locally** we don't wait for the merge — the patched build is mounted into the
run-flashing Z2M container:

```sh
kubectl create configmap z2m-ha-fan-endpoint-fix -n home-iot \
  --from-file=homeassistant.js=z2m-patch/homeassistant.js \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl patch deployment zigbee2mqtt -n home-iot --type=strategic -p "$(cat z2m-patch/deployment.patch.yaml)"
kubectl rollout restart deployment/zigbee2mqtt -n home-iot
```

`z2m-patch/` holds the patched `homeassistant.js`, the upstream diff, and the
deployment patch. **The ConfigMap survives pod restarts but not image upgrades** —
after a Z2M upgrade, re-run the two commands above (or drop the patch once
#33435 is merged).

## 7. Verified behaviour

| Check | Result |
|---|---|
| Pairing by the speed1→4 sequence | ✅ `paired remote 0x06800 into slot 0` |
| Persistence across reboot | ✅ after a real reset `list` still shows the slot with `speed=4 dir=in power=on` |
| RF transmit | ✅ `speed 0 3` / HA `fan_mode_2` → device sends the code |
| RF receive | ✅ physical remote decoded live (`rx device=0x06800 button=0x2 code=0x068002`) |
| Zigbee join + interview | ✅ 17 endpoints, `fan-bridge`, interviewed |
| HA entities | ✅ 8 fans, 9 switches, 9 selects, identify button, linkquality (28 total) |
| HA → device → state round trip | ✅ `/2/set/fan_mode_2 = high` → `{"fan_mode_2":"high","fan_state_2":"ON"}` |

Evidence: `evidence/fanbridge-*.txt`.

## 8. Known issues / next steps

1. **Z2M needs the patch** until #33435 merges.
2. **`Bind` requests are unanswered** — Z2M tries to bind `genOnOff` for
   reporting and logs `bindRsp after 10000ms` errors, then retries. Our stack
   doesn't implement the ZDO Bind request. Reporting is declared and accepted, but
   we don't push reports yet, so Z2M's state is optimistic for the switches.
3. **Sleepy end device.** `CapabilityInformation(0x80)` + 100 ms polling. Tried
   rx-on-when-idle (`0x88`) and the stack produced `MacError(NoData)` and gave
   up, so it stays sleepy. Consequence: interviews are timing-sensitive — if an
   interview fails with *"can not get active endpoints"*, remove the device in
   Z2M, wipe the device's Zigbee store, and let it rejoin with permit-join
   already open (see §9 for the exact order).
4. **Direction is a second endpoint** (switch) because ZCL has no direction
   attribute in Fan Control. A converter could fold it into the fan entity later.
5. **Real state reporting** — send `send_attribute_report` on change so HA tracks
   physical-remote presses without polling.
6. **End-to-end fan test.** The RF replay was verified as a technique and HA
   control verified as a round trip; confirming an actual fan spinning for each
   of the three remotes is the last physical check.

## 9. Re-pairing / recovery recipe

Order matters — Z2M only interviews a device it doesn't already know:

```sh
# 1. forget it in Z2M
mosquitto_pub -h 127.0.0.1 -t 'zigbee2mqtt/bridge/request/device/remove' \
  -m '{"id":"0x9c139efffecb054c","force":true}'      # then wait ~10 s
# 2. open permit-join BEFORE the device reboots
mosquitto_pub -h 127.0.0.1 -t 'zigbee2mqtt/bridge/request/permit_join' \
  -m '{"value":true,"time":254}'
# 3. wipe the device's Zigbee NIB so it commissions from scratch, then reset
sudo ~/.cargo/bin/../../tmp/opencode/espvenv/bin/esptool --port /dev/ttyACM0 \
  erase-region 0x3f0000 0x4000
```

(On `odroid`: `sudo /tmp/opencode/espvenv/bin/esptool …`, then reset the board.)

Doing 3 before 2 leaves the device "on a network" that the coordinator has
forgotten, and it will never re-announce.

## 10. State of the world

| Component | Where | State |
|---|---|---|
| Firmware | `firmware/examples/esp-fanbridge` (built on `odroid` at `~/c6/zigbee-rs/examples/esp-fanbridge`) | working, flashed |
| Z2M patch | `z2m-patch/`, live ConfigMap `z2m-ha-fan-endpoint-fix` | applied |
| Upstream | issue #33434, PR #33435 | open, awaiting review |
| HA | `light.0x…79d4` (older Zigbee light), `fan.…054c_2..9`, switches, selects | working |
| Documentation | `FAN_REMOTE.md`, `DEMO.md`, this file, `hw/WIRING.md`, per-example READMEs | done |
