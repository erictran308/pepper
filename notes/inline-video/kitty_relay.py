"""Play a test pattern inside a box of the terminal through mpv's kitty output,
relayed the way tuitube would: mpv writes to a pipe, this script passes it on,
fixing mpv 0.41's shared-memory name (no leading '/', which macOS refuses) and
dropping its stray `m=1`. q quits (or Ctrl-C)."""

import base64
import fcntl
import os
import re
import signal
import struct
import subprocess
import sys
import termios

MPV = "/opt/homebrew/bin/mpv"
LEFT, TOP, COLS, ROWS = 10, 5, 60, 17
SOURCE = "av://lavfi:testsrc2=size=1280x720:rate=30"
LOG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "kitty_relay.log")

SHM = re.compile(rb"\x1b_G(a=T,t=s,[^;]*);([A-Za-z0-9+/=]+)\x1b\\")


def cell_size():
    try:
        rows, cols, xpix, ypix = struct.unpack(
            "HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))
        if xpix and ypix:
            return xpix / cols, ypix / rows
    except OSError:
        pass
    return 10.0, 20.0


def fix(match, stats):
    keys = match.group(1).replace(b",m=1", b"")
    name = base64.b64decode(match.group(2))
    if not name.startswith(b"/"):
        name = b"/" + name
        stats["fixed"] += 1
    stats["frames"] += 1
    return b"\x1b_G" + keys + b";" + base64.b64encode(name) + b"\x1b\\"


def main():
    cw, ch = cell_size()
    width, height = int(COLS * cw), int(ROWS * ch)
    mpv = subprocess.Popen(
        [MPV, "--no-config", "--really-quiet", "--profile=sw-fast",
         "--vo=kitty", "--vo-kitty-use-shm=yes",
         "--vo-kitty-alt-screen=no", "--vo-kitty-config-clear=no",
         f"--vo-kitty-left={LEFT}", f"--vo-kitty-top={TOP}",
         f"--vo-kitty-cols={COLS}", f"--vo-kitty-rows={ROWS}",
         f"--vo-kitty-width={width}", f"--vo-kitty-height={height}",
         "--", SOURCE],
        stdout=subprocess.PIPE)
    signal.signal(signal.SIGINT, lambda *_: mpv.terminate())
    stats = {"frames": 0, "fixed": 0, "bytes": 0}
    pending = b""
    out = sys.stdout.fileno()
    while True:
        try:
            data = os.read(mpv.stdout.fileno(), 65536)
        except InterruptedError:
            continue
        if not data:
            break
        stats["bytes"] += len(data)
        pending += data
        # Hold back a sequence that may not have fully arrived yet.
        cut = pending.rfind(b"\x1b_G")
        if cut != -1 and pending.find(b"\x1b\\", cut) == -1:
            ready, pending = pending[:cut], pending[cut:]
        else:
            ready, pending = pending, b""
        os.write(out, SHM.sub(lambda m: fix(m, stats), ready))
    # mpv 0.41 ends without closing its last delete command.
    os.write(out, pending + b"\x1b\\\x1b[?25h\x1b[?1003l")
    mpv.wait()
    with open(LOG, "w") as log:
        log.write(f"cell {cw:.1f}x{ch:.1f} px, box {width}x{height} px, "
                  f"{stats['frames']} frames, {stats['fixed']} names fixed, "
                  f"{stats['bytes']} bytes from mpv, mpv exit {mpv.returncode}\n")


main()
