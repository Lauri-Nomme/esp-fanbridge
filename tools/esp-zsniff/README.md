# esp-zsniff — a minimal IEEE 802.15.4 sniffer

Prints one line per frame on the chosen channel with what is readable without the
network key: frame length, RSSI, MAC sequence and addresses, and — for secured
frames — the NWK sequence number and the **security frame counter**. Payloads are
encrypted and stay opaque.

It is the tool that found the two most stubborn bugs in this project:

* a device that acknowledged frames at the MAC and never handled them (the
  console was blocking: backlog #51), and
* frames whose counters and addresses proved the coordinator was talking to the
  wrong channel after a reboot (backlog #50).

Flash it to a *second* ESP32-C6 board, set `CHANNEL` to the network's channel,
and watch the console:

```
zsniff on channel 25
rx len=50 fcf=8861 seq=156 dst=14da src=0000 nwkfcf=0248 srcref=0 nwkseq=30 sec=1 fc=2540775630
rx len=12 fcf=8863 seq=157 dst=0000 src=14da nwkseq=0 sec=0 fc=0
```

`dst=`/`src=` are the 16-bit network addresses, `dst=0000` is the coordinator.
A 12-byte frame addressed to the coordinator is a poll/keepalive; 40–70 byte
secured frames are the interesting ones.

```sh
cargo build --release            # from this directory
espflash flash --port /dev/ttyACM1 target/riscv32imac-unknown-none-elf/release/esp-zsniff
```

It listens promiscuously and never acknowledges, so it does not disturb the
network beyond sharing the channel.
