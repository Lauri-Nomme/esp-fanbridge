# Backlog

Ideas, follow-ups and known issues, newest context first. Nothing here is
promised work; it is the list we keep coming back to.

Legend: **S** = small (minutes), **M** = an evening, **L** = a session of its own.

---

## 22. ZDO Bind — **already implemented by the stack**

The backlog entry came from old log noise. `zigbee-core` handles
`BIND_REQ`/`UNBIND_REQ` (`build_zdp_response` -> `apply_bind_req`), Z2M's
endpoint dump shows real bindings (`genOnOff` -> coordinator), and there is not
a single `bindRsp` timeout in 24 h of logs. Nothing to do.

## 1. Zigbee command delivery — measured, announce added

**Original symptom.** Commands sometimes came back as
`MAC_TRANSACTION_EXPIRED (0xf0)` and appeared to be applied minutes later.

**What the measurements show.**

* Steady state is fine: 10 commands 2 s apart and 15 commands 300 ms apart were
  all applied (latency 0.5–2 s), with **zero** expired transactions.
* Over 24 h, 88 of 89 expiries belonged to this device — and they cluster
  exactly in the windows where it was being flashed/rebooted. The only other
  device (an Aqara sensor) had one.
* After a reset, commands are **queued, not lost**: 20 publishes over 40 s all
  landed, the first one 12.9 s after the reset (boot + rejoin + route
  rediscovery). Nothing is silently dropped; it just waits.
* Root cause of the slow part: a resume after reset keeps the stored short
  address but the firmware never announced it, so the coordinator only
  rediscovered the route after its writes started failing.

**Fix (done).** `announce_task` broadcasts `Device_annce` three times right after
joining. Measured effect: first command after a reset moved from 12.9 s to
11.5 s and no expired transactions appeared in the window.

**One more data point (2026-10-23).** A direction write from Home Assistant came
back as `MAC_TRANSACTION_EXPIRED` and the value reverted in HA (the optimistic
value was overwritten by the device's next report). It happened while the device
was being reset during a test, and repeated attempts immediately afterwards - and
10/10 pairing-switch writes - went through. So the failure mode is real but
rare; if it is seen in normal use again, `status` (`rearm`, `tx`) and the Z2M
log around the write are the things to capture.

**Left to consider.**

* Boot-to-joined is ~6 s; the expensive part is radio bring-up/calibration, not
  something the application can skip.
* If slow rejoin becomes a problem again, poll faster (`poll_interval_ms`) or
  revisit `rx-on-when-idle` (`0x88`) now that the stack has moved on — the older
  `MacError(NoData)` may be gone, and this device is mains powered anyway.

## 2. Console commands update the stored state — **done**

`speed`, `dir` and `power` used to transmit without touching `cfg.slots[n]`, so
the device and Home Assistant kept reporting the old state after a console-driven
change. They now update and save the slot like the Zigbee handlers do (including
turning power on with a speed press) and reject empty slots. Verified: after
`speed 1 3`, `dir 1 out`, `power 1 off` the slot reads `speed=3 dir=out power=0`.

---

## 3. `debug` is a setter — **done**

`debug on|off` sets it explicitly, a bare `debug` reports the state. (It used to
toggle, which silently turned debugging off in scripted captures twice.)

---

## 4. Make packet mode the default — blocked on a fan — **M**

Packet transmit is merged and works on air (`docs/PACKET-TX.md`), but it is
opt-in (`txmode packet`) because of the next item. Once a physical fan accepts
it, flip `TX_PACKET` to `true` in `src/main.rs` and drop the `txmode` default.

---

## 5. Packet mode is still ignored by one fan — **M**

`lr-r` (`0x191D0`) reacts to **every** bit-banged command and to **none** of the
packet-mode ones, while `lr-l` (`0x06800`) accepted packet mode at least once.
Its own remote works, so the receiver is picky rather than broken.

Notes from the investigation:

* Frames are byte-identical in content to the bit-bang path (verified with a
  second C6+CC1101 listener at 8 µs resolution, 49 edges, 38.8 ms).
* Received level at the listener: ours −24 dBm, the remote −49 dBm. Our signal
  is *much* stronger, which distorts the listener's edge measurements — any
  timing comparison must be done at a comparable level (attenuator or distance).
* Measured pulse widths at that high level: ours ~409/1178 µs, the remote's
  ~415/1199 µs. Close, but the listener is not trustworthy at −24 dBm.
* The radio drops the **first `STX`** after switching async RX → packet TX; the
  firmware compensates with one extra frame (`REPEATS + 1`).
* Gap/repeat experiments (8×12 ms, 8×20 ms, 5×200 ms) made no difference.
* A carrier sweep ±200 kHz is **not** a valid experiment as implemented: the
  `freq` command wrote the FREQ registers without a synthesizer recalibration,
  so everything after it was off-tune until a reboot. If a sweep is ever wanted
  again, `SCAL` is mandatory (and the command was removed for this reason).

Next steps: compare at matched signal level, then try biasing the bit period to
match the remote's numbers, or try `PKTCTRL0`/`PKTCTRL1` variants that change
the preamble handling.

---

## 6. Pairing changes need a restart + re-interview — **M**

The endpoint table is built at boot, so a remote paired afterwards is counted in
`paired_count` but has no endpoint until a restart and a re-interview
(`exposed_count` vs `paired_count` and the `restart_required` binary sensor show
this). Options: restart automatically after a successful pairing (most
forgiving), or re-announce and let the user press "re-interview".

---

## 7. Synchronous-serial RX — **L (nice to have)**

`docs/PACKET-TX.md` §7: packet-mode RX is impossible for this protocol (no
preamble/sync to frame on), but `PKT_FORMAT = 1` (synchronous serial) makes the
radio output the demodulated bitstream with a hardware clock on GDO2. Framing
stays in software, but bit timing comes from the crystal instead of a 30 µs
polling loop, and the MCU only wakes on clock edges. Would make RX bit-accurate
and free the main loop.

---

## 8. Tooling: the console driver — **done**

`tools/fan.py` is in the repo with a usage note in the README: scheduled
commands, timestamps, optional reset, `-p PORT`, `-t seconds`. Every hardware
test in this session used it.

Related lesson, also in the script docstring: background captures must run
`python3 -u`, otherwise the output is block-buffered and the file stays empty.

---

## 9. Z2M: remove the local discovery patch — **S, waiting for a release**

Koenkk/zigbee2mqtt#33435 (fan entities on non-primary endpoints) is **merged**
(2026-10-20). The local workaround must stay until the running image is newer
than 2.14.2:

* ConfigMap `z2m-ha-fan-endpoint-fix` mounted over
  `/app/dist/extension/homeassistant.js`
* plus the `ha-fan-fix` volume/volumeMount in the deployment

Remove all three once a release with the fix is deployed, and re-run the fan
entities through a fresh interview to confirm upstream behaves the same.

---

## 10. Z2M: ship the device definition upstream — **M/L**

`z2m/esp-fanbridge.js` (external converter) registers the manufacturer-specific
`espPairing`/`espDirection` clusters and exposes the pairing stage, direction and
four speeds. Once the firmware is stable it should become a device definition in
zigbee-herdsman-converters, so no local file is needed:

* `zigbeeModel: ['esp-fanbridge']`, vendor `locals.tf`
* `espPairing` (0xFC00): `stage`, `pairedCount`, `exposedCount`, `restartRequired`
* `espDirection` (0xFC01): `direction` (in/out/auto)
* fan as speed-controlled (1..4) with `state` from `fanMode`

---

## 11. Docs / repo hygiene — **S**

* `c6/FANBRIDGE.md` and the repo's `docs/DESIGN.md` are copies and will drift;
  pick one canonical home (the repo) and leave a pointer in the other.
* `~/dev/c6` is untracked inside the outer git repo, which now contains a nested
  repository (`c6/firmware/examples/esp-fanbridge/.git`). Either add `c6/` to
  the outer repo's `.gitignore` or decide to track it deliberately.
* The listener firmware (`c6/firmware/examples/esp-ookcapture`) gained two local
  changes that are not committed anywhere: `MIN_PULSE_US = 8` and an RSSI
  readout per burst. They were essential for the packet-mode investigation.

---

## 12. Cosmetic leftovers — **partly done**

* The README now explains how to clear a stale retained discovery topic after an
  endpoint-set change (empty retained payload on the `homeassistant/.../config`
  topic).
* The device's published state still carries a few stale keys from earlier
  definitions: `state_10`, `state_11`, `state_12`, plus `speed_2_2`/`state_2_2`
  from the old double-postfix bug. Harmless, but they live in Zigbee2MQTT's state
  cache; deleting the device's cache (or re-interviewing after clearing it) makes
  them go away.
* `speed_2_2`/`state_2_2` came from returning already-postfixed properties in
  `toZigbee`; Zigbee2MQTT postfixes those results itself. Noted so nobody debugs
  it twice.

---

# Round 2 — ideas (not implemented, discuss first)

Ordered by area, not priority. Effort: **S** minutes, **M** an evening, **L** a
session.

## State visibility

### 41. Retain speed and direction while the fan is off — **done**

Today a powered-down fan loses its speed in Home Assistant: `FanMode` reports
`Off`, the converter maps that to speed 0, and the slider snaps to 0%. The device
itself still remembers the slot's speed and direction in its config, so turning
the fan back on restores them on the wire - it is the *reported* state that
forgets.

**Decided: the first option.** The converter no longer publishes `speed: 0` when
the fan is off - the payload carries `state: OFF` alone, so the cached speed
survives and Home Assistant shows "off, last 50 %". Turning the fan on again
already re-sent the remembered speed (the `meta.state` lookup in `convertSet`),
so this makes the reported side match what the wire side was doing. Verified on
hardware: `state_3 OFF` with `speed_3 2` retained, and back to `ON, 2`.

Options that were considered:

* report the last non-zero speed alongside `state: OFF` (HA then shows "off but
  33%", which is how most MQTT fans behave), at the cost of a second value to
  keep in sync - **chosen**
* keep the `Off`/0 mapping and accept that the UI forgets: simpler, matches the
  ZCL model where `Off` *is* the mode
* if the physical fans also forget their direction when unpowered, re-assert
  direction (and speed) when the power command is issued, so the fan comes back
  in the state the UI shows; needs a real fan that behaves that way to confirm

Related: the console `power <slot> on` already re-sends the last speed, which is
the same idea one layer down.


### 13. Report fan/switch/direction changes — **done and verified from RF**

Right now **only the pairing cluster sends attribute reports**. A press on a
physical remote changes the device's state, but nothing tells the coordinator:
Home Assistant keeps showing the old speed/direction until something else
triggers a read. That breaks the "state is readable back" requirement as soon as
someone uses the remote instead of HA.

Fix: send `Report Attributes` on change for `FanMode` (per fan endpoint),
`OnOff` (pairing switch) and `direction` (per fan), from *both* the RF path and
the Zigbee writes. `send_pairing_report` already shows the pattern; generalise it
(bitmask of dirty endpoints + the existing retry loop).

Verified from the air (a second C6+CC1101 replaying real remote codes):

```
device:  rx device=0x191D0 button=0xC code=0x191D0C
z2m:     speed_2: 3, state_2: ON
device:  rx device=0x191D0 button=0x4 code=0x191D04
z2m:     direction_2: out
```

No Zigbee write involved - the updates came only from the device's reports.

### 14. Publish the full state after (re)join — **S**

After a reboot or re-interview the coordinator only knows defaults. Send one
report with every attribute once joined (reuse #13's machinery), so HA and Z2M
resync without waiting for a change.

## RF / OOK tuning

### 15. Apply TI DN022's OOK receiver settings — **S/M**

DN022 ("CC110x OOK/ASK register settings") says SmartRF's FSK/MSK AGC defaults
are not optimal for OOK and recommends `AGCCTRL2 = 0x03..0x07`,
`AGCCTRL1 = 0x00`, `AGCCTRL0 = 0x91 or 0x92`, plus `FREND1 = 0xB6` for an RX
filter > 101 kHz (`0x56` for ≤ 101 kHz).

Ours: `AGCCTRL2=0x03` ✓, `AGCCTRL0=0x91` ✓, but `AGCCTRL1=0x40` and
`FREND1=0x56` while our filter is 203 kHz (`MDMCFG4=0x87` → CHANBW_E=2,
CHANBW_M=0 → 26e6/(8·4·4) ≈ 203 kHz). Worth trying `0x00` and `0xB6`, measuring
decode range/noise on the listener and the real remotes.

Also confirmed from the datasheet: `MDMCFG2.MOD_FORMAT` `011` = ASK/OOK and
`010` is *reserved* (not OOK) — which is exactly the packet-mode bug we hit.

### 16. Log RSSI per received frame — **S**

`rx device=… button=… rssi=-74dBm`. Costs one register read; turns "the remote
worked 3 of 5 times" into a number and shows whether the fanbridge or the remote
is the weak side.

### 17. Calibrate the carrier against a real remote — **M**

The listener can measure both transmitters, but only at comparable levels (ours
is ~24 dB stronger at 0.3 m). With an attenuator or a fixed distance, compare our
carrier frequency against a remote's; if there is a systematic offset, adjust the
frequency word (or model the crystal tolerance) instead of assuming 26 MHz.

### 18. Per-fan transmit profile — **M**

`lr-r` ignores packet mode while `lr-l` accepts it. Store per-slot transmit
parameters (mode, repeats, gap, PA level) so a picky receiver can be tuned
individually without breaking the others. Also makes the "learn the remote's
pattern" idea possible (the remote sends 4-5 frames with a long gap, we send 8
with 12 ms).

## Pairing / endpoints

### 19. Auto-restart after pairing + automatic re-interview — **done**

The firmware restarts three seconds after learning a remote (new endpoint
advertised on boot) and the Z2M converter now re-interviews by itself when it
sees an announce whose `exposedCount` differs from the endpoint count it has
cached. Verified in both directions: pairing a remote produced the fourth fan
entity without any click, deleting one removed it again.

The endpoint set is fixed at boot, so a new remote needs `reboot` + re-interview.
Restarting automatically after `Paired` (with a console message) removes the most
confusing step in the whole flow. `restart_required` already exists as a signal.

### 20. Pairing session timeout is dead code — **S**

`PAIR_SESSION_MS` is defined and never used (compiler warning). Implement the
90 s session limit and report `failed` (stage 6) when it expires, so HA shows a
timeout instead of "waiting forever".

### 21. Distinct LED pattern per pairing stage — **S**

The LED already blinks fast while pairing. Give "learned"/"failed" their own
short patterns so pairing can be done without the console.

## Zigbee stack / device behaviour

### 22. Answer ZDO `Bind_req` — **already done by the stack** (see round 1, #22)

### 23. `fanModeSequence` attribute — **S**

ZCL 4.4.2.2.2 (`0x0202` cluster, attribute `0x0001`) declares which mode
sequences the device supports. We implement `Off/Low/Medium/High/On`; declaring
it is cheap and helps coordinators reason about the fan.

### 24. Make Identify blink the LED — **S**

The Identify cluster is served, but the LED ignores it, so Z2M's "identify"
button does nothing visible. Blink for `identify_time` seconds.

### 25. Revisit `rx-on-when-idle` — **M**

The device is mains powered but declares itself sleepy (`0x80`). `0x88` failed
earlier with `MacError(NoData)`; the stack has changed a lot since. Staying awake
would remove polling latency entirely (see backlog §1).

### 26. Expose the watchdog counters — **S**

`rxRearmCount` (how often the radio had to be re-armed, see #6 in round 1) as a
diagnostic attribute — a device that needs re-arming often is telling us
something.

## Firmware structure / tooling

### 27. Split the protocol logic into a testable library — **M**

`expand_code`, `decode_frame`, the config serialisation/migration and the
speed↔mode mappings are pure functions with **no tests**, because the crate
depends on esp-hal and cannot run on the host. Moving them into a `no_std` lib
crate (no esp dependencies) allows `cargo test` on the workstation and protects
the config migration and the 49-byte expansion, which are exactly the parts that
broke unnoticed.

### 28. CI: build + clippy + fmt — **S/M**

A GitHub Actions workflow with the esp-rs toolchain, `cargo build --release` for
`riscv32imac-unknown-none-elf`, `cargo fmt --check` and `clippy`. Catches
"compiles only because of my local toolchain" regressions without hardware.

### 29. Tagged releases with binaries — **done**

`.github/workflows/release.yml` runs on a `v*` tag: protocol tests, firmware
build, `espflash save-image --merge`, then a release with the merged `.bin`, the
`.elf` and the matching converter `.js`. `docs/RELEASING.md` describes cutting
one and flashing it. First tag: `v0.1.0`.

Attach the `.bin`/`.elf` to a tag so flashing does not require the toolchain
(`esptool`/`espflash` can write a bin directly).

### 30. Persist the last panic — **done**

The firmware installs its own `#[panic_handler]` (esp-backtrace's handler is no
longer compiled in, `Backtrace` still is): it formats the panic into a bounded
buffer, writes one record to its own sector at `0x3D0000`, prints the same
information and halts. The next boot prints

```
last panic after 1s: panicked at src/console.rs:685:13: console: test panic (expected)
```

and the main loop erases the record once the scheduler is running. That last
detail is not cosmetic: erasing during early boot *aborts the chip*, which is how
this was found - the first version erased inside the boot-time read and every
subsequent boot died with "Aborted". The console `panic` command exists to test
the whole path.

`esp-backtrace` prints to the console, which nobody sees when the device is in a
cabinet. Writing a panic record (message/PC) to a spare flash sector and printing
it on the next boot would explain spontaneous reboots.

## Console / UX

### 31. `status` command — **S**

Uptime, transmit count, received-frame count, watchdog re-arms, current mode and
frequency in one line — the first thing to ask a device when debugging.

### 32. Per-slot overrides from the console — **S**

`set <slot> repeats 12`, `set <slot> gap 200`, `set <slot> power 0x60`: expose
what #18 stores without a reflash.

### 33. Config backup/restore — **done**

`cfg dump` prints the 512-byte image as hex (32 bytes per line with an offset);
`cfg load` puts the console into paste mode, `end` commits. The image is
validated (magic, version, checksum) before anything is written, so a truncated
or mistyped paste changes nothing. Verified on hardware: a clobbered fan name
was restored from a dump, and a dump with one digit flipped was refused. The
console line buffer grew to 96 bytes so a dump line fits.

## Z2M / Home Assistant

### 34. Upstream device definition (was #10) — **M/L**

### 35. Remove the local discovery patch (was #9) — **S**, waiting for a release

### 36. "Apply pairing change" button — **done**

The `espPairing` cluster now answers command `0x00` (`reboot`): it schedules a
restart 300 ms out, so the command's default response is on the air first, and
the main loop does the reset. The converter exposes it as
`select.…_apply_pairing_change` (`apply`) — this version of Zigbee2MQTT has no
`exposes.Button`, so an enum action is the idiom here. Verified on hardware:
selecting `apply` rebooted the device (console: `restarting (requested)...`),
it rejoined and announced, and Zigbee2MQTT reported no error.

### 37. Names as read-only sensors — **rejected for now**

We decided earlier not to hack entity naming: HA names live in Zigbee2MQTT/HA.
Device-side names stay in the console only. Revisit only if a real need appears.

## Docs

### 38. Wiring diagram in the repo — **done**

`docs/WIRING.md`: parts, the pin map with the silkscreened numbers, an ASCII
diagram, the two ways to destroy the module (5 V, and the 2×4 header inside one
breadboard column), the friction-fit problem, and a power-up checklist built
around the values `stat` actually prints. Text only - the photos stay in the
hardware notes rather than adding 60 MB to this repository.

### 39. Troubleshooting decision tree — **done**

`docs/TROUBLESHOOTING.md`: a symptom table, then the order we actually debug in -
bridge alive (`status`), radio alive (`stat`), RF (`rx` lines, `dump`, RSSI),
Zigbee reachability (`zb:` lines, `desc`, Zigbee2MQTT log), with the good value
next to each. Also covers the two HA-specific traps: an expired transaction
(value flips back) versus `UNSUPPORTED_CLUSTER` (converter writing the wrong
cluster), and the stale state keys left behind by older definitions.

### 40. Reference waveform in `docs/PROTOCOL.md` — **done**

A "Reference waveform" section next to the nominal table: measured short/long
pulse ranges over 24 pulses (375–425 µs / 1183–1219 µs, means 400.1/1197.5),
frame total 38.75 ms, and the decoder's acceptance windows (250–650, 850–1800,
33–45 ms) so a mismatch can be read off directly. Plus how to compare a live
device: `dump` for the raw edges, `rx …` for the decoded code and RSSI.
