# Releasing

A release is a git tag on `main`. The `Release` workflow builds the firmware,
runs the protocol tests, and attaches three files:

| File | What to do with it |
|---|---|
| `esp-fanbridge-vX.Y.Z.app.bin` | the application, flash at `0x10000`; keeps the pairing config |
| `esp-fanbridge-vX.Y.Z.elf` | for the usual `espflash flash`; keeps the pairing config |
| `esp-fanbridge-vX.Y.Z.full.bin` | bootloader + partition table + app at `0x0`; for a blank board, **erases the config** |
| `esp-fanbridge-vX.Y.Z.js` | the Zigbee2MQTT converter that matches this firmware |

The config sector is `0x3E_0000`, the last sector of the `factory` partition
(`0x10000..0x400000`), which is why the app image and the ELF leave it alone and
the full image does not. Back a pairing up with `cfg dump` before a full flash and
restore it with `cfg load`.

## Cutting one

```sh
git checkout main && git pull
git tag -a v0.2.0 -m "v0.2.0"
git push origin v0.2.0
```

Watch the run, then check the assets: the `.app.bin` should be roughly the size
of the application (under a megabyte today), and the `.full.bin` is padded to the
whole flash — **4 MiB** on this board. A padded size of 8 MiB means the flash size
defaulted wrong.

## Flashing a release

```sh
espflash write-bin --port /dev/ttyACM0 0x10000 esp-fanbridge-v0.2.0.app.bin
espflash flash    --port /dev/ttyACM0         esp-fanbridge-v0.2.0.elf
```

Both keep the config sector (`0x3E_0000`), so the learned remotes survive. A
release that changes the *endpoint set* still needs a re-interview, which the
converter requests on its own when the device announces a different count.

## Version numbers

`Cargo.toml`'s `version` is what the firmware prints at boot (`0.1.0+<sha>`), so
bump it in the release commit. The tag and the crate version should not drift:
the boot banner is the first thing to check when a device behaves like an older
build than expected.
