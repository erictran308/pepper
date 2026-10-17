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
        self.agent, self.env, self.cwd = agent, env, cwd
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

    def keys(self, data, pause=0.3):
        os.write(self.fd, data)
        time.sleep(pause)

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


PAGE = """<!doctype html><html><head><title>Shop &#x1b;]2;pwned&#x07; admin</title>
<style>body{font:16px system-ui;margin:0} header{padding:16px 24px;background:#223;color:#fff}
.card{margin:24px;padding:16px;border:1px solid #ccc} .card.bad{margin-left:41px} .tall{height:2400px}</style></head>
<body><header><h1 id="title">Dashboard</h1></header>
<div class="card">First card</div><div class="card bad" data-testid="second">Second card</div>
<button id="b" onclick="this.textContent='clicked '+(window.n=(window.n||0)+1)">Click me</button>
<input id="in" aria-label="Name"><div class="tall"></div>
<script>
console.log("from the page \x1b]52;c;cHduZWQ=\x07");
setTimeout(() => { throw new Error("boom"); }, 50);
fetch("https://example.com/x").catch(() => {});
setTimeout(() => { const d = document.createElement("p"); d.id = "late"; d.textContent = "late"; document.body.append(d); }, 500);
</script></body></html>"""


def serve(root):
    """A static server on 127.0.0.1, on a thread; its port."""
    import functools
    import http.server
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *args):
            pass

    handler = functools.partial(Quiet, directory=root)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def press_at(viewer, box, viewport, css, cell=(10, 20)):
    """Pane pixels of a page's CSS point, as the viewer lays it out."""
    col, row, cols, rows = box
    bx, by, bw, bh = col * cell[0], row * cell[1], cols * cell[0], rows * cell[1]
    aspect = viewport["width"] / viewport["height"]
    cw, ch = (round(bh * aspect), bh) if bw / bh > aspect else (bw, round(bw / aspect))
    ox, oy = (bw - cw) // 2, (bh - ch) // 2
    return (int(bx + ox + css[0] * cw / viewport["width"]), int(by + oy + css[1] * ch / viewport["height"]))


def frames_clip(dir):
    """4 s at 25 fps, no sound, each frame showing its number as 8 black or white
    bars (bit k in the k-th 40 pixels): lossy encoding can't change it."""
    path = os.path.join(dir, "frames.mp4")
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i",
                    "nullsrc=size=320x240:rate=25,geq=lum='16+219*mod(floor(N/pow(2,floor(X/40))),2)':cb=128:cr=128",
                    "-t", "4", "-c:v", "libx264", "-pix_fmt", "yuv420p", path], check=True)
    return path


def frame_numbers(path):
    """Each frame's number, read back from its bars."""
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-vf", "crop=320:2:0:119,format=gray",
                          "-f", "rawvideo", "-"], capture_output=True, check=True).stdout
    rows = [raw[i:i + 320] for i in range(0, len(raw) - 639, 640)]
    return [sum((row[20 + 40 * k] > 128) << k for k in range(8)) for row in rows]


def edit_checks(cli, viewer, work, project, clip, hostile):
    frames = frames_clip(work)
    cut = os.path.join(project, "cut.json")
    rel = lambda p: os.path.relpath(p, project)

    def write(path, data):
        with open(path, "w") as f:
            json.dump(data, f)

    # A hostile file name among the clips: its path goes into the EDL and the events.
    write(cut, {"pepper_edit": 1, "clips": [
        {"src": rel(clip), "in": 2, "out": 4, "name": "open"},
        {"src": rel(hostile), "in": 0.5, "out": 2.5},
        {"src": rel(frames), "in": "0:01", "out": 2.4},
    ]})
    v = cli("edit", "validate", cut)
    check("edit validate says a good project is", v.get("valid") is True and v.get("duration") == 5.4, v)
    check("its output is the first clip's", v.get("output") == {"width": 640, "height": 360, "fps": 30, "fit": "pad"}, v)
    bad = os.path.join(project, "bad.json")
    write(bad, {"pepper_edit": 1, "outptu": {}, "clips": [
        {"src": rel(clip), "in": 10, "out": 14},
        {"src": "nope.mp4"},
        {"src": rel(clip), "in": "1:99", "name": "\x1b]2;x\x07"},
        {"src": rel(clip), "in": 3, "out": 2},
    ]})
    e = cli("edit", "validate", bad, ok=False).get("error", {})
    ats = [p.get("at") for p in e.get("problems", [])]
    check("edit validate lists every problem at once", e.get("code") == "INVALID_PROJECT" and ats == [
        "outptu", "clips[0].out", "clips[1].src", "clips[2].name", "clips[2].in", "clips[3].out"], ats)
    t = cli("edit", "timeline", cut, "--at", "2.5,4.5")
    at = t.get("at", [{}, {}])
    check("edit timeline maps output to source time",
          at[0].get("clip") == 1 and at[0].get("src_time") == 1.0 and at[1].get("clip") == 2 and at[1].get("src_time") == 1.5, at)
    check("and lists each clip's span", [c.get("start") for c in t.get("clips", [])] == [0, 2, 4], t.get("clips"))

    p = cli("edit", "preview", cut)
    src = p.get("source", {})
    check("edit preview opens the cut, paused", src.get("name") == "cut" and src.get("status") == "paused", p)
    check("the preview is as long as the cut", abs((src.get("duration") or 0) - 5.4) < 0.1, src)
    check("its source says its project", src.get("project") == cut, src)
    check("the EDL is in .pepper/edits", p.get("edl", "").endswith("/.pepper/edits/cut.edl") and os.path.isfile(p.get("edl", "")), p)
    since = cli("events").get("last_id", 0)
    cli("seek", "2.5")
    w = cli("where")
    check("where on a preview gives the clip and its file's time",
          w.get("clip") == 1 and abs((w.get("src_time") or 0) - 1.0) < 0.05 and "evil" in w.get("src", ""), w)
    check("the label says which clip", viewer.wait_for(b"clip 2 of 3"))
    check("a preview's seek bar shows its cuts", viewer.wait_for("│".encode()) and viewer.wait_for("●".encode()))
    viewer.keys(b"m")
    cli("seek", "4.5")
    viewer.keys(b"n")
    viewer.keys(b"too long")
    viewer.keys(b"\r")
    viewer.keys(b"y")
    ev = cli("events", "--since", str(since), "--until", "approve", "--wait", "5")
    got = {e.get("type"): e for e in ev.get("events", [])}
    mark, note, ok = got.get("mark", {}), got.get("note", {}), got.get("approve", {})
    check("a mark on a preview has both times",
          mark.get("project") == cut and mark.get("clip") == 1 and abs((mark.get("time") or 0) - 2.5) < 0.05
          and abs((mark.get("src_time") or 0) - 1.0) < 0.05 and "file" not in mark, mark)
    check("a note too", note.get("clip") == 2 and abs((note.get("src_time") or 0) - 1.5) < 0.05
          and note.get("src") == frames and note.get("text") == "too long", note)
    check("y approves", ok.get("project") == cut and ok.get("source") == "cut", ok)
    check("the mark's flash names the clip", viewer.wait_for(b"clip 2 of 3 (evil"))
    check("the clip's file name reaches the terminal inert", b"\x1b]2;pwned" not in viewer.screen())
    shot = cli("snapshot")
    check("a snapshot of a preview has the clip's time", shot.get("clip") == 2 and shot.get("project") == cut, shot)

    # The human asked for the middle clip to go: the same box shows the new cut.
    write(cut, {"pepper_edit": 1, "clips": [
        {"src": rel(clip), "in": 2, "out": 4, "name": "open"},
        {"src": rel(frames), "in": 1, "out": 2.4},
    ]})
    p = cli("edit", "preview", cut)
    check("a preview again shows the new cut", abs((p.get("source", {}).get("duration") or 0) - 3.4) < 0.1, p)
    check("in the same box", [s.get("name") for s in cli("sources").get("sources", [])].count("cut") == 1)
    cli("seek", "2.5")
    w = cli("where")
    check("its times are the new cut's", w.get("clip") == 1 and abs((w.get("src_time") or 0) - 1.5) < 0.05, w)
    # The note (frames.mp4 at 1.5) is still in the new cut, at 2.5; the mark
    # on the clip that went is gone, so after the note there's nothing.
    cli("seek", "0")
    viewer.keys(b">")
    w = cli("where")
    check("> goes to the next mark, kept in place in the revised cut", abs((w.get("time_pos") or -9) - 2.5) < 0.05, w)
    check("paused on it, the keys row shows the note", viewer.wait_for(b"your note at 0:02.5") and viewer.wait_for(b": too long"))
    check("marks show on the seek bar", "◆".encode() in viewer.screen())
    viewer.keys(b">")
    check("the mark whose clip was cut out is gone", viewer.wait_for(b"No mark after this one"))
    viewer.keys(b"<")
    check("< goes back", viewer.wait_for(b"No mark before this one"))
    cli("close", "cut")

    r = subprocess.run([viewer.agent, "edit", "render", cut], capture_output=True, text=True, cwd=project, env=viewer.env)
    out = json.loads(r.stdout or "{}")
    check("edit render makes the cut", r.returncode == 0 and os.path.isfile(out.get("path", "")), r.stderr[-300:])
    check("as a draft in .pepper/renders", out.get("path", "").endswith("/.pepper/renders/cut.draft.mp4")
          and out.get("encoder") == "h264_videotoolbox", out)
    check("as long as the project", abs((out.get("duration") or 0) - 3.4) < 0.04, out)
    check("with a contact sheet to look at", os.path.isfile(out.get("sheet") or ""), out)
    check("progress comes on stderr", '{"progress":{"step":"render"' in r.stderr, r.stderr[:200])
    streams = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type,width,height", "-of", "csv=p=0",
                              out.get("path", "")], capture_output=True, text=True).stdout.split()
    check("its picture is the output's, and its sound is there", streams == ["video,640,360", "audio"], streams)

    # Frame by frame: each clip from its in, nothing dropped or doubled at the cut.
    exact = os.path.join(project, "exact.json")
    write(exact, {"pepper_edit": 1, "output": {"width": 320, "height": 240, "fps": 25}, "clips": [
        {"src": rel(frames), "in": 1.0, "out": 2.4}, {"src": rel(frames), "in": 0.4, "out": 1.6}]})
    made = os.path.join(project, "exact.mp4")
    cli("edit", "render", exact, "-o", made)
    want = list(range(25, 60)) + list(range(10, 40))
    check("a render is frame-exact at its cuts", frame_numbers(made) == want, frame_numbers(made))
    again = cli("edit", "render", exact, "-o", made, ok=False)
    check("a file that exists isn't replaced without --overwrite", again.get("error", {}).get("code") == "BAD_ARGS", again)
    final = cli("edit", "render", exact, "-o", made, "--overwrite", "--quality", "final")
    check("--overwrite --quality final", final.get("encoder") == "libx264" and frame_numbers(made) == want, final)
    over = cli("edit", "render", exact, "-o", frames, "--overwrite", ok=False)
    check("never over a clip's file", over.get("error", {}).get("code") == "BAD_ARGS" and os.path.getsize(frames) > 0, over)
    leftovers = [f for f in os.listdir(project) if ".part" in f]
    check("no half-written files are left", not leftovers, leftovers)

    seekbar_checks(cli, viewer, frames)
    video_use_checks(cli, viewer, work, clip, frames)

    schemas = cli("schema").get("schemas", [])
    check("schema lists its schemas", [s.get("name") for s in schemas] == ["edit", "event", "error", "timeline", "render"], schemas)
    check("schema edit is a JSON Schema", "$schema" in cli("schema", "edit"))


def video_use_checks(cli, viewer, work, clip, frames):
    """video-use's way (github.com/browser-use/video-use): footage, an edit/ folder
    with its edl.json and Scribe transcripts. Pepper previews the EDL, shows its
    render, and marks come back with the range and what was said."""
    edit = os.path.join(work, "edit")
    os.makedirs(os.path.join(edit, "transcripts"), exist_ok=True)
    edl = os.path.join(edit, "edl.json")
    with open(edl, "w") as f:
        json.dump({"version": 1, "sources": {"talk": "../clip.mp4", "frames": frames},
                   "ranges": [{"source": "talk", "start": 1.0, "end": 3.0, "beat": "HOOK", "quote": "hello world"},
                              {"source": "frames", "start": 0.5, "end": 2.0, "beat": "END"}],
                   "subtitles": "edit/master.srt", "total_duration_s": 3.5}, f)
    with open(os.path.join(edit, "transcripts", "talk.json"), "w") as f:
        json.dump({"words": [
            {"text": "hello", "start": 1.0, "end": 1.4, "type": "word", "speaker_id": "speaker_0"},
            {"text": " ", "start": 1.4, "end": 1.5, "type": "spacing"},
            {"text": "world", "start": 1.5, "end": 2.1, "type": "word", "speaker_id": "speaker_0"},
            {"text": "again\u001b]2;x\u0007", "start": 2.6, "end": 3.0, "type": "word", "speaker_id": "speaker_0"}]}, f)

    v = cli("edit", "validate", edl)
    check("edit validate reads video-use's edl.json", v.get("valid") is True and v.get("clips") == 2 and v.get("duration") == 3.5, v)
    check("and says what a preview of it leaves out",
          any("subtitles" in (w.get("warning") or "") for w in v.get("warnings", [])), v.get("warnings"))
    t = cli("edit", "timeline", edl, "--at", "0.7")
    at = (t.get("at") or [{}])[0]
    check("edit timeline gives a range's source name and quote",
          [(c.get("source"), c.get("name"), c.get("quote")) for c in t.get("clips", [])]
          == [("talk", "HOOK", "hello world"), ("frames", "END", None)], t.get("clips"))
    check("and what was said there, from the transcript",
          (at.get("said") or {}).get("word", {}).get("text") == "world" and at.get("src_time") == 1.7, at)
    check("with the pauses either side, to cut in",
          (at.get("said") or {}).get("pause_before") == [0, 1.0] and (at.get("said") or {}).get("pause_after") == [2.1, 2.6], at)
    raw = subprocess.run([viewer.agent, "edit", "timeline", edl, "--at", "1.8"], capture_output=True, cwd=viewer.cwd, env=viewer.env)
    check("a transcript's text comes out inert", b"again]2;x" in raw.stdout and b"\x1b" not in raw.stdout
          and b"\x07" not in raw.stdout, raw.stdout[:300])

    bad = os.path.join(edit, "bad.json")
    with open(bad, "w") as f:
        json.dump({"sources": {"talk": "../clip.mp4"},
                   "ranges": [{"source": "talk", "start": 1, "end": 99}, {"source": "nope", "start": 0, "end": 1}]}, f)
    e = cli("edit", "validate", bad, ok=False).get("error", {})
    check("its problems are said in its own terms",
          [p.get("at") for p in e.get("problems", [])] == ["ranges[1].source", "ranges[0].end"], e)
    r = cli("edit", "render", edl, ok=False).get("error", {})
    check("its render is video-use's render.py", r.get("code") == "UNSUPPORTED" and "render.py" in r.get("hint", ""), r)

    p = cli("edit", "preview", edl)
    check("edit preview shows video-use's cut", abs((p.get("source", {}).get("duration") or 0) - 3.5) < 0.1, p.get("source"))
    since = cli("events").get("last_id", 0)
    cli("seek", "0.7")
    viewer.keys(b"m")
    ev = cli("events", "--since", str(since), "--wait", "5").get("events", [{}])
    mark = ev[0] if ev else {}
    said = mark.get("said") or {}
    check("a mark on it names the range and what was said",
          mark.get("clip_name") == "HOOK" and abs((mark.get("src_time") or 0) - 1.7) < 0.05
          and said.get("word", {}).get("text") == "world" and said.get("phrase") == "hello world", mark)
    # A range: i at one end, o at the other, a note.
    since = cli("events").get("last_id", 0)
    cli("seek", "0.3")
    viewer.keys(b"i")
    check("i opens a range", viewer.wait_for(b"range from 0:00.3"))
    cli("seek", "1.2")
    viewer.keys(b"o")
    check("o asks for its note", viewer.wait_for(b"note on the range 0:00.3"))
    viewer.keys(b"cut this")
    viewer.keys(b"\r")
    ev = cli("events", "--since", str(since), "--wait", "5").get("events", [{}])
    rng = ev[0] if ev else {}
    check("a range gives both ends in the cut and in the source",
          rng.get("type") == "range" and abs((rng.get("time") or 0) - 0.3) < 0.05 and abs((rng.get("to") or 0) - 1.2) < 0.05
          and abs((rng.get("src_time") or 0) - 1.3) < 0.05 and abs((rng.get("to_src_time") or 0) - 2.2) < 0.05
          and rng.get("to_clip") == 0 and rng.get("note") == "cut this", rng)
    check("and what was said at each end", (rng.get("said") or {}).get("word", {}).get("text") == "hello"
          and (rng.get("to_said") or {}).get("in_silence") is True, rng)
    # Deleting: at 0.7 are the mark and the range around it; d asks first.
    cli("seek", "0.7")
    since = cli("events").get("last_id", 0)
    viewer.keys(b"d")
    check("d asks before deleting the newest mark there",
          viewer.wait_for(b"d again deletes your range 0:00.3") and viewer.wait_for(b": cut this"))
    viewer.keys(b"x")
    viewer.keys(b"d")
    viewer.keys(b"d")
    ev = cli("events", "--since", str(since), "--wait", "5").get("events", [])
    check("any other key keeps it (and isn't taken as itself)", [e.get("type") for e in ev] == ["delete"], ev)
    gone = ev[0] if ev else {}
    check("d again deletes it, and the agent is told", gone.get("deletes") == [rng.get("id")]
          and (gone.get("deleted") or {}).get("type") == "range" and (gone.get("deleted") or {}).get("note") == "cut this", gone)
    viewer.keys(b"d")
    viewer.keys(b"d")
    ids = [e.get("id") for e in cli("events", "--since", "0").get("events", [])]
    check("deleted marks are left out of events", rng.get("id") not in ids and mark.get("id") not in ids, ids)
    cli("seek", "0")
    viewer.keys(b">")
    check("and off the bar", viewer.wait_for(b"No marks yet"))
    cli("close", p.get("source", {}).get("name", "edl"))

    # Its render (here the same cut, made by edit render) shown with the EDL.
    same = os.path.join(edit, "same.json")
    with open(same, "w") as f:
        json.dump({"pepper_edit": 1, "output": {"width": 640, "height": 360, "fps": 30}, "clips": [
            {"src": "../clip.mp4", "in": 1.0, "out": 3.0}, {"src": frames, "in": 0.5, "out": 2.0}]}, f)
    preview = os.path.join(edit, "preview.mp4")
    cli("edit", "render", same, "-o", preview, "--overwrite")
    o = cli("open", preview, "--edit", edl, "--name", "vu")
    check("open --edit shows a render with its cut", o.get("source", {}).get("project") == edl and not o.get("warnings"), o)
    since = cli("events").get("last_id", 0)
    cli("seek", "2.4")
    viewer.keys(b"m")
    ev = cli("events", "--since", str(since), "--wait", "5").get("events", [{}])
    mark = ev[0] if ev else {}
    check("a mark on the render gives the range and the time in its source",
          mark.get("clip") == 1 and mark.get("clip_name") == "END" and mark.get("file") == preview
          and abs((mark.get("src_time") or 0) - 0.9) < 0.05, mark)
    cli("close", "vu")

    fake = os.path.join(work, "skills", "video-use")
    os.makedirs(os.path.join(fake, "helpers"), exist_ok=True)
    open(os.path.join(fake, "SKILL.md"), "w").write("---\nname: video-use\n---\n")
    open(os.path.join(fake, "helpers", "render.py"), "w").write("")
    s = subprocess.run([viewer.agent, "status"], capture_output=True, text=True, cwd=viewer.cwd,
                       env={**viewer.env, "VIDEO_USE_DIR": fake})
    vu = json.loads(s.stdout or "{}").get("video_use", {})
    check("status finds video-use", vu.get("installed") is True and vu.get("path") == fake, vu)


def seekbar_checks(cli, viewer, frames):
    """The human's mouse on a video: its seek bar, and the wheel a frame a notch."""
    src = cli("open", frames, "--name", "frames").get("source", {})
    # The box takes the picture's shape once the player says it.
    deadline = time.time() + 5
    while time.time() < deadline and not (src.get("width") and src.get("box")):
        time.sleep(0.1)
        src = next((s for s in cli("sources").get("sources", []) if s.get("name") == "frames"), {})
    time.sleep(0.2)
    src = next((s for s in cli("sources").get("sources", []) if s.get("name") == "frames"), src)
    col, row, cols, rows = src.get("box") or (0, 0, 1, 1)
    d = src.get("duration") or 4
    # The bar is the row under the box: its middle, in pixels (10×20 cells).
    y = (row + rows) * 20 + 10
    at = lambda t: int(col * 10 + t / d * cols * 10)
    mouse = lambda code, x, end="M": viewer.keys(f"\x1b[<{code};{x};{y}{end}".encode(), 0.05)
    # A pixel of the bar is this long; a frame, 0.04 s.
    near = d / (cols * 10) + 0.041
    since = cli("events").get("last_id", 0)

    def settle(want, tolerance=0.001):
        deadline = time.time() + 5
        while time.time() < deadline:
            w = cli("where", "frames")
            if abs((w.get("time_pos") or -9) - want) <= tolerance:
                return w
            time.sleep(0.1)
        return cli("where", "frames")

    mouse(0, at(2.0))
    mouse(0, at(2.0), "m")
    w = settle(2.0, near)
    check("a click on the seek bar goes to that time", abs((w.get("time_pos") or -9) - 2.0) < near, w)
    check("and the label gives its frame", viewer.wait_for(b"frame "))
    shot = cli("snapshot", "frames")
    got = frame_numbers(shot.get("path", "/nonexistent")) if os.path.isfile(shot.get("path", "")) else []
    check("it shows the frame the bar says", got == [round((w.get("time_pos") or 0) * 25)], (got, w))
    mouse(0, at(1.0))
    mouse(32, at(1.6))
    mouse(32, at(2.4))
    mouse(32, at(3.0))
    mouse(0, at(3.0), "m")
    w = settle(3.0, near)
    check("a drag on the seek bar scrubs, landing where it's let go", abs((w.get("time_pos") or -9) - 3.0) < near, w)

    # The wheel over the picture: three notches forward, one back, a frame each.
    start = cli("where", "frames").get("time_pos") or 0
    px = int((col + cols / 2) * 10)
    py = int((row + rows / 2) * 20)
    for code in (65, 65, 65, 64):
        viewer.keys(f"\x1b[<{code};{px};{py}M".encode(), 0.01)
    w = settle(start + 2 / 25, 0.005)
    check("the wheel steps a frame a notch, however fast", abs((w.get("time_pos") or -9) - (start + 2 / 25)) < 0.005
          and w.get("paused") is True, (start, w))
    shot = cli("snapshot", "frames")
    got = frame_numbers(shot.get("path", "/nonexistent")) if os.path.isfile(shot.get("path", "")) else []
    check("to exactly that frame", got == [round(start * 25) + 2], (got, start))
    check("seeking with the mouse sends no events", not cli("events", "--since", str(since)).get("events"))
    cli("close", "frames")


def web_checks(cli, viewer, work, run_dir):
    if not cli("status").get("deps", {}).get("chrome", {}).get("ok"):
        print("skip web: no Chrome (PEPPER_CHROME)")
        return
    # Room for two pages: the media sources go.
    for source in cli("sources").get("sources", []):
        cli("close", source["name"])
    site = os.path.join(work, "site")
    os.makedirs(site)
    with open(os.path.join(site, "index.html"), "w") as f:
        f.write(PAGE)
    server = serve(site)
    url = f"http://127.0.0.1:{server.server_address[1]}/"

    opened = cli("web", "open", url, "--name", "app")
    src = opened.get("source", {})
    check("web open answers once it has loaded", src.get("status") == "ready" and src.get("kind") == "web", src)
    check("web open says the page's title", "Shop" in (src.get("title") or ""), src)
    check("the page gets a box", viewer.wait_for(b"app  ready"))
    check("the page's title never reaches the terminal raw", b"\x1b]2;pwned" not in viewer.screen())
    blocked = cli("web", "open", "https://example.com/", "--name", "far", ok=False)
    check("a page off this machine is ORIGIN_BLOCKED", blocked.get("error", {}).get("code") == "ORIGIN_BLOCKED", blocked)
    blocked = cli("web", "goto", "https://example.com/", ok=False)
    check("so is going to one", blocked.get("error", {}).get("code") == "ORIGIN_BLOCKED", blocked)
    playback = cli("pause", "--source", "app", ok=False)
    check("a page has no playback: NOT_PLAYABLE", playback.get("error", {}).get("code") == "NOT_PLAYABLE", playback)

    shot = cli("web", "shot")
    check("web shot writes a PNG", os.path.isfile(shot.get("path", "")) and shot.get("width") == 1568, shot)
    head = cli("web", "shot", "--selector", "header")
    check("web shot --selector is the element", head.get("width") == 1568 and 0 < head.get("height", 0) < 200, head)

    raw = subprocess.run([viewer.agent, "web", "console", "--viewer", str(viewer.pid)], capture_output=True, cwd=viewer.cwd, env=viewer.env)
    check("web console escapes the page's text", b"from the page" in raw.stdout and b"\x1b" not in raw.stdout, raw.stdout[:200])
    errors = cli("web", "errors").get("errors", [])
    kinds = {e.get("kind") for e in errors}
    check("web errors has the exception", any("boom" in e.get("text", "") for e in errors), errors)
    check("and the remote request that was stopped",
          any(e.get("kind") == "network" and "example.com" in e.get("url", "") for e in errors), errors)
    check("and the 404", any(e.get("status") == 404 for e in errors), kinds)

    clicked = cli("web", "click", "#b")
    check("web click clicks", cli("web", "eval", "document.querySelector('#b').textContent").get("value") == "clicked 1", clicked)
    cli("web", "type", "#in", "hello")
    cli("web", "press", "Shift+a")
    check("web type and press type", cli("web", "eval", "document.querySelector('#in').value").get("value") == "helloA")
    dom = cli("web", "dom", ".card.bad")
    check("web dom finds it", dom.get("matches") == 1 and "data-testid" in dom.get("html", ""), dom)
    tree = cli("web", "a11y").get("tree", "")
    check("web a11y has the button and the field", 'button "clicked 1"' in tree and 'textbox "Name"' in tree, tree[:300])
    check("web wait-for waits for an element", cli("web", "wait-for", "#late").get("waited_for") == "#late")
    check("web wait-for network-idle", cli("web", "wait-for", "network-idle").get("waited_for") == "network-idle")
    missing = cli("web", "click", "#nope", ok=False)
    check("no such element: NOT_FOUND", missing.get("error", {}).get("code") == "NOT_FOUND", missing)

    phone = cli("web", "viewport", "iphone-15")
    box = cli("where", "app").get("box")
    check("web viewport changes the page and its box",
          phone.get("viewport", {}).get("width") == 393 and box and box[2] < box[3] * 2, (phone, box))
    cli("web", "viewport", "desktop-1440")
    time.sleep(0.3)

    # The human points at the misaligned card and says what's wrong.
    app = cli("where", "app")
    rect = json.loads(cli("web", "eval", "JSON.stringify(document.querySelector('.card.bad').getBoundingClientRect())")["value"])
    since = cli("events").get("last_id", 0)
    x, y = press_at(viewer, app["box"], app["viewport"], (rect["x"] + 30, rect["y"] + rect["height"] / 2))
    viewer.keys(f"\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m".encode())
    check("a click opens the note prompt", viewer.wait_for(b"note on the point"))
    viewer.keys(b"align with the header\r")
    got = cli("events", "--since", str(since), "--wait", "10")
    point = (got.get("events") or [{}])[0]
    element = point.get("element") or {}
    check("a click is a point event", point.get("type") == "point" and point.get("note") == "align with the header", point)
    check("in the page's CSS pixels", abs(point.get("x", 0) - (rect["x"] + 30)) <= 2, point)
    check("with a selector that finds it", "second" in element.get("selector", ""), element)
    check("and its styles", "41px" in element.get("styles", {}).get("margin", ""), element)
    check("and pictures of it", os.path.isfile(point.get("snapshot", "")) and os.path.isfile(point.get("crop", "")), point)
    since = point.get("id", since)
    a = press_at(viewer, app["box"], app["viewport"], (20, 10))
    b = press_at(viewer, app["box"], app["viewport"], (400, 200))
    viewer.keys(f"\x1b[<0;{a[0]};{a[1]}M\x1b[<32;{b[0]};{b[1]}M\x1b[<0;{b[0]};{b[1]}m".encode())
    viewer.keys(b"\x1b")
    got = cli("events", "--since", str(since), "--wait", "10")
    region = (got.get("events") or [{}])[0]
    check("a drag is a region event", region.get("type") == "region" and 370 <= region.get("w", 0) <= 390, region)
    viewer.keys(f"\x1b[<65;{a[0]};{a[1]}M".encode() * 3)
    time.sleep(0.5)
    check("the wheel scrolls the page", (cli("web", "eval", "scrollY").get("value") or 0) > 0)

    # --watch: the page reloads when its folder changes.
    watched = os.path.join(site, "w.html")
    with open(watched, "w") as f:
        f.write("<title>one</title>")
    cli("web", "open", watched, "--name", "w", "--watch", site)
    with open(watched, "w") as f:
        f.write("<title>two</title>")
    deadline = time.time() + 5
    while time.time() < deadline and cli("where", "w").get("title") != "two":
        time.sleep(0.2)
    check("--watch reloads on a change", cli("where", "w").get("title") == "two")

    chrome = lambda: subprocess.run(["pgrep", "-f", f"user-data-dir={run_dir}/{viewer.pid}-chrome"],
                                    capture_output=True, text=True).stdout.split()
    check("the browser runs while pages are open", chrome())
    cli("close", "w")
    cli("close", "app")
    deadline = time.time() + 5
    while time.time() < deadline and chrome():
        time.sleep(0.2)
    check("the browser ends with the last page", not chrome())
    profiles = [d for d in os.listdir(run_dir) if d.startswith(f"{viewer.pid}-chrome")]
    check("and its profile is deleted", not profiles, profiles)
    server.shutdown()


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

    edit_checks(cli, viewer, work, project, clip, hostile)
    web_checks(cli, viewer, work, os.path.dirname(sock_path))

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
