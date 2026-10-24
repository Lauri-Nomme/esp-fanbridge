# Releasing

A release is a git tag on `main`. The `Release` workflow builds the firmware,
runs the protocol tests, and attaches three files:

| File | What to do with it |
|---|---|
| `esp-fanbridge-vX.Y.Z.bin` | merged image (bootloader + partition table + app), flash at `0x0` |
| `esp-fanbridge-vX.Y.Z.elf` | the application, for the usual `espflash flash` |
| `esp-fanbridge-vX.Y.Z.js` | the Zigbee2MQTT converter that matches this firmware |

## Cutting one

```sh
git checkout main && git pull
git tag -a v0.2.0 -m "v0.2.0"
git push origin v0.2.0
```

Watch the run, then check that all three assets are attached and that the `.bin`
size looks right (roughly 600 KiB with the current partition table, not 8 MiB —
an oversized image usually means the flash size defaulted wrong).

## Flashing a release

```sh
espflash write-bin --port /dev/ttyACM0 0x0 esp-fanbridge-v0.2.0.bin
espflash flash    --port /dev/ttyACM0     esp-fanbridge-v0.2.0.elf
```

Both keep the config sector (`0x3E_0000`), so the learned remotes survive. A
release that changes the *endpoint set* still needs a re-interview, which the
converter requests on its own when the device announces a different count.

## Version numbers

`Cargo.toml`'s `version` is what the firmware prints at boot (`0.1.0+<sha>`), so
bump it in the release commit. The tag and the crate version should not drift:
the boot banner is the first thing to check when a device behaves like an older
build than expected.
