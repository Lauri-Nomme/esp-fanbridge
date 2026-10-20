#!/usr/bin/env python3
"""Drive a serial console without shell quoting games.

usage: sudo ./fan.py [-p PORT] [-t SECONDS] [-r] "DELAY:command" ...

  -p PORT     serial port (default /dev/ttyACM0)
  -t SECONDS  how long to listen (default 6)
  -r          reset the board first via DTR/RTS (ESP auto-reset)
  DELAY:cmd   send `cmd` DELAY seconds after the start, e.g. "1.5:list"

Use `python3 -u` when redirecting to a file, otherwise the output is
block-buffered and the file stays empty until the process exits.

Each argument is scheduled relative to the start: a delay in seconds, a colon,
then the line to send. Everything the device prints is echoed back, prefixed
with the elapsed time, so ordering against the commands is obvious.
"""
import fcntl
import os
import select
import struct
import sys
import termios
import time


def main(argv):
    port = "/dev/ttyACM0"
    duration = 6.0
    args = argv[1:]

    reset = False
    while args and args[0].startswith("-"):
        if args[0] in ("-p", "--port"):
            port = args[1]
        elif args[0] in ("-t", "--time"):
            duration = float(args[1])
        elif args[0] in ("-r", "--reset"):
            reset = True
            args = args[1:]
            continue
        else:
            sys.exit(f"unknown option {args[0]}")
        args = args[2:]

    plan = []
    for arg in args:
        delay, sep, cmd = arg.partition(":")
        if not sep:
            sys.exit(f"bad argument {arg!r}, want DELAY:command")
        plan.append((float(delay), cmd))
    plan.sort()

    fd = os.open(port, os.O_RDWR | os.O_NOCTTY)
    if reset:
        # classic ESP auto-reset: EN low while the USB CDC control lines move
        fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("I", termios.TIOCM_DTR))
        fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("I", termios.TIOCM_RTS))
        time.sleep(0.15)
        fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("I", termios.TIOCM_RTS))
        time.sleep(0.05)
        fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("I", termios.TIOCM_DTR))
    start = time.monotonic()
    buf = b""
    try:
        while True:
            now = time.monotonic() - start
            if now >= duration:
                break
            while plan and plan[0][0] <= now:
                _, cmd = plan.pop(0)
                print(f"{now:6.1f}s >>> {cmd}")
                os.write(fd, (cmd + "\r").encode())
            ready, _, _ = select.select([fd], [], [], 0.02)
            if not ready:
                continue
            try:
                data = os.read(fd, 8192)
            except OSError:
                break
            if not data:
                break
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                text = line.decode("utf-8", "replace").replace("\r", "")
                if text.strip():
                    print(f"{now:6.1f}s     {text}")
            if len(buf) > 8192:
                print(f"{now:6.1f}s     {buf.decode('utf-8', 'replace')}")
                buf = b""
    finally:
        os.close(fd)


if __name__ == "__main__":
    main(sys.argv)
