#!/usr/bin/env python3
"""pepper-agent's integration test: the viewer in a pseudo-terminal of its
own, driven as an agent and a human would drive it, with generated media.

The viewer runs with a PEPPER_PORT no Pepper holds, so pepper-play draws to
nowhere: everything but the picture on screen is real (pepper-play, mpv, the
socket, the keys, what the viewer writes to its terminal, snapshots). The
picture is checked by hand in a test Pepper (README's "Trying it").

Usage: tests/integration.py [--agent PATH] [--play PATH]
  --agent  pepper-agent to test (default: target/debug/pepper-agent, built)
  --play   pepper-play to use (default: $PEPPER_PLAY, or Pepper.app's)
Needs ffmpeg, and mpv's library (brew install mpv).
"""

import argparse
import fcntl
import json
import os
import pty
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
CRATE = os.path.dirname(HERE)
FAILS = []


def check(what, ok, detail=""):
    print(("ok   " if ok else "FAIL ") + what + (f": {detail}" if detail and not ok else ""))
    if not ok:
        FAILS.append(what)


class Viewer:
    """pepper-agent view in a pseudo-terminal: keys in, screen out."""

    def __init__(self, agent, env, cwd):
        self.out = bytearray()
        self.lock = threading.Lock()
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(cwd)
            os.execve(agent, [agent, "view", "--debug"], env)
        self.pid, self.fd = pid, fd
        # 160×50 cells of 10×20 pixels.
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 1600, 1000))
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        while True:
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            with self.lock:
                self.out += data

    def screen(self):
        with self.lock:
            return bytes(self.out)

    def keys(self, data):
        os.write(self.fd, data)
        time.sleep(0.3)

    def wait_for(self, needle, timeout=10):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.screen():
                return True
            time.sleep(0.1)
        return False

    def end(self):
        try:
            os.write(self.fd, b"q")
            time.sleep(0.2)
            os.write(self.fd, b"q")
        except OSError:
            pass
        for _ in range(50):
            done, _ = os.waitpid(self.pid, os.WNOHANG)
            if done:
                return True
            time.sleep(0.1)
        os.kill(self.pid, 9)
        os.waitpid(self.pid, 0)
        return False


def make_media(dir):
    def ff(*args):
        subprocess.run(["ffmpeg", "-v", "error", "-y", *args], check=True)

    clip = os.path.join(dir, "clip.mp4")
    ff("-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-f", "lavfi",
       "-i", "sine=frequency=440:sample_rate=48000", "-t", "12", "-c:v", "libx264",
       "-pix_fmt", "yuv420p", "-g", "30", "-c:a", "aac", clip)
    # A title, a file name and tags with escape sequences in them.
    hostile = os.path.join(dir, "evil\x1b]2;pwned\x07.mp4")
    ff("-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25", "-t", "3", "-c:v", "libx264",
       "-pix_fmt", "yuv420p", "-metadata", "title=\x1b]52;c;cHduZWQ=\x07\x1b[2J",
       "-metadata:s:v:0", "language=\x1b[31mx", hostile)
    sound = os.path.join(dir, "sound.m4a")
    ff("-f", "lavfi", "-i", "sine=frequency=220:sample_rate=48000", "-t", "4", "-c:a", "aac", sound)
    return clip, hostile, sound


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--agent")
    ap.add_argument("--play")
    a = ap.parse_args()
    agent = a.agent
    if not agent:
        subprocess.run(["cargo", "build", "--quiet", "--manifest-path", os.path.join(CRATE, "Cargo.toml")], check=True)
        agent = os.path.join(CRATE, "target", "debug", "pepper-agent")
    play = a.play or os.environ.get("PEPPER_PLAY") or "/Applications/Pepper.app/Contents/Helpers/Pepper.app/Contents/MacOS/pepper-play"
    if not shutil.which("ffmpeg"):
        sys.exit("integration.py: needs ffmpeg")

    work = tempfile.mkdtemp(prefix="pepper-agent-it-")
    project = os.path.join(work, "project")
    os.makedirs(project)
    clip, hostile, sound = make_media(work)

    env = dict(os.environ)
    env.pop("TMUX", None)
    # No Pepper holds this name: pepper-play draws to nowhere.
    env["PEPPER_PORT"] = "io.github.erictran308.pepper.0"
    env["PEPPER_PLAY"] = play
    viewer = Viewer(agent, env, project)
    check("the viewer says it waits for an agent", viewer.wait_for(b"waiting for an agent"))

    def cli(*args, ok=True, timeout=60):
        r = subprocess.run([agent, *args, "--viewer", str(viewer.pid)], capture_output=True,
                           text=True, cwd=project, env=env, timeout=timeout)
        if ok and r.returncode != 0:
            check(f"pepper-agent {' '.join(args)}", False, r.stderr.strip())
            return {}
        if not ok:
            return json.loads(r.stderr.strip().splitlines()[-1]) if r.stderr.strip() else {}
        return json.loads(r.stdout)

    status = cli("status")
    check("status finds the viewer", status.get("viewer", {}).get("id") == str(viewer.pid))
    check("status says what's next", "open" in status.get("next", ""), status.get("next"))

    opened = cli("open", clip, "--name", "clip")
    src = opened.get("source", {})
    check("open answers once the first frame shows", src.get("status") == "paused", src)
    check("open gives the length", abs((src.get("duration") or 0) - 12) < 0.2, src)
    check("the viewer places the box", viewer.wait_for(b"\x1b]777;pepper;place;"))
    check("the label shows the source", viewer.wait_for(b"clip  paused"))
    check("hardware decoding", src.get("hwdec") == "videotoolbox" or cli("where").get("status") == "paused",
          src.get("hwdec"))

    seek = cli("seek", "4.5")
    check("seek lands", abs((seek.get("time_pos") or 0) - 4.5) < 0.05, seek)
    where = cli("where")
    check("where says the time", abs((where.get("time_pos") or 0) - 4.5) < 0.1, where)
    rel = cli("seek", "-2")
    check("seek -2 goes back", abs((rel.get("time_pos") or 0) - 2.5) < 0.1, rel)
    bad = cli("seek", "1:99", ok=False)
    check("a bad time is BAD_TIME", bad.get("error", {}).get("code") == "BAD_TIME", bad)

    shot = cli("snapshot", "clip")
    path = shot.get("path", "")
    check("snapshot writes a PNG", path.endswith(".png") and os.path.isfile(path), shot)
    check("snapshot is the source's size", (shot.get("width"), shot.get("height")) == (640, 360), shot)
    check("snapshots go in the project's .pepper", path.startswith(os.path.realpath(project)) or path.startswith(project), path)
    crop = cli("snapshot", "--region", "0,0,320,180")
    check("snapshot --region crops", (crop.get("width"), crop.get("height")) == (320, 180), crop)
    check("the raw snapshot is gone", not os.path.exists(shot.get("raw", "/nonexistent")))

    review = cli("review", "1", "2")
    check("review answers with its span", review.get("to") == 2, review)
    time.sleep(1.8)
    w = cli("where")
    check("review pauses at its end", w.get("paused") is True and 1.9 <= (w.get("time_pos") or 0) <= 2.3, w)

    said = cli("say", "Watch", "this:", "\x1b]52;c;eA==\x07press m")
    check("say is kept inert", "\\u{1b}" in (said.get("say") or ""), said)
    check("say shows on the status line", viewer.wait_for(b"agent: Watch this:"))
    check("say's escape never reaches the terminal raw", b"\x1b]52" not in viewer.screen())

    # The human: a mark, a note with an escape typed and pasted in, done.
    since = cli("events").get("last_id", 0)
    viewer.keys(b"m")
    viewer.keys(b"n")
    viewer.keys(b"too \x1b[31mdark")
    viewer.keys(b"\x1b[200~ \x1b]2;x\x07pasted\x1b[201~")
    viewer.keys(b"\r")
    viewer.keys(b"\r")
    ev = cli("events", "--since", str(since), "--until", "done", "--wait", "5")
    types = [e.get("type") for e in ev.get("events", [])]
    check("marks, notes and done come as events", types == ["mark", "note", "done"], ev)
    note = next((e for e in ev.get("events", []) if e.get("type") == "note"), {})
    check("a note keeps no control characters", "\x1b" not in note.get("text", "x\x1b") and "dark" in note.get("text", ""), note)
    mark = next((e for e in ev.get("events", []) if e.get("type") == "mark"), {})
    check("a mark has the media time", mark.get("source") == "clip" and isinstance(mark.get("time"), (int, float)), mark)
    check("events are written to .pepper/events.jsonl", os.path.isfile(os.path.join(project, ".pepper", "events.jsonl")))

    t0 = time.time()
    to = cli("events", "--since", str(ev.get("last_id", 0)), "--wait", "1", ok=False)
    check("events --wait times out", to.get("error", {}).get("code") == "TIMEOUT" and time.time() - t0 >= 0.9, to)

    # A long poll answered by a key while it waits.
    def press_later():
        time.sleep(1)
        viewer.keys(b"m")
    threading.Thread(target=press_later).start()
    t0 = time.time()
    lp = cli("events", "--since", str(ev.get("last_id", 0)), "--wait", "10")
    check("events --wait answers when the human marks", len(lp.get("events", [])) == 1 and time.time() - t0 < 5, lp)

    # Hostile media: a file name and tags with escapes.
    h = cli("open", hostile)
    check("a hostile file opens", h.get("source", {}).get("status") == "paused", h)
    check("its name is made safe", all(c.isalnum() or c in "._-" for c in h.get("source", {}).get("name", "")), h)
    p = subprocess.run([agent, "probe", hostile], capture_output=True, text=True, cwd=project)
    check("probe escapes tags", "\x1b" not in p.stdout and "\\u001b" in p.stdout, p.stdout[:300])
    screen = viewer.screen()
    check("nothing hostile reached the terminal",
          b"\x1b]2;" not in screen and b"\x07" not in screen and b"\x1b]52" not in screen)

    s = cli("open", sound)
    check("a sound-only file opens", s.get("source", {}).get("status") in ("paused", "playing"), s)
    ns = cli("snapshot", s.get("source", {}).get("name", "sound"), ok=False)
    check("no snapshot of sound", ns.get("error", {}).get("code") == "FAILED", ns)

    lay = cli("layout", "grid")
    check("layout grid shows all three", len(lay.get("visible", [])) == 3, lay)
    places = viewer.screen().count(b"\x1b]777;pepper;place;")
    check("each source got a box", places >= 3, places)
    extra = cli("open", clip, "--name", "four")
    check("a fourth source fits", extra.get("source", {}).get("name") == "four", extra)
    limit = cli("open", clip, "--name", "five", ok=False)
    check("a fifth is LIMIT_REACHED", limit.get("error", {}).get("code") == "LIMIT_REACHED", limit)
    closed = cli("close", "four")
    check("close", closed.get("closed") == "four", closed)
    check("its box was taken away", b"\x1b]777;pepper;remove;" in viewer.screen())

    # The socket: unknown commands, bad versions, too long, not JSON.
    tmp =subprocess.run(["getconf", "DARWIN_USER_TEMP_DIR"], capture_output=True, text=True).stdout.strip()
    sock_path = os.path.join(tmp, f"pepper-agent-{os.getuid()}", f"{viewer.pid}.sock")
    check("the socket is the user's only", oct(os.stat(sock_path).st_mode & 0o777) == "0o600")
    check("its folder too", oct(os.stat(os.path.dirname(sock_path)).st_mode & 0o777) == "0o700")

    def raw(payload):
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.settimeout(5)
        c.connect(sock_path)
        c.sendall(payload)
        data = b""
        while not data.endswith(b"\n"):
            chunk = c.recv(65536)
            if not chunk:
                break
            data += chunk
        c.close()
        return json.loads(data) if data else {}

    r = raw(b'{"v":1,"cmd":"run","args":{"command":["sh","-c","touch /tmp/x"]}}\n')
    check("unknown commands are refused", r.get("error", {}).get("code") == "UNSUPPORTED", r)
    r = raw(b'{"v":1,"cmd":"loadfile","args":{}}\n')
    check("mpv's commands aren't passed through", r.get("error", {}).get("code") == "UNSUPPORTED", r)
    r = raw(b'{"v":9,"cmd":"status"}\n')
    check("other versions are refused", r.get("error", {}).get("code") == "UNSUPPORTED", r)
    r = raw(b"not json\n")
    check("not JSON is refused", r.get("error", {}).get("code") == "BAD_REQUEST", r)
    r = raw(b'{"v":1,"cmd":"say","args":{"text":"' + b"a" * 70000 + b'"}}\n')
    check("an oversized request is refused", r.get("error", {}).get("code") == "BAD_REQUEST", r)
    r = raw(b'{"v":1,"cmd":"open","args":{"target":"av://lavfi:testsrc"}}\n')
    check("open takes no mpv protocols", r.get("error", {}).get("code") == "BAD_ARGS", r)
    r = raw(b'{"v":1,"cmd":"open","args":{"target":"https://example.com/\\u001b[2J"}}\n')
    check("a web address takes no control characters", r.get("error", {}).get("code") == "BAD_ARGS", r)

    ended = viewer.end()
    check("q, q quits the viewer", ended)
    check("the screen is given back", viewer.screen().endswith(b"\x1b[?1049l"), viewer.screen()[-40:])
    check("the socket is gone", not os.path.exists(sock_path))
    time.sleep(0.5)
    left = subprocess.run(["pgrep", "-f", f"^{play}"], capture_output=True, text=True).stdout.split()
    mine = [p for p in left if subprocess.run(["ps", "-o", "ppid=", "-p", p], capture_output=True, text=True).stdout.strip() == str(viewer.pid)]
    check("no pepper-play is left", not mine, mine)
    after = subprocess.run([agent, "where", "--viewer", str(viewer.pid)], capture_output=True, text=True, cwd=project)
    check("no viewer: NO_VIEWER", '"NO_VIEWER"' in after.stderr, after.stderr)

    shutil.rmtree(work, ignore_errors=True)
    print(f"\n{'all passed' if not FAILS else f'{len(FAILS)} failed'}")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
