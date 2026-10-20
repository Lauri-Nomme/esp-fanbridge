# Backlog

Ideas, follow-ups and known issues, newest context first. Nothing here is
promised work; it is the list we keep coming back to.

Legend: **S** = small (minutes), **M** = an evening, **L** = a session of its own.

---

## 1. Zigbee command delivery is flaky — **M/L, next up**

**Symptom.** Commands published while the device runs sometimes come back as
`MAC_TRANSACTION_EXPIRED (0xf0)` from the coordinator, and are applied minutes
later (or not at all). Observed repeatedly while testing:

```
Error: ZCL command 0x9c139efffecb054c/2 hvacFanCtrl.write({"fanMode":1} ...)
  failed (Data request failed with error: 'MAC_TRANSACTION_EXPIRED' (0xf0))
```

followed later by:

```
zh:zstack: Data confirm error (0x9c139efffecb054c:5338,240,3)
zh:zstack: Request network address of '0x9c139efffecb054c'
```

**Why it matters.** This is the only failure that makes Home Assistant control
feel broken while the RF side is perfectly fine. Everything else in this file is
cosmetic next to it.

**What we know.**

* The device is a sleepy end device: `CapabilityInformation(0x80)` (allocate
  address, **rx-off-when-idle**), poll interval 100 ms (`TimingConfig`).
  `0x88` (rx-on-when-idle) was tried earlier and produced `MacError(NoData)`, so
  the stack was reverted to polling.
* The device is mains powered (USB), so sleeping is a choice, not a necessity.
* After a rejoin the coordinator's cached short address goes stale; the log line
  `Request network address of ...` right after the failure is that recovery.
  Every flash/reset during development triggered it, but the user also sees
  stale/late commands in normal use.
* RSSI around −77..−81 dBm, LQI 222..225 — usable but not great, and the device
  sits inside a cabinet of hardware.
* Each RF command keeps the CC1101 busy for ~400 ms (8 frames × 39 ms + gaps).
  In async mode the loop yields between edges, so the stack can poll, but the
  main loop is still spending 400 ms per command on RF.

**Things to try, cheapest first.**

1. Measure it: publish N speed commands (e.g. 50, one per second) with a unique
   fan, count successes/latencies/`MAC_TRANSACTION_EXPIRED` in the Z2M log.
   Do it with the device untouched (no flashing) and again while it is
   transmitting RF, to separate "stale address after rejoin" from "busy
   device".
2. Poll faster (`TimingConfig { poll_interval_ms: 50 }`) and see whether the
   expiry rate changes.
3. Try `rx-on-when-idle` again (`0x88`) now that the stack has moved on — if the
   `MacError(NoData)` is gone, a mains-powered device should simply stay awake.
4. Check the parent: `database.db` has the parent address; if it is a weak
   router, force a rejoin near the coordinator and compare.
5. If the stale-address recovery is the dominant failure: make the device send a
   `Device_annce` after every rejoin (it should already) and check when it
   doesn't.

**Done when.** 100 sequential commands from Home Assistant all apply within a
second or two, with no expired transactions in the Z2M log.

---

## 2. Console commands do not update the stored state — **S**

`speed <slot> <n>`, `dir <slot> …` and `power <slot> …` transmit the infrared…
er, RF code but leave `cfg.slots[n]` untouched. So after using the console the
device still reports the old speed/direction/power — and Home Assistant shows a
stale value until the next Zigbee write or a remote press. The Zigbee path does
update the state correctly.

Fix: update the slot exactly like the cluster handlers do (respecting the "turn
on when a speed is chosen" rule), save, and then transmit.

---

## 3. `debug` is a toggle, not a setter — **S**

`debug` flips the flag, so a scripted `debug on` can silently turn debugging
*off* (this bit us twice while capturing evidence). Make it
`debug [on|off|status]`, mirroring `txmode`.

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

## 8. Tooling: the console driver — **S**

`/tmp/fan.py` (scheduled commands + timestamped output, one process owns the
port, `-p PORT`, `-r` reset, `-t seconds`) was written to stop fighting
`cat`/`printf` over ssh. It is now the tool used for every hardware test. Worth
committing as `tools/fan.py` with a short usage note in the README.

Related lesson: background captures must run `python3 -u`, otherwise the output
is block-buffered and the file stays empty until the process exits.

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

## 12. Cosmetic leftovers — **S**

* Stale Home Assistant entities from the old 17-endpoint layout (`switch_10`,
  `switch_11`, `state_12`, `power_on_behavior_*`) had to be cleared by hand with
  retained empty discovery payloads. A note in the README about what to do after
  an endpoint-set change would save that.
* `speed_2_2` / `state_2_2` style keys appeared once when the converter returned
  already-postfixed properties; Zigbee2MQTT postfixes `toZigbee` results itself.
  Documented here so nobody debugs it twice.
