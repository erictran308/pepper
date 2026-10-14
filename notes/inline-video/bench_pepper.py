"""What playing a video costs: inside Pepper through tuitube-video, or in
mpv's own window, at the same sizes. Each run plays the same 1080p H.264 file
for SECONDS with tuitube's --hwdec=auto-safe and no sound (--ao=null, as
bench.py), and measures the CPU time of the player, Pepper and WindowServer,
the graphics chip's use (ioreg's Device Utilization %, sampled), and the
largest memory of the player and Pepper.

Run it in a Pepper tab, as large as you use it, and don't touch anything:

    python3 -I bench_pepper.py [HELPER] [VIDEO]

HELPER defaults to ../../../tuitube/target/release/tuitube-video, VIDEO to
test.mp4 beside this file (or TT_BENCH_VIDEO); bench.py says how to make it.
"""

import fcntl
import json
import os
import re
import secrets
import socket
import struct
import subprocess
import sys
import termios
import threading
import time

MPV = "/opt/homebrew/bin/mpv"
HERE = os.path.dirname(os.path.abspath(__file__))
HELPER = os.path.abspath(os.path.join(HERE, "../../../tuitube/target/release/tuitube-video"))
VIDEO = os.environ.get("TT_BENCH_VIDEO", os.path.join(HERE, "test.mp4"))
LOG = os.path.join(HERE, "bench_pepper.log")
SECONDS = 15
COMMON = ["--no-config", "--ao=null", "--hwdec=auto-safe", f"--end={SECONDS}"]
GPU = re.compile(r'"Device Utilization %"=(\d+)')


def cpu_seconds(pid):
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)],
                         capture_output=True, text=True).stdout.strip()
    total = 0.0
    for part in out.split(":"):
        total = total * 60 + float(part)
    return total


def rss_mb(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                         capture_output=True, text=True).stdout.strip()
    return int(out) / 1024 if out else 0.0


def gpu_percent():
    out = subprocess.run(["/usr/sbin/ioreg", "-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"],
                         capture_output=True, text=True).stdout
    found = GPU.findall(out)
    return float(found[0]) if found else 0.0


def terminal():
    rows, cols, xpix, ypix = struct.unpack(
        "HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))
    return rows, cols, xpix / cols, ypix / rows


def say(text):
    os.write(1, f"\x1b[H\x1b[2J{text}\r\n".encode())


class Measure:
    """CPU time, peak memory and the graphics chip's use while a run lasts."""

    def __init__(self, pids):
        self.pids = pids
        self.before = {k: cpu_seconds(p) for k, p in pids.items()}
        self.peak = {"player": 0.0, "pepper": rss_mb(pids["pepper"])}
        self.gpu = []
        self.player = None
        self.done = threading.Event()
        self.start = time.monotonic()
        self.thread = threading.Thread(target=self.sample)
        self.thread.start()

    def sample(self):
        while not self.done.wait(0.5):
            self.gpu.append(gpu_percent())
            if self.player:
                self.peak["player"] = max(self.peak["player"], rss_mb(self.player))
            self.peak["pepper"] = max(self.peak["pepper"], rss_mb(self.pids["pepper"]))

    def stop(self, usage=None):
        """`usage`: the player's own, from wait4 (the samplers' ps and ioreg
        are children too, so all children's together would count them)."""
        wall = time.monotonic() - self.start
        self.done.set()
        self.thread.join()
        player = usage.ru_utime + usage.ru_stime if usage else 0.0
        used = {k: cpu_seconds(p) - self.before[k] for k, p in self.pids.items()}
        return {
            "wall": wall,
            "player": 100 * player / wall,
            "pepper": 100 * used["pepper"] / wall,
            "windowserver": 100 * used["windowserver"] / wall,
            "gpu": sum(self.gpu) / len(self.gpu) if self.gpu else 0.0,
            "player_mb": self.peak["player"],
            "pepper_mb": self.peak["pepper"],
        }


def inline(name, col, row, cols, rows, cw, ch, pids):
    """tuitube-video into a box of cells, placed as tuitube places it."""
    token = secrets.token_hex(16)
    width, height = round(cols * cw), round(rows * ch)
    ours, theirs = socket.socketpair()
    env = {"HOME": os.environ["HOME"], "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
           "PEPPER_PORT": os.environ["PEPPER_PORT"]}
    os.write(1, f"\x1b]777;pepper;place;{token};{col};{row};{cols};{rows}\x1b\\".encode())
    measure = Measure(pids)
    helper = subprocess.Popen(
        [HELPER, *COMMON, "--terminal=no", "--idle=once", "--input-ipc-client=fd://3"],
        env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE, close_fds=False,
        preexec_fn=lambda: os.dup2(theirs.fileno(), 3))
    theirs.close()
    measure.player = helper.pid
    hwdec = {}

    def read():
        for line in ours.makefile("rb"):
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if message.get("event") == "playback-restart":
                ours.sendall(b'{"command":["get_property","hwdec-current"],"request_id":1}\n')
            if message.get("request_id") == 1:
                hwdec["current"] = message.get("data")

    threading.Thread(target=read, daemon=True).start()
    for command in (["script-message-to", "main", "pepper-size", str(width), str(height)],
                    ["script-message-to", "main", "pepper-token", token],
                    ["loadfile", VIDEO, "replace"]):
        ours.sendall((json.dumps({"command": command}) + "\n").encode())
    err = []
    reader = threading.Thread(target=lambda: err.append(helper.stderr.read()))
    reader.start()
    _, status, usage = os.wait4(helper.pid, 0)
    helper.returncode = os.waitstatus_to_exitcode(status)
    result = measure.stop(usage)
    reader.join()
    stderr = b"".join(err).decode(errors="replace")
    ours.close()
    os.write(1, f"\x1b]777;pepper;remove;{token}\x1b\\".encode())
    shown = re.search(r"(\d+) frames drawn, (\d+) shown", stderr)
    result.update(run=f"Pepper {width}x{height}",
                  fps=int(shown.group(2)) / result["wall"] if shown else None,
                  hwdec=hwdec.get("current") or "?")
    return result


def window(name, width, height, pids):
    """mpv's own window, as wide in pixels (its geometry is in points)."""
    log = os.path.join(HERE, f"bench_pepper-{name}.mpvlog")
    measure = Measure(pids)
    mpv = subprocess.Popen([MPV, *COMMON, "--really-quiet", f"--log-file={log}",
                            f"--geometry={width // 2}x{height // 2}", "--force-window-position",
                            "--", VIDEO], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
    measure.player = mpv.pid
    _, status, usage = os.wait4(mpv.pid, 0)
    mpv.returncode = os.waitstatus_to_exitcode(status)
    result = measure.stop(usage)
    with open(log, errors="replace") as f:
        hw = re.findall(r"Using hardware decoding \(([^)]*)\)", f.read())
    result.update(run=f"window {width}x{height}", fps=None, hwdec=hw[0] if hw else "no")
    return result


def main():
    helper = sys.argv[1] if len(sys.argv) > 1 else HELPER
    globals()["HELPER"] = helper
    if len(sys.argv) > 2:
        globals()["VIDEO"] = sys.argv[2]
    if not os.environ.get("PEPPER_PORT"):
        sys.exit("run this in a Pepper tab (PEPPER_PORT is not set)")
    for path in (HELPER, VIDEO):
        if not os.path.exists(path):
            sys.exit(f"not found: {path}")
    pids = {"pepper": int(os.environ["PEPPER_PORT"].rsplit(".", 1)[1]),
            "windowserver": int(subprocess.run(["pgrep", "-x", "WindowServer"],
                                               capture_output=True, text=True).stdout.split()[0])}
    rows, cols, cw, ch = terminal()
    small = (60, 17)
    large = (max(20, cols - 12), max(8, rows - 8))
    results = []

    say(f"1/5 idle baseline, {SECONDS} s (don't touch anything)")
    measure = Measure(pids)
    time.sleep(SECONDS)
    idle = measure.stop()
    idle.update(run="idle", fps=None, hwdec="-")
    results.append(idle)

    for i, (bc, br) in enumerate((small, large)):
        say(f"{2 + i}/5 Pepper, a {bc}x{br} cell box, {SECONDS} s")
        results.append(inline(f"inline-{i}", 6, 3, bc, br, cw, ch, pids))
    for i, (bc, br) in enumerate((small, large)):
        w, h = round(bc * cw), round(br * ch)
        say(f"{4 + i}/5 mpv's own window, {w}x{h} px, {SECONDS} s (it opens over this)")
        results.append(window(f"window-{i}", w, h, pids))

    lines = ["run                   player%  Pepper%  windowserver%  gpu%  player MB  Pepper MB  fps  hwdec"]
    for r in results:
        fps = f"{r['fps']:.0f}" if r["fps"] is not None else "-"
        lines.append(f"{r['run']:<21} {r['player']:>7.1f}  {r['pepper']:>7.1f}  {r['windowserver']:>13.1f}"
                     f"  {r['gpu']:>4.0f}  {r['player_mb']:>9.0f}  {r['pepper_mb']:>9.0f}  {fps:>3}  {r['hwdec']}")
    report = "\n".join(lines)
    with open(LOG, "w") as f:
        f.write(report + "\n")
    say(report.replace("\n", "\r\n"))


main()
