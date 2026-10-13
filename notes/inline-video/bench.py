"""Compare what playing a video costs: mpv's own window, or mpv's kitty output
relayed into a box of this terminal. Each run plays the same 1080p H.264 file
for SECONDS with tuitube's --hwdec=auto-safe and measures the CPU time of mpv,
Ghostty and WindowServer, and the largest memory of mpv and Ghostty."""

import base64
import fcntl
import os
import re
import resource
import struct
import subprocess
import sys
import termios
import threading
import time

MPV = "/opt/homebrew/bin/mpv"
HERE = os.path.dirname(os.path.abspath(__file__))
VIDEO = os.environ.get("TT_BENCH_VIDEO", os.path.join(HERE, "test.mp4"))
LOG = os.path.join(HERE, "bench.log")
SECONDS = 15
COMMON = ["--no-config", "--really-quiet", "--ao=null", "--hwdec=auto-safe",
          f"--end={SECONDS}"]
SHM = re.compile(rb"\x1b_G(a=T,t=s,[^;]*);([A-Za-z0-9+/=]+)\x1b\\")


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


def ghostty_pid():
    pid = os.getppid()
    while pid > 1:
        ppid, comm = subprocess.run(["ps", "-o", "ppid=,comm=", "-p", str(pid)],
                                    capture_output=True, text=True).stdout.split(None, 1)
        if comm.strip().endswith("/ghostty"):
            return pid
        pid = int(ppid)
    sys.exit("run this inside Ghostty (not tmux)")


def terminal():
    rows, cols, xpix, ypix = struct.unpack(
        "HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))
    return rows, cols, xpix / cols, ypix / rows


def say(text):
    os.write(1, f"\x1b[H\x1b[2J{text}\r\n".encode())


def relay(mpv, stats):
    pending = b""

    def fix(m):
        stats["frames"] += 1
        name = base64.b64decode(m.group(2))
        if not name.startswith(b"/"):
            name = b"/" + name
        return (b"\x1b_G" + m.group(1).replace(b",m=1", b"") + b";"
                + base64.b64encode(name) + b"\x1b\\")

    while data := os.read(mpv.stdout.fileno(), 65536):
        pending += data
        cut = pending.rfind(b"\x1b_G")
        if cut != -1 and pending.find(b"\x1b\\", cut) == -1:
            ready, pending = pending[:cut], pending[cut:]
        else:
            ready, pending = pending, b""
        os.write(1, SHM.sub(fix, ready))
    os.write(1, pending + b"\x1b\\\x1b[?25h\x1b[?1003l")


def run(name, args, kitty, pids):
    log = os.path.join(HERE, f"bench-{name}.mpvlog")
    before = {k: cpu_seconds(p) for k, p in pids.items()}
    peak = {"mpv": 0.0, "ghostty": rss_mb(pids["ghostty"])}
    child_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.monotonic()
    mpv = subprocess.Popen([MPV, *COMMON, f"--log-file={log}", *args, "--", VIDEO],
                           stdout=subprocess.PIPE if kitty else subprocess.DEVNULL,
                           stdin=subprocess.DEVNULL)
    done = threading.Event()

    def sample():
        while not done.wait(0.5):
            peak["mpv"] = max(peak["mpv"], rss_mb(mpv.pid))
            peak["ghostty"] = max(peak["ghostty"], rss_mb(pids["ghostty"]))

    sampler = threading.Thread(target=sample)
    sampler.start()
    stats = {"frames": 0}
    if kitty:
        relay(mpv, stats)
    mpv.wait()
    wall = time.monotonic() - start
    done.set()
    sampler.join()
    child = resource.getrusage(resource.RUSAGE_CHILDREN)
    mpv_cpu = (child.ru_utime - child_before.ru_utime
               + child.ru_stime - child_before.ru_stime)
    used = {k: cpu_seconds(p) - before[k] for k, p in pids.items()}
    with open(log, errors="replace") as f:
        hw = re.findall(r"Using hardware decoding \(([^)]*)\)", f.read())
    return {
        "run": name, "wall": wall, "mpv": 100 * mpv_cpu / wall,
        "ghostty": 100 * used["ghostty"] / wall,
        "windowserver": 100 * used["windowserver"] / wall,
        "mpv_mb": peak["mpv"], "ghostty_mb": peak["ghostty"],
        "fps": stats["frames"] / wall if kitty else None,
        "hwdec": hw[0] if hw else "no (software)",
    }


def main():
    pids = {"ghostty": ghostty_pid(),
            "windowserver": int(subprocess.run(["pgrep", "-x", "WindowServer"],
                                               capture_output=True, text=True).stdout.split()[0])}
    rows, cols, cw, ch = terminal()
    results = []

    say(f"1/4 idle baseline, {SECONDS} s (don't touch anything)")
    before = {k: cpu_seconds(p) for k, p in pids.items()}
    time.sleep(SECONDS)
    results.append({"run": "idle", "wall": SECONDS, "mpv": 0.0,
                    "ghostty": 100 * (cpu_seconds(pids["ghostty"]) - before["ghostty"]) / SECONDS,
                    "windowserver": 100 * (cpu_seconds(pids["windowserver"]) - before["windowserver"]) / SECONDS,
                    "mpv_mb": 0.0, "ghostty_mb": rss_mb(pids["ghostty"]), "fps": None, "hwdec": "-"})

    box = [(60, 17), (max(20, cols - 12), max(8, rows - 8))]
    for i, (bc, br) in enumerate(box):
        w, h = int(bc * cw), int(br * ch)
        say(f"{2 + i}/4 kitty in a {bc}x{br} cell box ({w}x{h} px), {SECONDS} s")
        results.append(run(f"kitty-{w}x{h}", [
            "--profile=sw-fast", "--vo=kitty", "--vo-kitty-use-shm=yes",
            "--vo-kitty-alt-screen=no", "--vo-kitty-config-clear=no",
            "--vo-kitty-left=6", "--vo-kitty-top=3",
            f"--vo-kitty-cols={bc}", f"--vo-kitty-rows={br}",
            f"--vo-kitty-width={w}", f"--vo-kitty-height={h}"], True, pids))

    # The window as wide in pixels as the large box (mpv's geometry is in points).
    w = int(box[1][0] * cw)
    say(f"4/4 mpv's own window, {w} px wide, {SECONDS} s (it opens over this)")
    results.append(run(f"window-{w}px", [f"--geometry={w // 2}x{w * 9 // 32}",
                                         "--force-window-position"], False, pids))

    lines = ["run                  mpv%  ghostty%  windowserver%  mpv MB  ghostty MB  fps sent  hwdec"]
    for r in results:
        fps = f"{r['fps']:.0f}" if r["fps"] is not None else "-"
        lines.append(f"{r['run']:<20} {r['mpv']:>4.0f}  {r['ghostty']:>8.0f}  {r['windowserver']:>13.0f}"
                     f"  {r['mpv_mb']:>6.0f}  {r['ghostty_mb']:>10.0f}  {fps:>8}  {r['hwdec']}")
    report = "\n".join(lines)
    with open(LOG, "w") as f:
        f.write(report + "\n")
    say(report.replace("\n", "\r\n"))


main()
