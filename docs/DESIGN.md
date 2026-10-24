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

Resolved since the first draft of this file, listed so nobody re-opens them:

* **State reporting** is pushed from a task of its own with retries, for every
  change from any source; `status` counts acknowledgements (`rep ok/fail`).
* **`Bind` requests** are answered by `zigbee-core`; the stack handles them.
* **Receive path**: the device is mains powered and declares
  `rxOnWhenIdle`, so it listens instead of polling for queued frames. The
  earlier attempt failed with `MacError(NoData)`; the current stack does not.
* **Direction** is a manufacturer-specific cluster (`espDirection` `0xFC01`) on
  the fan's own endpoint, not a second endpoint.
* **Endpoint changes** no longer need a console: the pairing cluster answers a
  `reboot` command and Home Assistant exposes it as `apply_pairing_change`, and
  the converter re-interviews by itself when the announced count changes.

Still open, with the backlog as the source of truth (`backlog.md`):

1. **Packet mode is opt-in, not the default.** It works on two of three fans; one
   (`lr-r`) ignores it while accepting every bit-banged frame. Both modes are
   verified to put the same 24 symbols on the air, so the remaining difference is
   timing (see `docs/PACKET-TX.md`).
2. **No carrier calibration** against a real remote at a controlled level, and no
   per-fan transmit profile (repeats/gap/power). Both wait for a measurement
   setup; see backlog #17/#18.
3. **RX bit timing comes from a polling loop** (~30 µs). Synchronous-serial mode
   (`PKT_FORMAT = 1`) would take it from the crystal instead; backlog #7.
4. **The config and panic sectors live inside the `factory` partition**, past the
   application image. Application-only flashes keep them, a merged full-flash
   image does not, and an application that ever grows past ~3.8 MB would collide
   with them. A custom partition table would remove the caveat.
5. **Zigbee2MQTT 2.14.2 needs the upstream patch** (#33435, merged) for fan
   exposes on non-primary endpoints; 2.15.0 and later contain it.

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
| Firmware | this repository, `src/` | working, flashed, releases tagged |
| Converter | `z2m/esp-fanbridge.js`, tested by `z2m/test.js` | working, live |
| Host tests | `protocol/` (12) and `z2m/test.js` (16) | in CI, along with `fmt`/`clippy`/build |
| Releases | git tags, built by `.github/workflows/release.yml` | `v0.1.0` onwards |
| Upstream | Koenkk/zigbee2mqtt#33434, #33435 | merged; 2.15.0+ needs no patch |
| Home Assistant | `fan.…_<endpoint>`, `select.…_direction_<endpoint>`, pairing switch + sensors + `apply_pairing_change` | working |
| Documentation | this file, `PROTOCOL.md`, `PACKET-TX.md`, `WIRING.md`, `TROUBLESHOOTING.md`, `RELEASING.md` | current |
