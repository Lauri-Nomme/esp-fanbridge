# Backlog

Ideas, follow-ups and known issues, newest context first. Nothing here is
promised work; it is the list we keep coming back to.

Legend: **S** = small (minutes), **M** = an evening, **L** = a session of its own.

---

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
