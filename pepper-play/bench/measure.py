"""What a playback costs, inside Pepper and in mpv's own window.

    bench/measure.sh [options] CLIP [MPV-OPTION...]

Run it in a Pepper tab (not in tmux). It plays CLIP with pepper-play in a box
of the tab, then with mpv in a window of its own the same size in pixels,
both with the same options: the MPV-OPTIONs given, and pepper-play's own
`hwdec` and `video-sync` as they were, so the two decode and pace alike.
Each run is measured for --seconds from its first frame, after --warmup:

- CPU, as a share of one core, of the player, Pepper (PEPPER_PORT's pid) and
  WindowServer: the mean, and the 95th percentile of the seconds. From the
  processes' CPU time (proc_pid_rusage, ps for WindowServer), not ps's
  %cpu, which is an average that decays over many seconds.
- The most memory of the player and Pepper: resident (RSS) and footprint
  (what Activity Monitor shows, which counts a process's IOSurfaces).
- The player's wakeups a second, and the graphics chip's mean use (ioreg).
- mpv's counts over the measured seconds, through its IPC: frames dropped,
  mistimed and delayed, and at the end `hwdec-current`, `vsync-jitter`,
  `estimated-display-fps` and others.
- pepper-play's PEPPER_PLAY_DEBUG line: frames drawn and shown, surfaces
  made, time spent waiting on the graphics chip.

Prints a table and appends one JSON object a run to --out.

Options:
  --seconds N     how long each run is measured (60)
  --warmup N      seconds after the first frame before measuring (3)
  --box WxH       the box's size in pixels, as near as cells allow
                  (1920x1080; as much of the tab as fits)
  --player PATH   pepper-play (the repo's target/release build, else
                  PEPPER_PLAY)
  --mpv PATH      mpv, for its own window (/opt/homebrew/bin/mpv)
  --no-window     only pepper-play
  --no-pepper     only mpv's window
  --label TEXT    what this run measures, for the table and the JSON
  --out FILE      where the JSON goes (bench/results/results.jsonl)
  --drag N        first drag the box's size about for N seconds, as a
                  window drag would (pepper-play's surfaces made say how
                  often it made new ones)
  --idle          no clip: measure Pepper and WindowServer with nothing
                  playing, for --seconds
  --table FILE    only print the table of a JSON file's runs
"""

import ctypes
import fcntl
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
REPO_PLAYER = os.path.join(HERE, "..", "target", "release", "pepper-play")
COUNTS = ["frame-drop-count", "decoder-frame-drop-count", "mistimed-frame-count",
          "vo-delayed-frame-count"]
AT_END = ["hwdec-current", "hwdec", "video-sync", "vsync-jitter", "estimated-display-fps",
          "display-fps", "container-fps", "estimated-vf-fps", "current-vo", "osd-width",
          "osd-height", "width", "height", "mpv-version"]
GPU = re.compile(rb'"Device Utilization %"=(\d+)')
# pepper-play's own lines only: anchored at a line's start, since mpv's log
# messages (holding a video's tags) share the stream as `mpv [...] ...`.
DEBUG_LINE = re.compile(
    r"^pepper-play: (\d+) frames drawn, (\d+) shown, (\d+) into a surface still on screen; "
    r"(\d+) surfaces made(?: in (\d+) rings)?;(?: (\d+) repeats not drawn;)? "
    r"(\d+(?:\.\d+)?) s waiting on the graphics chip; "
    r"hwdec (\S+)$", re.M)
BOX_LINE = re.compile(
    r"^pepper-play: Pepper says the box is BoxSize \{ width: (\d+), height: (\d+), visible: true \}$",
    re.M)
# How much of a player's stderr is read after a run.
STDERR_TAIL = 1 << 20


def safe(text):
    """Text made inert for the terminal: control and invisible format
    characters shown escaped, as pepper-play's printable() does."""
    out = []
    for c in str(text):
        if unicodedata.category(c) in ("Cc", "Cf"):
            out.append(c.encode("unicode_escape").decode())
        else:
            out.append(c)
    return "".join(out)


# --- the processes' use ----------------------------------------------------

LIBC = ctypes.CDLL("/usr/lib/libSystem.B.dylib")


class _Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_tb = _Timebase()
LIBC.mach_timebase_info(ctypes.byref(_tb))
TICK = _tb.numer / _tb.denom / 1e9  # seconds per mach tick


def rusage(pid):
    """(cpu seconds, wakeups, rss bytes, footprint bytes, start time) of a
    process of this user's, from proc_pid_rusage (RUSAGE_INFO_V2), or None.
    The start time tells a reused pid from the process sampled before."""
    buf = (ctypes.c_uint64 * 64)()
    if LIBC.proc_pid_rusage(pid, 2, ctypes.byref(buf)) != 0:
        return None
    # rusage_info_v2: uuid (2 words), user, system, pkg idle wakeups,
    # interrupt wakeups, pageins, wired, resident, footprint, start, ...
    return ((buf[2] + buf[3]) * TICK, buf[4] + buf[5], buf[8], buf[9], buf[10])


def pid_path(pid):
    """The program a pid runs, or None."""
    buf = ctypes.create_string_buffer(4096)
    n = LIBC.proc_pidpath(pid, buf, ctypes.sizeof(buf))
    return buf.raw[:n].decode(errors="replace") if n > 0 else None


def ps_cpu(pid):
    """CPU seconds from ps, for a process that isn't this user's."""
    out = subprocess.run(["/bin/ps", "-o", "time=", "-p", str(pid)],
                         capture_output=True, text=True).stdout.strip()
    if not out:
        return None
    total = 0.0
    for part in out.split(":"):
        total = total * 60 + float(part)
    return total


def gpu_percent():
    out = subprocess.run(["/usr/sbin/ioreg", "-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"],
                         capture_output=True).stdout
    found = GPU.findall(out)
    return float(found[0]) if found else None


def percentile(values, p):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(round(p / 100 * (len(ordered) - 1))))]


class Sampler:
    """Samples the processes once a second while a run is measured."""

    def __init__(self, pids):
        self.pids = pids  # name -> pid
        self.started = {}  # name -> the sampled process's start time
        self.samples = []  # (time, {name: (cpu, wakeups, rss, footprint, start)}, gpu)
        self.done = threading.Event()
        self.take()
        self.thread = threading.Thread(target=self.loop, daemon=True)
        self.thread.start()

    def take(self):
        now = time.monotonic()
        row = {}
        for name, pid in self.pids.items():
            usage = rusage(pid)
            if usage is None:
                cpu = ps_cpu(pid)
                usage = (cpu, None, None, None, None) if cpu is not None else None
            elif self.started.setdefault(name, usage[4]) != usage[4]:
                usage = None  # the pid now belongs to another process
            row[name] = usage
        self.samples.append((now, row, gpu_percent()))

    def loop(self):
        start = self.samples[0][0]
        n = 1
        while not self.done.wait(max(0.0, start + n - time.monotonic())):
            self.take()
            n += 1

    def stop(self):
        self.done.set()
        self.thread.join()
        self.take()
        return self.summary()

    def summary(self):
        out = {}
        first, last = self.samples[0], self.samples[-1]
        wall = last[0] - first[0]
        for name in self.pids:
            cpus = [(t, row[name][0]) for t, row, _ in self.samples if row.get(name)]
            if len(cpus) < 2:
                continue
            seconds = [100 * (c1 - c0) / (t1 - t0)
                       for (t0, c0), (t1, c1) in zip(cpus, cpus[1:]) if t1 > t0]
            entry = {
                "cpu_mean": 100 * (cpus[-1][1] - cpus[0][1]) / (cpus[-1][0] - cpus[0][0]),
                "cpu_p95": percentile(seconds, 95),
            }
            rows = [row[name] for _, row, _ in self.samples if row.get(name)]
            if rows[0][1] is not None:
                entry["wakeups_per_s"] = (rows[-1][1] - rows[0][1]) / wall
                entry["rss_peak_mb"] = max(r[2] for r in rows) / 2**20
                entry["footprint_peak_mb"] = max(r[3] for r in rows) / 2**20
            out[name] = entry
        gpus = [g for _, _, g in self.samples if g is not None]
        out["gpu_mean"] = sum(gpus) / len(gpus) if gpus else None
        out["wall"] = wall
        return out


# --- mpv's JSON IPC ----------------------------------------------------------

class Ipc:
    def __init__(self, path, process, timeout=15):
        deadline = time.monotonic() + timeout
        self.sock = socket.socket(socket.AF_UNIX)
        while True:
            try:
                self.sock.connect(path)
                break
            except OSError:
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(f"no IPC socket at {path}")
                time.sleep(0.05)
        self.next_id = 0
        self.replies = {}
        self.cond = threading.Condition()
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.sock.makefile("rb"):
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if "request_id" in message:
                with self.cond:
                    self.replies[message["request_id"]] = message
                    self.cond.notify_all()

    def command(self, *args, timeout=5):
        with self.cond:
            self.next_id += 1
            rid = self.next_id
        self.sock.sendall((json.dumps({"command": list(args), "request_id": rid}) + "\n").encode())
        deadline = time.monotonic() + timeout
        with self.cond:
            while rid not in self.replies:
                left = deadline - time.monotonic()
                if left <= 0:
                    return None
                self.cond.wait(left)
            return self.replies.pop(rid)

    def get(self, name):
        reply = self.command("get_property", name)
        if reply and reply.get("error") == "success":
            return reply.get("data")
        return None

    def get_string(self, name):
        reply = self.command("get_property_string", name)
        if reply and reply.get("error") == "success":
            return reply.get("data")
        return None

    def wait_playing(self, process, timeout=30):
        """Until a frame has been shown."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline and process.poll() is None:
            if isinstance(self.get("time-pos"), (int, float)) and self.get("vo-configured"):
                return True
            time.sleep(0.1)
        return False

    def counts(self):
        return {name: self.get(name) for name in COUNTS}

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


# --- runs --------------------------------------------------------------------

TTY = None  # the tab, whatever stdout is


def terminal():
    """The tab: rows, columns, and a cell's size in pixels."""
    global TTY
    if TTY is None:
        TTY = os.open("/dev/tty", os.O_RDWR)
    rows, cols, xpix, ypix = struct.unpack(
        "HHHH", fcntl.ioctl(TTY, termios.TIOCGWINSZ, b"\0" * 8))
    if not xpix or not ypix:
        sys.exit("measure: the terminal doesn't say its size in pixels")
    return rows, cols, xpix / cols, ypix / rows


def fit_window(ipc, pid, box):
    """mpv's window as big as the box. A tiling window manager (AeroSpace)
    sizes new windows itself, whatever mpv's options say: the window is
    made floating first. Then its size is set as a scale of the video's,
    which mpv keeps in pixels (`window-scale`)."""
    if shutil.which("aerospace"):
        for _ in range(50):
            out = subprocess.run(["aerospace", "list-windows", "--all", "--format",
                                  "%{window-id} %{app-pid}"],
                                 capture_output=True, text=True).stdout
            ids = [line.split()[0] for line in out.splitlines()
                   if line.split()[1:2] == [str(pid)]]
            if ids:
                subprocess.run(["aerospace", "layout", "--window-id", ids[0], "floating"],
                               capture_output=True)
                break
            time.sleep(0.1)
    width = ipc.get("width")
    if width:
        ipc.command("set_property", "window-scale", box[0] / width)
        time.sleep(0.5)


def say(text):
    os.write(TTY, f"\x1b[{status_row};1H\x1b[J{safe(text)}\r\n".encode())


status_row = 1


def drag(ipc, place, seconds):
    """What a window drag does to a box an app placed: the app places it
    again at each new size, about every 16 ms. Here its columns and rows go
    from the whole box down to 60% and back, every 2 s."""
    col, row, cols, rows = place
    start = time.monotonic()
    while (t := time.monotonic() - start) < seconds:
        f = 1 - 0.4 * abs(((t / 2) % 1) * 2 - 1)
        ipc.command("script-message-to", "main", "pepper-place", str(col), str(row),
                    str(max(1, round(cols * f))), str(max(1, round(rows * f))), timeout=1)
        time.sleep(0.016)
    ipc.command("script-message-to", "main", "pepper-place", *map(str, place), timeout=1)


def run_player(kind, argv, sock, args, pids, place=None, window=None):
    """Starts a player, waits for its first frame, measures, and stops it."""
    stderr = tempfile.TemporaryFile()
    env = dict(os.environ, PEPPER_PLAY_DEBUG="1")
    process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                               stderr=stderr, env=env)
    result = {"kind": kind}
    ipc = None
    try:
        ipc = Ipc(sock, process)
        if place:
            ipc.command("script-message-to", "main", "pepper-place", *map(str, place))
        if not ipc.wait_playing(process):
            raise RuntimeError(f"{kind}: no frame shown")
        if window:
            fit_window(ipc, process.pid, window)
        time.sleep(args.warmup)
        if place and args.drag:
            drag(ipc, place, args.drag)
            result["drag_s"] = args.drag
        before = ipc.counts()
        sampler = Sampler({"player": process.pid, **pids})
        deadline = time.monotonic() + args.seconds
        while time.monotonic() < deadline and process.poll() is None:
            time.sleep(0.2)
        result.update(sampler.stop())
        if process.poll() is not None:
            raise RuntimeError(f"{kind}: ended before the run did")
        after = ipc.counts()
        result["counts"] = {name: (after[name] - before[name])
                            if isinstance(after[name], int) and isinstance(before[name], int)
                            else None for name in COUNTS}
        result["props"] = {name: ipc.get(name) for name in AT_END}
        result["props"].update({name: ipc.get_string(name) for name in ("hwdec", "video-sync")})
        ipc.command("quit", timeout=2)
        process.wait(timeout=15)
    except Exception as err:  # noqa: BLE001 - a run that fails is reported
        result["error"] = str(err)
    finally:
        if ipc:
            ipc.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    text = read_tail(stderr)
    result.update(parse_stderr(text))
    if "error" in result:
        result["stderr_tail"] = text[-2000:]
    return result


def read_tail(f):
    """The last STDERR_TAIL bytes of a file, as text."""
    size = f.seek(0, os.SEEK_END)
    f.seek(max(0, size - STDERR_TAIL))
    return f.read().decode(errors="replace")


def parse_stderr(text):
    """pepper-play's debug counts and the box's size, from its own lines."""
    out = {}
    debug = DEBUG_LINE.findall(text)
    if debug:
        d = debug[-1]
        out["pepper_play"] = {"drawn": int(d[0]), "shown": int(d[1]), "on_screen": int(d[2]),
                              "surfaces": int(d[3]), "rings": int(d[4]) if d[4] else None,
                              "repeats": int(d[5]) if d[5] else None,
                              "gpu_wait_s": float(d[6]), "hwdec": d[7]}
    boxes = BOX_LINE.findall(text)
    if boxes:
        out["box_px"] = [int(boxes[-1][0]), int(boxes[-1][1])]
    return out


def parse(argv):
    class Args:
        seconds = 60.0
        warmup = 3.0
        box = (1920, 1080)
        player = None
        mpv = "/opt/homebrew/bin/mpv"
        window = True
        pepper = True
        label = ""
        out = os.path.join(HERE, "results", "results.jsonl")
        clip = None
        options = []
        idle = False
        drag = 0.0
    args = Args()
    it = iter(argv)
    for arg in it:
        if arg in ("-h", "--help"):
            print(__doc__)
            sys.exit(0)
        elif arg == "--seconds":
            args.seconds = float(next(it))
        elif arg == "--warmup":
            args.warmup = float(next(it))
        elif arg == "--box":
            w, h = next(it).lower().split("x")
            args.box = (int(w), int(h))
        elif arg == "--player":
            args.player = next(it)
        elif arg == "--mpv":
            args.mpv = next(it)
        elif arg == "--no-window":
            args.window = False
        elif arg == "--no-pepper":
            args.pepper = False
        elif arg == "--label":
            args.label = next(it)
        elif arg == "--out":
            args.out = next(it)
        elif arg == "--drag":
            args.drag = float(next(it))
        elif arg == "--idle":
            args.idle = True
        elif arg == "--table":
            with open(next(it)) as f:
                print(table([json.loads(line) for line in f if line.strip()]))
            sys.exit(0)
        elif arg == "--":
            rest = list(it)
            args.clip, args.options = rest[0], rest[1:]
        elif arg.startswith("--") and args.clip is None:
            sys.exit(f"measure: unknown option {safe(arg)} (--help)")
        elif args.clip is None:
            args.clip = arg
            args.options = list(it)
    if not args.clip and not args.idle:
        sys.exit("measure: a clip to play (--help)")
    if args.player is None:
        args.player = (os.path.abspath(REPO_PLAYER) if os.access(REPO_PLAYER, os.X_OK)
                       else os.environ.get("PEPPER_PLAY"))
    return args


def pids_of(port):
    """Pepper's (PEPPER_PORT ends in its pid, which must be a running
    Pepper, as pepper-play checks) and WindowServer's (the system's, run by
    _windowserver (its effective user), not any process of that name)."""
    try:
        pepper = int(port.rsplit(".", 1)[1])
    except (IndexError, ValueError):
        sys.exit(f"measure: PEPPER_PORT ({safe(port)}) doesn't end in Pepper's pid")
    path = pid_path(pepper) if pepper > 0 else None
    if not path or not path.endswith(".app/Contents/MacOS/ghostty"):
        sys.exit(f"measure: no Pepper runs as pid {pepper} (PEPPER_PORT is left from one that quit?)")
    pids = {"pepper": pepper}
    ws = subprocess.run(["/usr/bin/pgrep", "-x", "-u", "_windowserver", "WindowServer"],
                        capture_output=True, text=True).stdout.split()
    if len(ws) == 1:
        pids["windowserver"] = int(ws[0])
    return pids


def idle(args, pids):
    """Pepper with nothing playing, its tab showing a line of text."""
    global status_row
    terminal()
    os.write(TTY, b"\x1b[H\x1b[2J")
    status_row = 1
    say(f"idle: nothing plays for {args.seconds:.0f} s")
    sampler = Sampler(pids)
    time.sleep(args.seconds)
    r = {"kind": "idle", "clip": "-", "label": args.label, "seconds": args.seconds,
         "when": time.strftime("%Y-%m-%dT%H:%M:%S"), **sampler.stop()}
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    with open(args.out, "a") as f:
        f.write(json.dumps(r) + "\n")
    print(table([r]))


def main():
    global status_row
    args = parse(sys.argv[1:])
    port = os.environ.get("PEPPER_PORT")
    if not port or os.environ.get("TMUX"):
        sys.exit("measure: run it in a Pepper tab, not in tmux (PEPPER_PORT isn't set)")
    if args.idle:
        return idle(args, pids_of(port))
    if not os.path.exists(args.clip):
        sys.exit(f"measure: no such clip: {safe(args.clip)}")
    pids = pids_of(port)

    rows, cols, cw, ch = terminal()
    box_cols = max(1, min(cols, round(args.box[0] / cw)))
    box_rows = max(1, min(rows - 4, round(args.box[1] / ch)))
    status_row = box_rows + 2
    os.write(TTY, b"\x1b[H\x1b[2J")
    tmp = tempfile.mkdtemp(prefix="pepper-bench-")
    os.chmod(tmp, 0o700)
    clip = os.path.abspath(args.clip)
    name = os.path.splitext(os.path.basename(clip))[0]
    common = {"clip": name, "label": args.label, "seconds": args.seconds,
              "options": args.options, "when": time.strftime("%Y-%m-%dT%H:%M:%S")}
    results = []
    settings = []
    try:
        if args.pepper:
            if not args.player:
                sys.exit("measure: no pepper-play (--player)")
            say(f"{name}: pepper-play in a {box_cols}x{box_rows} cell box, "
                f"{args.seconds:.0f} s{' ' + args.label if args.label else ''}")
            sock = os.path.join(tmp, "pepper-play.sock")
            r = run_player("pepper", [args.player, f"--input-ipc-server={sock}", *args.options,
                                      "--", clip], sock, args, pids,
                           place=(0, 0, box_cols, box_rows))
            r.update(common, player_path=args.player)
            results.append(r)
            props = r.get("props") or {}
            for name_ in ("hwdec", "video-sync"):
                if props.get(name_) and not any(o.startswith(f"--{name_}=") for o in args.options):
                    settings.append(f"--{name_}={props[name_]}")
        if args.window:
            box = results[0].get("box_px") if results else None
            if not box:
                box = [round(box_cols * cw), round(box_rows * ch)]
            say(f"{name}: mpv's own window, {box[0]}x{box[1]} px, {args.seconds:.0f} s "
                f"(it opens over this; leave it be)")
            sock = os.path.join(tmp, "mpv.sock")
            options = ["--no-config", "--osc=no", "--terminal=no", *settings, *args.options]
            r = run_player("window", [args.mpv, f"--input-ipc-server={sock}", *options,
                                      "--", clip], sock, args, pids, window=box)
            r.update(common, player_path=args.mpv, window_options=options)
            results.append(r)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    with open(args.out, "a") as f:
        for r in results:
            f.write(json.dumps(r) + "\n")
    os.write(TTY, b"\x1b[H\x1b[2J")
    print(table(results))


def table(results):
    lines = ["clip                 run     player% (p95)   Pepper%  WS%   gpu%  "
             "player MB rss/foot   drops mistimed delayed  hwdec"]
    for r in results:
        if "error" in r:
            lines.append(f"{safe(r['clip']):<20} {safe(r['kind']):<7} ERROR {safe(r['error'])}")
            continue
        p = r.get("player") or {}
        c = r.get("counts", {})
        props = r.get("props", {})
        lines.append(
            f"{safe(r['clip']):<20} {safe(r['kind']):<7} {p.get('cpu_mean', 0):>6.1f} ({p.get('cpu_p95') or 0:>5.1f})"
            f"   {r.get('pepper', {}).get('cpu_mean', 0):>6.1f}"
            f"  {r.get('windowserver', {}).get('cpu_mean', 0):>4.0f}"
            f"  {r.get('gpu_mean') or 0:>5.0f}"
            f"  {p.get('rss_peak_mb', 0):>8.0f}/{p.get('footprint_peak_mb', 0):<6.0f}"
            f"  {c.get('frame-drop-count')!s:>6} {c.get('mistimed-frame-count')!s:>8}"
            f" {c.get('vo-delayed-frame-count')!s:>7}  {safe(props.get('hwdec-current'))}")
    return "\n".join(lines)


if __name__ == "__main__":
    main()
