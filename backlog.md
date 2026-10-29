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

**Resolved (2026-10-25), by going the other way.** The device briefly declared
`rxOnWhenIdle` and stopped polling; the parent kept queueing frames for a polling
child, and commands expired (`MAC_TRANSACTION_EXPIRED`) — the "it bounces back"
symptom. It polls again (100 ms, the path measured at 25/25 and 30/30), which is
what the parent negotiates (`nwkParentInformation = 0x01`). See #25 for the
reversal and #46 for what is still unexplained about the link itself.

**Left to consider.**

* Boot-to-joined is ~6 s; the expensive part is radio bring-up/calibration, not
  something the application can skip.

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

## 5. Packet mode is still ignored by one fan — **answered: no, keep bit-bang**

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

**The experiment is now one command per fan** (backlog #58): `txmode <slot>
packet|async` overrides the transmit mode for one slot, this boot only, so `lr-r`
can be tried in packet mode while the other two keep the bit-bang path that works
for them. Before that, the mode was global and every packet-mode test silently
broke the fans that had been working.

**Result (2026-10-30), measured with the per-slot switch:** `lr-r` ignored the
packet frame twice out of two, while `lr-l` obeyed the identical packet frame
(1/1) and `lr-r` obeyed the bit-bang control both times. Three packet attempts
across two builds, zero reactions - that receiver rejects the packet engine's
waveform, and it is not the firmware, the flag or the radio.

Timing accuracy is excluded (packet mode is the more accurate path), so the
difference is the burst envelope - most plausibly the first symbol, where the
packet path has the PA ramping through the first 400 us pulse while the bit-bang
path holds the PA up for 2 ms first. Not worth confirming: packet mode has no
measured benefit over the proven path.

**Decision: `async` stays the default and #18 is not worth building for its own
sake.** The per-slot switch (#58) stays because it makes the question cheap to
re-ask if a receiver is ever replaced. Details: `docs/PACKET-TX.md` §8.2.

---

## 6. Pairing changes need a restart + re-interview — **done**

The endpoint table is built at boot, so a pairing change needs a restart and a
fresh interview. Both halves are now automatic: the device restarts three
seconds after learning a remote, the converter re-interviews when it notices the
announced count changed, and a *deletion* can be applied on demand through
`apply_pairing_change` (a `reboot` command on the pairing cluster, exposed as a
select in Home Assistant). `paired_count` vs `exposed_count` and the
`restart_required` binary sensor still show when something is pending.

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

### 43. Tests for the converter's payload mapping — **done**

`z2m/test.js`, 16 checks against the real `zigbee-herdsman-converters` (pinned in
`z2m/package.json`), covering the two bugs that actually reached the fan - a
`state` write going to FanMode on an endpoint without a fan controller, and a
stopped fan reporting speed 0 - plus the speed 4 ↔ `on` mapping, direction
values, the pairing payload, clamping, and the exposes of a device with and
without endpoints. CI runs it in its own job.

### 44. Rejoin from the console — **done**

`rejoin` re-associates (same keys, fresh child entry). Motivated by a real
symptom: writes from Home Assistant timing out with `MAC_TRANSACTION_EXPIRED`
while reports flowed fine, and a burst of commands arriving in a clump tens of
seconds later - the parent was queueing for a sleepy child, because the device
had only ever *resumed* since it gained `rxOnWhenIdle`. After `rejoin`, writes
were answered one-for-one in under a second.

### 56. The stack still printed straight into the USB FIFO — **fixed**

#52 moved the stack's `log` output into the ring, but `src/zigbee/mod.rs` kept
five `println!` calls: the report failures, the announce results and the
"stack stopped" line. `println!` writes into the USB FIFO directly, so those were
the last lines that could (a) block the stack task in the #51 way and (b) land in
the middle of a console line - observed as `zb: slot 2 "brrep fail: NoAck`, which
looks like a corrupt frame and is not.

All five now go through `log` (`warn`/`info`) and therefore the ring. Only the
boot banner and the panic handler still use `println!`: both run once, before the
ring exists, and the whole banner is ~200 bytes - less than the FIFO plus the
USB endpoint buffers, which is why a reset with a host attached but never reading
still boots and answers (measured: 843 ms for a ZCL read).

### 59. An occasional burst is simply not obeyed — **open, measured**

On 2026-10-30 a single async `speed 4` command to `lr-r` was transmitted
(`tx 0x191D07`, device state updated, `tx_dropped` 0) and the fan did not move.
Three more identical commands five seconds apart and it went to 100 % - so at
least one landed and the first did not.

That is a different failure from #54: the queue delivered the command to the
radio, the radio sent 8 frames, and the receiver ignored all of them. Nothing in
the firmware can see it, which is why `tx_dropped` alone is not evidence that the
fan obeyed.

Not yet measured: the rate. Candidates:

* the link is marginal at that spot (the fan is across the flat; the remotes' `rx`
  RSSI is only a proxy, since the fan never transmits),
* the receiver needs a longer gap between bursts than 12 ms, or more frames per
  burst (`repeats`, console knob, 8 by default),
* the burst shape: our frames measure ~2 % shorter than the remote's 415/1199 us,
  which a picky decoder might not tolerate.

Next is a counted experiment, not a fix: N = 10 single commands per mode, each
from the same known state (25 %), with a human confirming each step. A rate beats
an anecdote, and only then is "send each command twice" or "more repeats"
justified rather than guessed.

### 58. Per-slot transmit mode — **done**

`txmode` was global: trying the packet engine on one fan put *every* command on
it, so a test that half-worked also broke the fans that had been fine. The mode
is now resolved per slot when the code is queued (`txmode <slot> packet|async`,
this boot only), so the experiment is one fan, one mode, one command.

The flag rides in the queued word (bit 31), which the queue already had room for;
nothing about the default changed - `txmode` still selects the bit-bang path.

### 55. See the dropped-command counter from Home Assistant — **done**

The queue's drop count was console-only, i.e. invisible in normal operation - the
one silent failure this device has was only visible to someone with a serial
cable attached. It is now `txDropped` (0x0005) on the `espPairing` cluster, carried
by the same report as `rxRearmCount`, and exposed as the diagnostic sensor
`tx_dropped`, which should sit at 0 forever.

Verified on hardware: a twelve-write burst produced `txDropped: 3` in the
spontaneous report and `sensor.0x9c139efffecb054c_tx_dropped = 3` in Home
Assistant.

### 54. Commands were dropped between the ZCL reply and the air — **fixed**

Symptom: the noon schedule set three fans to 100% with directions in/out/in. The
fans started, but `br` was turning the wrong way and `lr-l` was still at its old
speed. Home Assistant showed all six values applied - and so did the device.

Cause: the transmit request was a **single 32-bit slot** (`TX_REQ`). `apply()`
stores the code and the ZCL write is answered immediately, while the main loop
then holds the slot for ~400 ms while one code goes out (8 repeats). Every command
that arrived during those 400 ms overwrote the pending one and was never
transmitted, with no error anywhere: the write response, the attribute report and
Home Assistant all described the device's bookkeeping, not the fan.

Reproduced on hardware: six writes 30 ms apart - the schedule's burst - produced
**two** transmissions. `debug on` showed four `zb: slot ...` lines with no `tx`
line after them.

Fix: `TxQueue`, an eight-entry FIFO (about three seconds of air) drained in order
by the main loop, so speed-then-direction for the same fan stays in order. On
overflow the oldest code goes and the loss is counted: one console line when it
happens, and `txq <waiting>/<dropped>` in `status`. Absolute commands make
dropping the oldest safe - the newest is the better description of what is wanted.

Verified on hardware: the six-write burst now produces six transmissions in order
(`0x191D07`, `0x191D08`, `0x068007`, `0x068004`, `0x1A5F07`, `0x1A5F08`); a
twelve-write burst produces nine transmissions plus `tx queue full: 3 command(s)
dropped` and `txq 0/3`.

This is also why the converter's read-back could not catch it: it reads the
*device's* attribute, which `apply()` has already updated, so it confirms the
bookkeeping rather than the fan.

### 53. Console lines longer than the FIFO were cut in half — **fixed**

Writing straight into the 64-byte USB FIFO meant the tail of every longer line was
dropped - and the drop happened mid-line. `status` stopped after `| async`, so the
frequency, the debug flag and the pairing flag were never visible on a device that
had been up for a while; a `slots` dump lost whole entries and glued the remaining
ones together. It read like corruption rather than truncation, which is why it was
mistaken for a dying USB link more than once.

Output is now staged in a 192-byte ring and pushed by `flush()` as the FIFO
drains - once per main-loop iteration, and once per line inside `outln` - so a
line is whole whenever the host reads at all, and nothing blocks when it does not.
Bytes are only lost when the host stops reading entirely; `status` counts those
chunks (`out N`). `line` and `status` buffers are sized per line with one
const-generic writer instead of three copy-pasted ones, and `status` now fits.

### 52. The stack's own log path could wedge it too — **fixed**

Fixing the console (#51) left `log` alone: the stack logs through the `log` crate
and the default backend writes straight into the USB FIFO, so a debug-level build
with nobody reading could still block the stack task. The firmware now installs
its own backend that formats into a 2 KiB ring, drained by the main loop into the
(dropping) console. Level is runtime-switchable with `log <level>`, and `status`
reports how many bytes were dropped.

### 51. THE root cause: console output blocked and wedged the device — **fixed**

For days the symptom was "flipping a fan in Home Assistant bounces back", and it
was reproducible only when *nobody was reading the serial console*. That was the
clue I kept discarding: every debugging session attached a console and everything
worked.

The device's console (and every `println!`) writes into the USB-Serial-JTAG's
64-byte FIFO, and `write` waits for room. With a host enumerated but not reading -
a serial reader left attached, or nothing ever draining - the FIFO fills and the
write blocks forever. Whatever task prints then stops: the Zigbee stack stops
dispatching, so commands arrive, get MAC-acked and are never handled, and the
coordinator retries until it gives up. The radio is fine, which is why reports
already queued still trickled out and made this look like a delivery problem.

**Measured:** kill the serial reader mid-soak and the device stops answering
within 90 seconds (5 checks, 3 failed, `device=null` on every read). After the fix
(`write_byte_nb`, drop when full) the same soak ran 19/19, then 60/60, with no
reader attached at all.

### 50. Root cause: the stack resumed on the wrong channel and then threw the network away — **fixed in the fork**

Two stack bugs explain why a rebooted device stopped accepting commands while its
reports kept working:

1. **Resume used `channels.start` as the channel.** Once channels became a range
   (11..27, so a channel move needs no reflash), a network on 25 was resumed on
   11. The rejoin went out on the wrong channel, nothing answered, and the device
   fell back to *commissioning* - which needs a permit-join a rebooted device
   should not need.
2. **A refused rejoin forgot the network.** `commission` called
   `forget_network()` on any rejoin failure. Since that runs on every boot, one
   refusal - a busy parent, a coordinator that restarted, a stale child entry -
   left the device off the network until somebody opened a join window. That is
   the bouncing: HA publishes, the device is not on the network, the state flips
   back, and reports that were queued still trickle out, which makes it look like
   a delivery problem.

Both are fixed in `Lauri-Nomme/zigbee-rs`, branch `fanbridge-fixes`, together with
a scan fallback for the BDB rejoin and a non-fatal transport-key poll (the receive
loop can install the key first). The firmware now depends on that revision.

Verified on hardware with the network on channel 25: after a reboot the log shows
`network found on channel 25` and `resuming on network: addr=0x14da ch 25`; a
refused rejoin logs `keeping the network (retrying)`; and a command sent
afterwards applies (`tx` climbs, 4 frames reach the coordinator).

### 49. The device goes deaf while an acknowledged send waits — **superseded by #50**

After a fresh commission everything works: commands apply (`tx` climbs), reports
are acknowledged, `writeRsp` arrives. Hours (or a reboot) later the same device
delivers the coordinator's frames to its MAC (the coordinator's `dataConfirm`
says `status: 0`, and the device's own `tx` stays at 0) and answers nothing.

The most consistent explanation left: the *acknowledged* send used by reports
holds the radio lock while it waits for an APS acknowledgement (up to
`apscMaxFrameRetries` × the ack timeout). While that lock is held the receive
loop cannot drain, so frames queue and are dropped - and the queue is short. The
reply path is fire-and-forget (`unicast_data`), which is why replies are *also*
lost but without blocking anything.

Evidence for it: the sniffer shows the coordinator's frames arriving at the
device with increasing security frame counters, the device's MAC acknowledging
them (`dataConfirm status: 0`), and no frames coming back at all - a device that
is alive on the radio but not processing.

Next: instrument the send path (log when the radio lock is taken and released,
and for how long), reproduce the degraded state, and then fix it - either bound
the wait, or do not hold the lock across the ack wait, or send reports without
expecting an acknowledgement.

### 48. A refused join rebooted the device — **fixed, and it was today's "bounce"**

The stack task reset the device whenever the stack stopped. After the channel move
the device needed a fresh commission, the join window was closed, the coordinator
answered `NotPermitted`, and the firmware reset itself — every 3.7 seconds. From
outside that looks like a device that is *there* but ignores commands: Home
Assistant publishes, the write fails, the state flips back. It was diagnosed with
the 802.15.4 sniffer example (`c6/firmware/examples/esp-zsniff`): the coordinator's
frames reach the device (`dataConfirm status: 0`) and the device answers nothing,
because it is rebooting before it can.

Fixed: the stack task retries with a backoff (5 s doubling to 120 s) and keeps
running, printing the reason (`permit_joining=false` → open the join window). A
device that cannot join is now recoverable instead of a reboot loop.

Measured afterwards, on channel 25: 22 reports acknowledged, 2 real
transmissions, writes applied - the link is healthy.

### 47. Zigbee channel moved 11 → 25 — **done, devices must re-pair**

WiFi around the flat is crowded on channel 6 (three APs, the strongest -38 dBm from
the IoT SSID itself); Zigbee 11 sat 22 MHz away and Zigbee traffic was intermittent
in both directions. The network is now on **channel 25** (2475 MHz, ~28 MHz above
the strong AP and ~3 MHz above a weak ch-11 one), same PAN and extended PAN, so
nothing about the identity changed.

Everything on the old channel has to re-pair: the fan bridge rejoined on its own
(`leave` → commission), the other end devices need their pairing button. The
firmware now scans channels 11..26 when commissioning, so a future channel move
needs no reflash.

**Measured effect:** the fan bridge's attribute reports went from "arrive late, in
clumps, with failures" to **17/17 acknowledged in one flush**, so the RF link is
now good. Reply frames (`writeRsp`/`defaultRsp`) are still mostly lost - see #46,
which is now a clean signal rather than one buried in interference.

### 46. `rxOnWhenIdle` cannot be used: the transport-key poll loses the key — **blocker**

The parent negotiates "keepalive by MAC data poll" (`nwkParentInformation = 0x01`),
i.e. it queues frames for a polling child. Declaring `rxOnWhenIdle` makes the
stack stop polling, so those queued commands expire (`MAC_TRANSACTION_EXPIRED`) —
the "it bounces back" symptom. Measured tonight: replies 3-40 s late and in
clumps with rx-on, sub-second when polling.

But declaring **rx-off is not bootable on this stack**:

```
[MLME-ASSOCIATE] success, short_addr=ShortAddress(0x14da)
[BDB] step 9: poll for network key (transport-key)
   ... nothing. No key installed, no error surfaced.
```

The association succeeds, the coordinator sees the device, and every frame the
device sends then fails with `SecurityFail` — the network key never arrives,
because `poll_transport_key()` fetches it with `poll_nwk_data()` rather than the
receive loop. With rx-on the key arrives through the receive loop instead.

So the two requirements are incompatible today: rx-on joins but is delivered to
badly, rx-off is delivered to well but cannot join. Backlog #45 is the way out.
Until then the firmware keeps rx-on, because a device that joins and is
occasionally slow beats one that cannot join at all.

Side observations worth keeping:

* A join that starts with a device the coordinator *knows* never gets a network
  key ("secure rejoin failed (MacError)", then no transport key), so
  re-commissioning needs the device removed in Zigbee2MQTT first — DESIGN §9's
  first step is not optional.
* After such a re-commission the interview failed once ("can not get active
  endpoints") and succeeded on a retry — the timing sensitivity in DESIGN §8.
* `keepalive poll dropped a data frame` (stack, debug level): a frame retrieved
  by the keepalive poll is discarded. Rare with polling as the main path, but it
  is a lost command when it happens.

### 45. Router role instead of end device — **M, now the most promising fix**

A router is addressed directly: no indirect queue, no poll, and `rxOnWhenIdle` is
part of what a router *is*, so neither of the two problems in #46 applies. This
device is mains powered, so it is entitled to be one.

Costs: it would relay for others (a role change in the household mesh), and the
stack has to support router duties properly — worth watching whether other
devices route through it without trouble. `logical_type: LogicalType::Router`
plus the router capability bit is the switch.

### 42. Report counters and an on-demand flush — **done**

`status` carries `rep <ok>/<fail>` and `report` queues the full state flush that
follows a (re)join. Both exist to separate "the device is not reporting" from
"the coordinator is not listening" without waiting for a state change - the
first question after any "HA shows nothing" report.

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

### 16. Log RSSI per received frame — **done, then fixed**

`rx device=… button=… rssi=-74dBm`. The first version read the register once the
frame had ended - by which time the AGC has decayed (`AGCCTRL2=0x03`), so it
printed the noise floor for every frame. It now samples the RSSI register every
300 us while a frame is being captured and reports the **peak**, which is the
number that answers "how well did that remote arrive". `junk:` lines carry the
same level, so a burst that fails to decode can be told apart from noise.

The sampling read costs ~15 us and can delay the *capture* of one edge by that
much; it cannot lose an edge (they are >=250 us apart) and the decoder windows
(250..650, 850..1800 us) absorb it. Needs one remote press on hardware to confirm
the values track distance.

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

### 23. `fanModeSequence` attribute — **done**

Served as `0x00` (Low/Med/High). ZCL marks the attribute required, and the
interesting decision is the value: the fan has four speeds, but only three are
standard modes - the fourth is ZCL `on`, which no sequence value covers. The ZCL
default `0x02` claims `Low/Med/High/Auto`, an `auto` mode this fan does not have,
so `0x00` is the closest true statement. `on` is still accepted.

Verified end to end: a temporary read from Zigbee2MQTT after a device announce
returned `fanModeSequence=0` (the read is not part of the shipped converter).

### 24. Make Identify blink the LED — **S**

The Identify cluster is served, but the LED ignores it, so Z2M's "identify"
button does nothing visible. Blink for `identify_time` seconds.

### 25. Revisit `rx-on-when-idle` — **tried, reverted; do not retry without a reason**

The device is mains powered but declares itself sleepy (`0x80`). `0x88` failed
earlier with `MacError(NoData)`; the stack has changed a lot since. Staying awake
would remove polling latency entirely (see backlog §1).

**Outcome (2026-10-25).** Implemented, then reverted within a day. The stack
takes the `receive_and_dispatch` path when the capability bit is set, so it
stops polling; the coordinator, however, answers the end-device timeout request
with `nwkParentInformation = 0x01` - "keepalive by MAC data poll" - and keeps
queueing frames for a polling child. Commands then sat in its indirect queue
until they expired. Delivery latency went from sub-second to 3-40 s with
`MAC_TRANSACTION_EXPIRED` bursts. Polling at 100 ms is what this parent wants.

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

### 28. CI: build + clippy + fmt — **done**

CI now runs `cargo fmt --check` and `cargo clippy --release -- -D warnings` for
the firmware, and the protocol job runs both from the temp directory the tests
already use (`LINT=1 ./protocol/run-tests.sh`). Getting there meant one sweep:
`cargo fmt` across the tree (93 hunks, no semantic change) and the 16 clippy
warnings - collapsible `if`s, redundant references and casts, three `for i in
0..8` loops that could iterate, one useless operation. Verified after the sweep
on hardware: `list`, `desc`, `slots`, `cfg dump`, `status` all behave, and the
build is warning-free.

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
