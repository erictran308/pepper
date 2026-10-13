#!/usr/bin/env python3
"""Plays a file through tuitube-video into pepper, the way tuitube will.

Run it in a pepper tab:

    python3 -I helper_try.py HELPER VIDEO [SECONDS]

It places a 16:9 box (half the window wide, 2 cells in from the top left)
with OSC 777;pepper and a fresh token, starts the helper with tuitube's mpv
options and a socketpair end as fd 3, then sends the box's pixel size, the
token and the file over mpv's JSON IPC. With SECONDS it quits then (closing
the socket, as tuitube does); without, when the file ends (--idle=once).
"""

import fcntl
import json
import os
import secrets
import socket
import struct
import subprocess
import sys
import termios
import threading
import time

MPV_OPTIONS = [
    "--no-config",
    "--load-scripts=no",
    "--terminal=no",
    "--ytdl=no",
    "--tls-verify=yes",
    "--keep-open=no",
    "--save-position-on-quit=no",
    "--resume-playback=no",
    "--hwdec=auto-safe",
    "--audio-format=float",
    "--force-window=immediate",
    "--idle=once",
    "--input-ipc-client=fd://3",
]


def box():
    if os.environ.get("TRY_BOX"):  # cols,rows,width,height: as another window had it
        cols, rows, width, height = map(int, os.environ["TRY_BOX"].split(","))
        return cols, rows, width, height
    try:
        rows, cols, xpix, ypix = struct.unpack(
            "HHHH", fcntl.ioctl(sys.stdout.fileno(), termios.TIOCGWINSZ, b"\0" * 8)
        )
    except OSError:
        return 40, 11, 640, 360  # not a terminal: a box for trying
    if not (rows and cols and xpix and ypix):
        return 40, 11, 640, 360
    cw, ch = xpix / cols, ypix / rows
    box_cols = max(8, cols // 2)
    box_rows = max(1, round(box_cols * cw * 9 / 16 / ch))
    return box_cols, box_rows, round(box_cols * cw), round(box_rows * ch)


def main():
    helper, video = sys.argv[1], os.path.abspath(sys.argv[2])
    seconds = float(sys.argv[3]) if len(sys.argv) > 3 else None
    token = secrets.token_hex(16)
    cols, rows, width, height = box()

    ours, theirs = socket.socketpair()
    env = {
        "HOME": os.environ["HOME"],
        "LANG": os.environ.get("LANG", "en_US.UTF-8"),
        "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "PEPPER_PORT": os.environ["PEPPER_PORT"],
    }
    # Python's descriptors don't pass to children; fd 3 made by dup2 does.
    proc = subprocess.Popen(
        [helper, *MPV_OPTIONS, *os.environ.get("TRY_MPV", "").split()],
        env=env,
        cwd=os.environ["HOME"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        close_fds=False,
        preexec_fn=lambda: os.dup2(theirs.fileno(), 3),
    )
    theirs.close()
    print(f"helper pid {proc.pid}, box {cols}x{rows} cells = {width}x{height} px", file=sys.stderr)

    sys.stdout.write(f"\033]777;pepper;place;{token};2;2;{cols};{rows}\033\\")
    sys.stdout.flush()

    def send(*command):
        ours.sendall((json.dumps({"command": list(command)}) + "\n").encode())

    def read():
        for line in ours.makefile("rb"):
            try:
                message = json.loads(line)
            except ValueError:
                continue
            event = message.get("event")
            if event in ("file-loaded", "playback-restart", "end-file", "shutdown"):
                print(f"mpv: {event}", file=sys.stderr)
            if event == "playback-restart":
                ours.sendall(b'{"command":["get_property","hwdec-current"],"request_id":1}\n')
            if message.get("request_id") == 1:
                print(f"mpv: hwdec-current = {message.get('data')}", file=sys.stderr)

    threading.Thread(target=read, daemon=True).start()
    send("script-message-to", "main", "pepper-size", str(width), str(height))
    send("script-message-to", "main", "pepper-token", token)
    send("loadfile", video, "replace")

    if seconds is not None and seconds >= 10:
        pepper_pid = os.environ["PEPPER_PORT"].rsplit(".", 1)[1]
        threading.Thread(target=measure, args=(proc.pid, pepper_pid), daemon=True).start()

    started = time.monotonic()
    try:
        if seconds is None:
            proc.wait()
            print("helper ended on its own", file=sys.stderr)
        else:
            try:
                proc.wait(timeout=seconds)
                print(f"helper ended on its own after {time.monotonic() - started:.1f} s", file=sys.stderr)
            except subprocess.TimeoutExpired:
                print(f"{seconds:g} s up: closing the socket", file=sys.stderr)
    except KeyboardInterrupt:
        print(f"Ctrl-C after {time.monotonic() - started:.1f} s: closing the socket", file=sys.stderr)
    finally:
        ours.shutdown(socket.SHUT_RDWR)
        ours.close()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            print("helper didn't quit when the socket closed: killed", file=sys.stderr)
        sys.stdout.write(f"\033]777;pepper;remove;{token}\033\\")
        sys.stdout.flush()
        print(f"helper exited {proc.returncode}", file=sys.stderr)


def measure(helper_pid, pepper_pid, samples=6):
    """CPU of the helper and pepper, from 3 s in (top's first sample is skipped)."""
    time.sleep(3)
    out = subprocess.run(
        ["/usr/bin/top", "-l", str(samples + 1), "-s", "1", "-stats", "pid,cpu",
         "-pid", str(helper_pid), "-pid", str(pepper_pid)],
        capture_output=True, text=True,
    ).stdout
    cpu = {str(helper_pid): [], str(pepper_pid): []}
    for line in out.splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[0] in cpu:
            cpu[fields[0]].append(float(fields[1]))
    for name, pid in (("helper", str(helper_pid)), ("pepper", str(pepper_pid))):
        values = cpu[pid][1:]
        if values:
            print(f"{name}: {sum(values) / len(values):.1f}% of a core over {len(values)} s", file=sys.stderr)


if __name__ == "__main__":
    main()
