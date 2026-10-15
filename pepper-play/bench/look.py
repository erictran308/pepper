"""The checks only eyes (or a screenshot) can make, in a Pepper tab.

    bench/run.sh --new-pepper --look

1. Colours: the sRGB patches clip, paused, in a box, next to mpv's own
   window and the YUV test window (yuv-layer.swift). One screenshot of the
   whole screen (Cmd-Shift-3, or any tool that saves to the Desktop or a
   folder in it) holds all of them; the patches should measure the same.
2. A window drag: a video fills the tab, placed again at each new size as an
   app does. Drag the window's corner about for a few seconds and say
   whether the picture ever went blank.

Writes what it saw to --out (JSON): the screenshot's path and the answer.
"""

import importlib.util
import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import termios
import time
import tty

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("measure", os.path.join(HERE, "measure.py"))
measure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measure)


def screenshot_dir():
    out = subprocess.run(["defaults", "read", "com.apple.screencapture", "location"],
                         capture_output=True, text=True).stdout.strip()
    return os.path.expanduser(out) if out else os.path.expanduser("~/Desktop")


def new_screenshots(folder, since):
    """PNGs made since `since` in the folder or a folder in it (Snapzy and
    other tools keep their own): regular files of this user's only, each
    looked at once (not through a link)."""
    found = []
    me = os.getuid()
    for root, dirs, names in os.walk(folder):
        if root.count(os.sep) - folder.rstrip(os.sep).count(os.sep) >= 1:
            dirs[:] = []
        for n in names:
            if not n.lower().endswith(".png"):
                continue
            path = os.path.join(root, n)
            try:
                st = os.lstat(path)
            except OSError:
                continue
            if stat.S_ISREG(st.st_mode) and st.st_uid == me and st.st_mtime > since:
                found.append((st.st_mtime, path))
    return [path for _, path in sorted(found)]


def key(fd, timeout=None):
    """One key from the tab, or None after `timeout`."""
    import select
    old = termios.tcgetattr(fd)
    try:
        tty.setcbreak(fd)
        ready, _, _ = select.select([fd], [], [], timeout)
        return os.read(fd, 1).decode(errors="replace") if ready else None
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)


def write(text):
    os.write(measure.TTY, text.encode())


# Every process this script starts, so that none outlives it, whatever fails.
STARTED = []


def spawn(argv, **kwargs):
    p = subprocess.Popen(argv, **kwargs)
    STARTED.append(p)
    return p


def start_player(player, sock, clip, options, env):
    stderr = tempfile.TemporaryFile()
    p = spawn([player, f"--input-ipc-server={sock}", *options, "--", clip],
              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=stderr,
              env=dict(os.environ, PEPPER_PLAY_DEBUG="1", **env))
    return p, measure.Ipc(sock, p), stderr


def stop(p, ipc=None):
    if ipc:
        ipc.command("quit", timeout=1)
        ipc.close()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()


def colors(args, tmp, result):
    rows, cols, cw, ch = measure.terminal()
    write("\x1b[H\x1b[2J\x1b[?25l")
    clip = os.path.join(HERE, "clips", "colors-1080p30-h264.mp4")
    common = ["--pause", "--start=1", "--ao=null", "--keep-open=yes"]
    write("\x1b[1;1HPepper")
    sock = os.path.join(tmp, "c.sock")
    p, ipc, _ = start_player(args.player, sock, clip, common, {})
    ipc.command("script-message-to", "main", "pepper-place", "0", "1",
                str(max(1, cols // 2)), str(max(1, (rows - 4) // 2)))
    players = [(p, ipc)]
    # mpv's own window, the same frame, and the YUV test window.
    sock = os.path.join(tmp, "mpv.sock")
    mpv = spawn([args.mpv, f"--input-ipc-server={sock}", "--no-config", "--osc=no",
                            "--terminal=no", "--hwdec=auto-safe", "--title=mpv window",
                            *common, "--", clip],
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    mpv_ipc = measure.Ipc(sock, mpv)
    mpv_ipc.wait_playing(mpv)
    measure.fit_window(mpv_ipc, mpv.pid, (960, 540))
    yuv = spawn([args.yuv], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)
    write(f"\x1b[{rows};1H\x1b[2K\x1b[1mStep 1 of 2:\x1b[m make sure this window, mpv's and the yuv-layer "
          "window can all be seen, then press Cmd-Shift-3 (or press s to skip).")
    folder, since = screenshot_dir(), time.time()
    shot = None
    while shot is None:
        found = new_screenshots(folder, since)
        if found:
            shot = found[-1]
            break
        if key(measure.TTY, 0.5) == "s":
            break
    result["screenshot"] = shot
    result["tab_cell_px"] = [cw, ch]
    for p, ipc in players:
        stop(p, ipc)
    stop(mpv, mpv_ipc)
    yuv.terminate()


def drag(args, tmp, result):
    write("\x1b[H\x1b[2J")
    clip = os.path.join(HERE, "clips", "1080p30-h264.mp4")
    sock = os.path.join(tmp, "drag.sock")
    p, ipc, err = start_player(args.player, sock, clip, ["--ao=null", "--loop-file=inf"], {})

    def place(*_):
        rows, cols, _, _ = measure.terminal()
        ipc.command("script-message-to", "main", "pepper-place", "0", "0", str(cols),
                    str(max(1, rows - 2)), timeout=1)
        write(f"\x1b[{rows};1H\x1b[2K\x1b[1mStep 2 of 2:\x1b[m drag this window's corner about for "
              "5 s, watching the video. Then press y if it ever went blank or black, n if not.")

    signal.signal(signal.SIGWINCH, place)
    place()
    answer = None
    while answer not in ("y", "n"):
        answer = key(measure.TTY)
    signal.signal(signal.SIGWINCH, signal.SIG_DFL)
    result["drag_blank"] = answer == "y"
    stop(p, ipc)
    debug = measure.parse_stderr(measure.read_tail(err)).get("pepper_play")
    if debug:
        result["drag_rings"] = debug["rings"] or 0


def main():
    class Args:
        player = os.environ.get("PEPPER_PLAY")
        mpv = "/opt/homebrew/bin/mpv"
        yuv = None
        out = None
    args = Args()
    it = iter(sys.argv[1:])
    for arg in it:
        if arg in ("--player", "--mpv", "--yuv", "--out"):
            setattr(args, arg[2:], next(it))
    tmp = tempfile.mkdtemp(prefix="pepper-look-")
    os.chmod(tmp, 0o700)
    result = {"when": time.strftime("%Y-%m-%dT%H:%M:%S")}
    try:
        colors(args, tmp, result)
        drag(args, tmp, result)
    finally:
        for p in STARTED:
            if p.poll() is None:
                p.terminate()
                try:
                    p.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    p.kill()
        shutil.rmtree(tmp, ignore_errors=True)
        write("\x1b[?25h\x1b[H\x1b[2J")
        if args.out:
            with open(args.out, "w") as f:
                json.dump(result, f, indent=1)
        print(json.dumps(result))


if __name__ == "__main__":
    main()
