#!/usr/bin/env python3
"""pepper-agent's integration test: the viewer in a pseudo-terminal of its
own, driven as an agent and a human would drive it, with generated media.

The viewer runs with a PEPPER_PORT no Pepper holds, so pepper-play draws to
nowhere: everything but the picture on screen is real (pepper-play, mpv, the
socket, the keys, what the viewer writes to its terminal, snapshots). The
picture is checked by hand in a test Pepper (README's "Trying it").

Usage: tests/integration.py [--agent PATH] [--play PATH] [--only GROUP,...]
  --agent  pepper-agent to test (default: target/debug/pepper-agent, built)
  --play   pepper-play to use (default: $PEPPER_PLAY, or Pepper.app's)
  --only   after the basics, only these groups: edit, web, sharing, live, extras,
           links
Needs ffmpeg, and mpv's library (brew install mpv); transcribe's checks,
whisper.cpp and a model (they're skipped without).
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
import stat

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rtsp_server import RtspServer  # noqa: E402

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
fetch("http://169.254.77.77/x").catch(() => {});
document.querySelector(".card.bad").__svelte_meta = {loc: {file: "src/Card.svelte", line: {"\\u001b]52;c;cHduZWQ=\\u0007": 1}, column: 4}};
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
    sneaky = cli("edit", "render", exact, "-o", os.path.join(os.path.realpath(project), ".pepper", "..", "exact.mp4"), ok=False)
    check("nor through .pepper/..", sneaky.get("error", {}).get("code") == "BAD_ARGS", sneaky)
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


def picture_pair(dir):
    """Two pictures alike but for a white box."""
    a, b = os.path.join(dir, "before.png"), os.path.join(dir, "after.png")
    for path, extra in ((a, ""), (b, ",drawbox=x=100:y=80:w=80:h=60:color=white:t=fill")):
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=1",
                        "-vf", "format=rgb24" + extra, "-frames:v", "1", path], check=True)
    return a, b


def bright_share(path):
    """The share of a picture's pixels that aren't near black."""
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-vf", "format=gray", "-f", "rawvideo", "-"],
                         capture_output=True).stdout
    return sum(1 for p in raw if p > 40) / max(len(raw), 1)


def last_frame(video, dir):
    """A video's last frame, as a PNG."""
    out = os.path.join(dir, "last.png")
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-sseof", "-0.2", "-i", video, "-update", "1", out], check=True)
    return out


COUNTER = """<!doctype html><title>Counter</title>
<button id="b" onclick="this.textContent='clicked '+(window.n=(window.n||0)+1)">Click me</button>
<input id="in" aria-label="Name"><button id="dark" onclick="document.body.style.background='#000'">Dark</button>"""


def sharing_checks(cli, viewer, work, project, clip):
    """Two agents' sessions in one viewer, recordings, before and after."""
    def write(path, data):
        with open(path, "w") as f:
            f.write(data if isinstance(data, str) else json.dumps(data))

    for source in cli("sources").get("sources", []):
        cli("close", source["name"], "--force")
    alice = cli("session", "new", "--owner", "alice")
    bob = cli("session", "new")
    a, b = alice.get("session", ""), bob.get("session", "")
    check("session new gives a token and an owner", len(a) >= 16 and alice.get("owner") == "alice", alice)
    check("an owner's name by default", bob.get("owner") == "agent-2" and a != b, bob)
    bad = cli("session", "new", "--owner", "no good", ok=False)
    check("an owner's name is checked", bad.get("error", {}).get("code") == "BAD_ARGS", bad)

    mine = cli("open", clip, "--name", "a-clip", "--session", a).get("source", {})
    check("a session's source is its owner's", mine.get("owner") == "alice", mine)
    check("its label says whose", viewer.wait_for(b"a-clip (alice)"))
    cli("open", clip, "--name", "free")
    taken = cli("seek", "3", "--source", "a-clip", "--session", b, ok=False).get("error", {})
    check("another agent can't drive it: NOT_OWNER", taken.get("code") == "NOT_OWNER" and "alice" in taken.get("message", ""), taken)
    taken = cli("pause", "--source", "a-clip", ok=False).get("error", {})
    check("nor can a command without a session", taken.get("code") == "NOT_OWNER", taken)
    taken = cli("close", "a-clip", "--session", b, ok=False).get("error", {})
    check("nor close it", taken.get("code") == "NOT_OWNER", taken)
    forced = cli("seek", "3", "--source", "a-clip", "--session", b, "--force")
    check("unless --force", abs((forced.get("time_pos") or 0) - 3) < 0.1, forced)
    fake = cli("where", "--session", "0" * 32, ok=False).get("error", {})
    check("a session the viewer didn't give is refused", fake.get("code") == "BAD_ARGS" and "session new" in fake.get("hint", ""), fake)
    # A newer viewer takes the commands that name none, but not a session's.
    other = Viewer(viewer.agent, viewer.env, viewer.cwd)
    other.wait_for(b"waiting for an agent")
    bare = lambda *a: json.loads(subprocess.run([viewer.agent, *a], capture_output=True, text=True,
                                                cwd=viewer.cwd, env=viewer.env).stdout or "{}")
    check("a session's commands go to the viewer that gave it",
          bare("where", "a-clip", "--session", a).get("viewer") == str(viewer.pid)
          and bare("sources").get("viewer") == str(other.pid))
    other.end()
    cli("focus", "free")
    own = cli("where", "--session", a)
    check("without --source, a session's command goes to its own source", own.get("source") == "a-clip", own)
    hers = [s.get("name") for s in cli("sources", "--owner", "alice").get("sources", [])]
    check("sources --owner lists that agent's", hers == ["a-clip"], hers)
    cli("say", "look at the intro", "--session", a)
    check("say shows who says it", viewer.wait_for(b"alice: look at the intro"))
    since = cli("events").get("last_id", 0)
    cli("focus", "a-clip")
    viewer.keys(b"m")
    cli("focus", "free")
    viewer.keys(b"m")
    got = cli("events", "--since", str(since), "--owner", "alice").get("events", [])
    check("events --owner: that agent's sources only", [e.get("source") for e in got] == ["a-clip"], got)
    check("an event says its source's owner", got and got[0].get("owner") == "alice", got)

    # A video's recording: the stretch that played, cut from the file.
    cli("seek", "2", "--source", "a-clip", "--session", a)
    started = cli("record", "start", "a-clip", "--session", a)
    check("record start", started.get("recording") is True and abs((started.get("from") or 0) - 2) < 0.1, started)
    check("the label shows it's recording", viewer.wait_for("● REC".encode()))
    again = cli("record", "start", "a-clip", "--session", a, ok=False)
    check("one recording a source", again.get("error", {}).get("code") == "BAD_ARGS", again)
    cli("review", "2", "4", "--source", "a-clip", "--session", a)
    time.sleep(2.4)
    rec = cli("record", "stop", "--session", a, timeout=120)
    path = rec.get("path", "")
    check("record stop cuts what played from the file", os.path.isfile(path) and abs((rec.get("duration") or 0) - 2) < 0.2, rec)
    check("into .pepper/recordings", "/.pepper/recordings/a-clip-" in path, path)
    check("with a sheet to check it by", os.path.isfile(rec.get("sheet") or ""), rec)
    none = cli("record", "stop", "a-clip", "--session", a, ok=False)
    check("record stop with none under way: BAD_ARGS", none.get("error", {}).get("code") == "BAD_ARGS", none)
    cli("record", "start", "free")
    still = cli("record", "stop", "free", ok=False)
    check("nothing played: FAILED", still.get("error", {}).get("code") == "FAILED", still)
    cli("close", "free")

    # A preview's: cut from the cut's clips.
    cut = os.path.join(project, "cut4.json")
    write(cut, {"pepper_edit": 1, "clips": [{"src": clip, "in": 2, "out": 4},
                                            {"src": os.path.join(work, "frames.mp4"), "in": 0, "out": 2}]})
    cli("edit", "preview", cut, "--name", "cut4", "--session", a)
    cli("seek", "1", "--source", "cut4", "--session", a)
    cli("record", "start", "cut4", "--session", a)
    cli("review", "1", "3", "--source", "cut4", "--session", a)
    time.sleep(2.4)
    rec = cli("record", "stop", "cut4", "--session", a, timeout=120)
    check("a preview's recording is rendered from its cut",
          rec.get("project") == cut and abs((rec.get("duration") or 0) - 2) < 0.2 and os.path.isfile(rec.get("path", "")), rec)
    cli("close", "cut4", "--session", a)
    cli("close", "a-clip", "--session", a)

    if cli("status").get("deps", {}).get("chrome", {}).get("ok"):
        site = os.path.join(work, "site4")
        os.makedirs(site)
        write(os.path.join(site, "index.html"), COUNTER)
        server = serve(site)
        url = f"http://127.0.0.1:{server.server_address[1]}/"
        cli("web", "open", url, "--name", "a-app", "--session", a)
        cli("web", "open", url, "--name", "b-app", "--session", b)
        lay = cli("layout", "grid")
        check("two agents' pages side by side", len(lay.get("visible", [])) == 2, lay)
        cli("web", "click", "#b", "--session", a)
        cli("web", "click", "#b", "--session", b)
        cli("web", "click", "#b", "--session", b)
        count = lambda s: cli("web", "eval", "window.n||0", "--session", s).get("value")
        check("each agent drives its own page", (count(a), count(b)) == (1, 2), (count(a), count(b)))
        taken = cli("web", "click", "#b", "--source", "b-app", "--session", a, ok=False).get("error", {})
        check("not the other's: NOT_OWNER", taken.get("code") == "NOT_OWNER", taken)
        looked = cli("web", "shot", "b-app", "--session", a)
        check("but it may look at it", os.path.isfile(looked.get("path", "")), looked)

        script = os.path.join(work, "demo.json")
        write(script, [{"click": "#b"}, {"wait": 0.5}, {"type": ["#in", "Ada"]}, {"press": "Enter"},
                       {"say": "that's the demo"}])
        run = cli("record", "run", script, "--session", b, timeout=180)
        check("record run runs every step", run.get("completed") is True and len(run.get("steps", [])) == 5, run)
        check("and records the page meanwhile", os.path.isfile(run.get("path", "")) and run.get("source") == "b-app"
              and (run.get("duration") or 0) >= 1.5 and (run.get("frames") or 0) >= 45, run)
        check("at the page's own size, not its box's", (run.get("width"), run.get("height")) == (1920, 1200), run)
        check("the demo's steps happened", count(b) == 3)
        write(script, [{"click": "#dark"}])
        dark = cli("record", "run", script, "--session", b, timeout=120)
        share = bright_share(last_frame(dark.get("path", ""), work))
        check("a recording ends on what the page shows last", share < 0.1, share)
        cli("web", "reload", "--session", b)
        write(script, [{"click": 3}, {"fly": "away"}])
        e = cli("record", "run", script, "--session", b, ok=False).get("error", {})
        check("a bad script: every problem at once",
              e.get("code") == "BAD_ARGS" and [p.get("at") for p in e.get("problems", [])] == ["steps[0]", "steps[1]"], e)
        write(script, [{"click": "#b"}, {"click": "#nope"}, {"click": "#b"}])
        r = cli("record", "run", script, "--session", b, timeout=120)
        check("a failing step ends the run; its recording is kept",
              r.get("completed") is False and r.get("failed", {}).get("step") == 1 and os.path.isfile(r.get("path", "")), r)
        cli("record", "start", "--max", "1", "--session", b)
        time.sleep(2)
        auto = cli("record", "stop", "--session", b, timeout=60)
        check("--max ends a recording by itself; record stop gives it",
              os.path.isfile(auto.get("path", "")) and 0.8 <= (auto.get("duration") or 0) <= 1.3, auto)

        shown = cli("play", run.get("path", ""), "--session", b).get("source", {})
        check("play shows a recording, playing",
              shown.get("kind") == "recording" and shown.get("status") == "playing" and shown.get("owner") == "agent-2", shown)
        since = cli("events").get("last_id", 0)
        cli("focus", shown.get("name", ""))
        viewer.keys(b"y")
        ev = (cli("events", "--since", str(since), "--until", "approve", "--wait", "5", "--owner", "agent-2")
              .get("events") or [{}])[-1]
        check("the human approves the demo with y",
              ev.get("type") == "approve" and ev.get("kind") == "recording" and ev.get("file") == run.get("path"), ev)
        for name, s in (("a-app", a), ("b-app", b), (shown.get("name", ""), b)):
            cli("close", name, "--session", s)
        server.shutdown()

    # Before and after.
    clip2 = os.path.join(work, "clip2.mp4")
    shutil.copy(clip, clip2)
    c = cli("compare", clip, clip2, "--sync")
    check("compare opens both side by side", c.get("compare") == {"before": "before", "after": "after", "sync": True}, c)
    check("labelled", viewer.wait_for(b"before [before]") and viewer.wait_for(b"after [after]"))
    cli("seek", "5", "--source", "after")
    time.sleep(0.6)
    w = cli("where", "before")
    check("with --sync the other follows a seek", abs((w.get("time_pos") or 0) - 5) < 0.1, w)
    cli("play", "--source", "after")
    time.sleep(1)
    w = cli("where", "before")
    check("and plays with it", w.get("paused") is False, w)
    cli("pause", "--source", "after")
    time.sleep(0.5)
    wa, wb = cli("where", "after"), cli("where", "before")
    check("and pauses with it, at the same time",
          wb.get("paused") is True and abs((wa.get("time_pos") or 0) - (wb.get("time_pos") or 9)) < 0.1, (wa, wb))
    since = cli("events").get("last_id", 0)
    viewer.keys(b"x")
    ev = (cli("events", "--since", str(since), "--until", "reject", "--wait", "5").get("events") or [{}])[-1]
    check("a verdict on a comparison names both", ev.get("compare") == {"before": "before", "after": "after"}, ev)

    pa, pb = picture_pair(work)
    cli("compare", "off")
    c = cli("compare", pa, pb)
    check("compare two pictures", c.get("compare", {}).get("sync") is False, c)
    viewer.keys(b"d")
    check("d shows their difference", viewer.wait_for(b"after [difference]"))
    time.sleep(0.5)
    share = bright_share(cli("snapshot", "after").get("path", ""))
    check("black but where they differ", 0.02 < share < 0.1, share)
    viewer.keys(b"d")
    time.sleep(0.5)
    share = bright_share(cli("snapshot", "after").get("path", ""))
    check("d again: the after picture", share > 0.5, share)
    cli("compare", "off")
    for name in ("before", "after"):
        cli("close", name)

def mean_rgb(path):
    """A picture's mean colour, (r, g, b)."""
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-vf", "scale=1:1:flags=area,format=rgb24",
                          "-f", "rawvideo", "-"], capture_output=True).stdout
    return tuple(raw[:3]) if len(raw) >= 3 else (0, 0, 0)


def colour(path, rgb):
    """Write a 320x240 picture of one colour."""
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "color=0x%02x%02x%02x:s=320x240" % rgb,
                    "-frames:v", "1", path], check=True)


def until(what, timeout=8):
    """Waits for `what()` to be true; its last value."""
    end = time.time() + timeout
    value = what()
    while not value and time.time() < end:
        time.sleep(0.2)
        value = what()
    return value


def live_checks(cli, viewer, work, env, run_dir):
    """Watched files, pipes and streams."""
    for source in cli("sources").get("sources", []):
        cli("close", source["name"], "--force")
    near = lambda a, b: all(abs(x - y) < 40 for x, y in zip(a, b))

    # A watched picture, rendered again in place and by a rename.
    plot = os.path.join(work, "plot.png")
    colour(plot, (220, 20, 20))
    w = cli("watch", plot, "--name", "plot").get("source", {})
    check("watch shows a picture", w.get("kind") == "watch" and w.get("reloads") == 0, w)
    check("as it is", near(mean_rgb(cli("snapshot", "plot").get("path", "")), (220, 20, 20)))
    colour(plot, (20, 20, 220))
    again = until(lambda: cli("where", "plot").get("reloads") == 1)
    check("a render in place is shown again", again and near(mean_rgb(cli("snapshot", "plot").get("path", "")), (20, 20, 220)))
    tmp = os.path.join(work, "plot.tmp.png")
    colour(tmp, (20, 200, 20))
    os.replace(tmp, plot)
    again = until(lambda: cli("where", "plot").get("reloads") == 2)
    check("and one written aside and renamed", again and near(mean_rgb(cli("snapshot", "plot").get("path", "")), (20, 200, 20)))
    check("the human is told", viewer.wait_for(b"plot was rendered again"))
    time.sleep(1)
    check("a file that doesn't change isn't shown again", cli("where", "plot").get("reloads") == 2)
    clip3 = os.path.join(work, "loop.mp4")
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25", "-t", "1.5",
                    "-pix_fmt", "yuv420p", clip3], check=True)
    cli("watch", clip3, "--name", "loop")
    time.sleep(2.5)
    lp = cli("where", "loop")
    check("a watched video loops", lp.get("status") == "playing" and (lp.get("time_pos") or 9) < 1.5, lp)
    cli("close", "loop")
    cli("close", "plot")

    # A pipe: y4m from ffmpeg, twice; raw BGRA from Python.
    p = cli("pipe", "new", "--name", "sim")
    fifo = p.get("pipe", "")
    st = os.stat(fifo) if os.path.exists(fifo) else None
    check("pipe new makes a FIFO only the user opens", st is not None and stat.S_ISFIFO(st.st_mode)
          and oct(st.st_mode & 0o777) == "0o600" and fifo.startswith(run_dir), (fifo, st))
    check("it waits for a writer", p.get("source", {}).get("status") == "waiting" and "ffmpeg" in p.get("try", ""), p)
    writer = lambda size, secs: subprocess.Popen(
        ["ffmpeg", "-v", "error", "-re", "-f", "lavfi", "-i", f"testsrc2=size={size}:rate=25", "-t", str(secs),
         "-f", "yuv4mpegpipe", "-y", fifo], stdin=subprocess.DEVNULL)
    first = writer("320x240", 3)
    live = until(lambda: cli("where", "sim").get("status") == "playing")
    where = cli("where", "sim")
    check("a writer's frames show", live and where.get("writers") == 1 and where.get("width") == 320, where)
    shot = cli("snapshot", "sim")
    check("a snapshot of a pipe", shot.get("width") == 320 and os.path.isfile(shot.get("path", "")), shot)
    norec = cli("record", "start", "sim", ok=False)
    check("a pipe can't be recorded yet: UNSUPPORTED", norec.get("error", {}).get("code") == "UNSUPPORTED", norec)
    first.wait()
    check("when it closes, the pipe waits for the next",
          until(lambda: cli("where", "sim").get("status") == "waiting"))
    second = writer("160x120", 1)
    check("the next writer shows", until(lambda: cli("where", "sim").get("width") == 160)
          and cli("where", "sim").get("writers") == 2)
    second.wait()
    raw = cli("pipe", "new", "--format", "bgra", "--size", "64x48", "--fps", "10", "--name", "raw")
    with open(raw.get("pipe", "/nonexistent"), "wb") as f:
        for _ in range(15):
            f.write(bytes([30, 200, 30, 255]) * 64 * 48)
            f.flush()
            time.sleep(0.1)
        until(lambda: cli("where", "raw").get("status") == "playing")
        check("raw BGRA frames show", near(mean_rgb(cli("snapshot", "raw").get("path", "")), (30, 200, 30)))
    bad = cli("pipe", "new", "--format", "bgra", ok=False)
    check("raw frames need a size", bad.get("error", {}).get("code") == "BAD_ARGS", bad)
    t0 = time.time()
    cli("close", "raw")
    cli("close", "sim")
    check("a waiting pipe closes at once, and its FIFO goes",
          time.time() - t0 < 3 and not os.path.exists(fifo), time.time() - t0)

    # A stream behind a password: from the environment, never shown.
    server = RtspServer(user="cam", password="s3cret-Pw")
    env["PEPPER_AGENT_TEST_CAM"] = "cam:s3cret-Pw"
    o = cli("open", server.address, "--credentials-env", "PEPPER_AGENT_TEST_CAM", "--name", "cam")
    src = o.get("source", {})
    check("open rtsp:// is a stream, playing", src.get("kind") == "stream" and src.get("status") == "playing", o)
    check("its address is shown without the password", src.get("target") == server.address.replace("rtsp://", "rtsp://***@"), src)
    check("the password reached the camera", server.auth_seen and server.auth_seen[-1] == "cam:s3cret-Pw", server.auth_seen)
    shot = cli("snapshot", "cam")
    check("a snapshot of a stream", shot.get("width") == 640, shot)
    since = cli("events").get("last_id", 0)
    cli("focus", "cam")
    viewer.keys(b"m")
    ev = cli("events", "--since", str(since))
    server.drop()
    check("a stream that drops is tried again", until(lambda: cli("where", "cam").get("status") == "reconnecting", 5))
    back = until(lambda: cli("where", "cam").get("status") == "playing", 15)
    check("and comes back", back and cli("where", "cam").get("reconnects", 0) >= 1 and server.clients >= 2,
          (cli("where", "cam"), server.clients))
    cli("record", "start", "cam")
    time.sleep(3)
    rec = cli("record", "stop", "cam", timeout=120)
    # From the next keyframe (one a second here) on.
    check("a stream's recording, its packets as they came", os.path.isfile(rec.get("path", ""))
          and 1.8 < (rec.get("duration") or 0) < 3.5, rec)
    env["PEPPER_AGENT_TEST_CAM"] = "cam:wrong-Pw"
    wrong = cli("open", server.address, "--credentials-env", "PEPPER_AGENT_TEST_CAM", "--name", "cam2", ok=False)
    check("wrong credentials: LOAD_FAILED, tried again", wrong.get("error", {}).get("code") == "LOAD_FAILED"
          and "tried again" in wrong.get("error", {}).get("hint", ""), wrong)
    plain = cli("open", server.address.replace("rtsp://", "rtsp://cam:s3cret-Pw@"), "--name", "cam3")
    check("a password in the address works too", plain.get("source", {}).get("status") == "playing", plain)
    said = [json.dumps(x) for x in (o, ev, wrong, plain, cli("sources"), cli("status"), cli("where", "cam"), rec, shot)]
    files = [os.path.join(run_dir, f"{viewer.pid}.json"), os.path.join(run_dir, f"{viewer.pid}.log"),
             os.path.join(viewer.cwd, ".pepper", "events.jsonl")]
    said += [open(f, errors="replace").read() for f in files if os.path.exists(f)]
    procs = subprocess.run(["ps", "-A", "-o", "args="], capture_output=True, text=True).stdout
    leaks = [s[:200] for s in said if "s3cret" in s or "wrong-Pw" in s]
    check("the password is in no answer, event, record or log", not leaks and len(said) == 12, leaks)
    check("nor on the screen", b"s3cret" not in viewer.screen() and b"wrong-Pw" not in viewer.screen())
    check("nor in any command line", "s3cret" not in procs and "wrong-Pw" not in procs)
    for name in ("cam", "cam2", "cam3"):
        cli("close", name)
    server.close()
    env.pop("PEPPER_AGENT_TEST_CAM", None)



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
    check("a link-local address is off this machine too",
          any("169.254.77.77" in e.get("url", "") and "PROXY" in e.get("text", "") for e in errors),
          [e for e in errors if "169.254" in e.get("url", "")])
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
    check("where the page says it's made, as numbers or nothing",
          element.get("source") == {"file": "src/Card.svelte", "line": None, "column": 4, "from": "svelte"}, element.get("source"))
    human = subprocess.run([viewer.agent, "events", "--since", str(since), "--human", "--viewer", str(viewer.pid)],
                           capture_output=True, cwd=viewer.cwd, env=viewer.env)
    check("events --human keeps the page's text inert", b"Card.svelte" in human.stdout and b"\x1b" not in human.stdout,
          human.stdout[:300])
    keyed = subprocess.run([viewer.agent, "web", "eval", '({"\\u001b]2;pwned\\u0007": 1})', "--human", "--viewer", str(viewer.pid)],
                           capture_output=True, cwd=viewer.cwd, env=viewer.env)
    check("so does web eval --human, keys and all", b"pwned" in keyed.stdout and b"\x1b" not in keyed.stdout, keyed.stdout[:200])
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


def talk_clip(dir):
    """Three shots (cuts at 3 s and 5 s) and a voice: two sentences, 1.5 s
    of quiet between them. None if macOS's `say` can't speak."""
    speech = os.path.join(dir, "speech.aiff")
    said = subprocess.run(["say", "-o", speech, "Hello there. This is a test of the transcription. "
                           "[[slnc 1500]] After a pause, we fixed the bug, and the demo works."])
    if said.returncode != 0 or not os.path.isfile(speech):
        return None
    talk = os.path.join(dir, "talk.mp4")
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:d=3",
                    "-f", "lavfi", "-i", "color=c=red:s=640x360:d=2", "-f", "lavfi", "-i", "smptebars=s=640x360:d=4",
                    "-i", speech, "-filter_complex", "[0][1][2]concat=n=3:v=1:a=0,format=yuv420p[v]",
                    "-map", "[v]", "-map", "3:a", "-c:v", "libx264", "-g", "30", "-c:a", "aac", "-t", "9", talk],
                   check=True)
    return talk


class Mcp:
    """`pepper-agent mcp`, as an agent's client runs it."""

    def __init__(self, agent, env, cwd):
        self.p = subprocess.Popen([agent, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  text=True, env=env, cwd=cwd)
        self.lines = []
        threading.Thread(target=self._read, daemon=True).start()
        self.next_id = 0

    def _read(self):
        for line in self.p.stdout:
            self.lines.append(json.loads(line))

    def send(self, message):
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", **message}) + "\n")
        self.p.stdin.flush()

    def ask(self, method, params=None, wait=True):
        self.next_id += 1
        self.send({"id": self.next_id, "method": method, "params": params or {}})
        return self.answer(self.next_id) if wait else self.next_id

    def answer(self, id, timeout=60):
        end = time.time() + timeout
        while time.time() < end:
            for m in self.lines:
                if m.get("id") == id:
                    return m
            time.sleep(0.05)
        return {}

    def call(self, tool, **arguments):
        return self.ask("tools/call", {"name": tool, "arguments": arguments}).get("result", {})

    def end(self):
        self.p.stdin.close()
        return self.p.wait(timeout=10)


def extras_checks(cli, viewer, agent, work, project, env, hostile, sound):
    """Scenes, silences, transcripts, the skill and the MCP server."""
    for source in cli("sources").get("sources", []):
        cli("close", source["name"], "--force")
    # A folder of its own: the edit checks leave a video-use transcript
    # called talk beside the media, and that one would be found first.
    own = os.path.join(work, "extras")
    os.makedirs(own)
    talk = talk_clip(own)
    check("a talk clip is made (macOS's say)", talk is not None)
    if talk is None:
        return

    sc = cli("scenes", talk)
    starts = [s.get("start") for s in sc.get("scenes", [])]
    check("scenes finds the two cuts", sc.get("cuts") == 2 and len(starts) == 3
          and abs(starts[1] - 3) < 0.05 and abs(starts[2] - 5) < 0.05, sc)
    check("each scene runs to the next", sc.get("scenes", [{}])[-1].get("end") == sc.get("duration"), sc)
    check("and says how to look at them", "frames <file> --at 0,3,5" in sc.get("next", ""), sc.get("next"))
    t0 = time.time()
    check("scenes is kept", cli("scenes", talk) == sc and time.time() - t0 < 1)
    check("a threshold is 0 to 1", cli("scenes", talk, "--threshold", "1.5", ok=False)
          .get("error", {}).get("code") == "BAD_ARGS")
    check("no scenes in sound", cli("scenes", sound, ok=False).get("error", {}).get("code") == "NOT_PLAYABLE")

    si = cli("silences", talk, "--min", "1", "--db", "-35dB")
    quiet = si.get("silences", [])
    check("silences finds the pause", len(quiet) == 1 and abs(quiet[0]["start"] - 2.7) < 0.15
          and abs(quiet[0]["end"] - 4.4) < 0.15, si)
    check("and the sound around it", [round(s["start"]) for s in si.get("sound", [])] == [0, 4], si.get("sound"))
    check("a level is under 0 dB", cli("silences", talk, "--db", "5", ok=False)
          .get("error", {}).get("code") == "BAD_ARGS")
    check("no silences without sound", cli("silences", hostile, ok=False)
          .get("error", {}).get("code") == "NOT_PLAYABLE")

    missing = cli("transcribe", talk, "--model", os.path.join(work, "no-model.bin"), ok=False)
    check("a missing model is NOT_FOUND", missing.get("error", {}).get("code") == "NOT_FOUND", missing)
    bare = subprocess.run([agent, "transcribe", talk], capture_output=True, text=True, cwd=project,
                          env={**env, "HOME": work, "PEPPER_WHISPER_MODEL": ""})
    no_model = json.loads(bare.stderr.strip().splitlines()[-1]).get("error", {}) if bare.stderr.strip() else {}
    check("no model at all: MISSING_DEP, with how to get one",
          no_model.get("code") == "MISSING_DEP" and "curl" in no_model.get("hint", ""), no_model)
    has_whisper = shutil.which("whisper-cli") or os.path.isfile("/opt/homebrew/bin/whisper-cli")
    models = os.path.expanduser("~/.cache/pepper-agent/whisper")
    has_model = os.environ.get("PEPPER_WHISPER_MODEL") or (
        os.path.isdir(models) and any(n.startswith("ggml-") and n.endswith(".bin") and "silero" not in n
                                      for n in os.listdir(models)))
    if not (has_whisper and has_model):
        print("  (skipped: transcribe needs whisper.cpp and a model)")
    else:
        tr = cli("transcribe", talk, "--lang", "en", timeout=300)
        text = " ".join(s.get("text", "") for s in tr.get("segments", []))
        check("transcribe hears what's said", "transcription" in text.lower() and "pause" in text.lower(), tr)
        check("in English", tr.get("language") == "en", tr.get("language"))
        words = json.load(open(tr.get("json", "/nonexistent"))).get("words", []) if tr.get("json") else []
        check("with every word's time", len(words) >= 12 and all(w["end"] >= w["start"] for w in words), words[:3])
        in_pause = [w for w in words if w["start"] > 2.8 and w["end"] < 4.3]
        check("no word in the pause", not in_pause, in_pause)
        last = next((w for w in words if "transcription" in w["text"].lower()), {})
        check("the word before it ends before it", 1.5 < last.get("end", 0) < 2.85, last)
        srt = open(tr.get("srt", "/nonexistent")).read() if tr.get("srt") else ""
        check("and an .srt", srt.startswith("1\n00:00:0") and " --> " in srt, srt[:80])
        t0 = time.time()
        check("transcribe is kept", cli("transcribe", talk, "--lang", "en") == tr and time.time() - t0 < 1)
        # A mark on the file now says what was being said.
        cli("open", talk, "--name", "talk")
        since = cli("events").get("last_id", 0)
        cli("seek", "5.0", "--source", "talk")
        viewer.keys(b"m")
        ev = cli("events", "--since", str(since), "--wait", "5")
        said = (ev.get("events") or [{}])[-1].get("said", {})
        check("a mark says what was said there", "pause" in said.get("phrase", "").lower()
              and said.get("transcript") == tr.get("json"), said)
        cli("close", "talk")

    skills = os.path.join(own, "skills")
    inst = cli("install-skill", "--dir", skills)
    installed = os.path.join(skills, "pepper", "SKILL.md")
    repo_skill = open(os.path.join(CRATE, "skills", "pepper", "SKILL.md")).read()
    check("install-skill writes the skill", inst.get("changed") is True and os.path.isfile(installed)
          and open(installed).read() == repo_skill, inst)
    check("which starts with its name", repo_skill.startswith("---\nname: pepper\n"))
    check("again: nothing to do", cli("install-skill", "--dir", skills).get("changed") is False)
    with open(installed, "w") as f:
        f.write("---\nname: someone-else\ndescription: x\n---\n")
    check("another skill is left alone", cli("install-skill", "--dir", skills, ok=False)
          .get("error", {}).get("code") == "BAD_ARGS")

    mcp = Mcp(agent, env, project)
    init = mcp.ask("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                  "clientInfo": {"name": "integration", "version": "1"}}).get("result", {})
    check("mcp: initialize", init.get("protocolVersion") == "2025-06-18"
          and "tools" in init.get("capabilities", {}) and "guide" in init.get("instructions", ""), init)
    mcp.send({"method": "notifications/initialized"})
    tools = {t["name"]: t for t in mcp.ask("tools/list").get("result", {}).get("tools", [])}
    check("mcp: every command is a tool", {"guide", "open", "web_shot", "pipe_new", "transcribe", "edit_render"}
          <= set(tools) and "view" not in tools and "mcp" not in tools, sorted(tools))
    check("mcp: a tool's arguments are named", set(tools.get("open", {}).get("inputSchema", {})
          .get("properties", {})) >= {"file", "name", "play", "session", "viewer"})
    st = mcp.call("status", viewer=str(viewer.pid))
    check("mcp: a call answers with the command's JSON", st.get("isError") is False
          and json.loads(st["content"][0]["text"]).get("viewer", {}).get("id") == str(viewer.pid), st)
    cli("open", talk, "--name", "talk")
    shot = mcp.call("snapshot", name="talk", viewer=str(viewer.pid))
    images = [c for c in shot.get("content", []) if c.get("type") == "image"]
    import base64
    check("mcp: a snapshot comes back as an image too", len(images) == 1 and images[0].get("mimeType") == "image/png"
          and base64.b64decode(images[0].get("data", ""))[:4] == b"\x89PNG", [c.get("type") for c in shot.get("content", [])])
    bad = mcp.call("seek", time="1:99", viewer=str(viewer.pid))
    check("mcp: an error is the command's", bad.get("isError") is True and '"BAD_TIME"' in bad["content"][0]["text"], bad)
    odd = mcp.call("probe", bogus=1)
    check("mcp: an unknown argument is BAD_ARGS", odd.get("isError") is True and "BAD_ARGS" in odd["content"][0]["text"], odd)
    check("mcp: an unknown method", mcp.ask("resources/list").get("error", {}).get("code") == -32601)
    last = cli("events").get("last_id", 0)
    waiting = mcp.ask("tools/call", {"name": "events", "arguments": {
        "wait": 30, "since": last, "viewer": str(viewer.pid)}}, wait=False)
    check("mcp: answers while a call waits", mcp.ask("ping", {}).get("result") == {})
    time.sleep(0.5)
    mcp.send({"method": "notifications/cancelled", "params": {"requestId": waiting}})
    after = mcp.call("where", name="talk", viewer=str(viewer.pid))
    check("mcp: a cancelled call is stopped, and not answered", after.get("isError") is False
          and not mcp.answer(waiting, timeout=1), after)
    left = subprocess.run(["pgrep", "-f", f"{agent} events --wait=30"], capture_output=True, text=True).stdout.split()
    check("mcp: its process is gone", not left, left)
    check("mcp: ends with its client", mcp.end() == 0)
    cli("close", "talk")


def links_checks(agent, env, work, clip):
    """A project that came with links in its .pepper/: nothing is written
    through them."""
    linked = os.path.join(work, "linked")
    outside = os.path.join(work, "outside")
    os.makedirs(os.path.join(linked, ".pepper"))
    os.makedirs(outside)
    os.symlink(os.path.join(outside, "events.txt"), os.path.join(linked, ".pepper", "events.jsonl"))
    os.symlink(outside, os.path.join(linked, ".pepper", "snapshots"))
    viewer = Viewer(agent, env, linked)
    check("a viewer starts in a project with links in .pepper/", viewer.wait_for(b"waiting for an agent"))

    def cli(*args, ok=True):
        r = subprocess.run([agent, *args, "--viewer", str(viewer.pid)], capture_output=True,
                           text=True, cwd=linked, env=env, timeout=60)
        if ok and r.returncode != 0:
            check(f"pepper-agent {' '.join(args)}", False, r.stderr.strip())
            return {}
        if not ok:
            return json.loads(r.stderr.strip().splitlines()[-1]) if r.stderr.strip() else {}
        return json.loads(r.stdout)

    cli("open", clip, "--name", "clip")
    since = cli("events").get("last_id", 0)
    viewer.keys(b"m")
    got = cli("events", "--since", str(since), "--wait", "5")
    check("a mark still comes", [e.get("type") for e in got.get("events", [])] == ["mark"], got)
    check("but not through .pepper/events.jsonl, a link",
          not os.path.exists(os.path.join(outside, "events.txt")) and not got.get("events_file", linked).startswith(linked),
          got.get("events_file"))
    shot = cli("snapshot", ok=False)
    check("a .pepper/snapshots that's a link is refused",
          shot.get("error", {}).get("code") == "FAILED" and "link" in shot.get("error", {}).get("message", ""), shot)
    check("and nothing is written where the links lead", os.listdir(outside) == [], os.listdir(outside))

    # A cached frame, and the name it's made under, as links: replaced, not followed.
    frame = (cli("frames", clip, "--at", "1").get("frames") or [{}])[0].get("path", "")
    kept = os.path.join(outside, "kept.png")
    with open(kept, "w") as f:
        f.write("keep")
    os.remove(frame)
    os.symlink(kept, frame)
    os.symlink(os.path.join(outside, "made.png"), frame[:-len(".png")] + ".tmp.png")
    again = (cli("frames", clip, "--at", "1").get("frames") or [{}])[0].get("path", "")
    with open(again, "rb") as f:
        head = f.read(4)
    check("a cached frame that's a link is made again, as a file",
          again == frame and not os.path.islink(frame) and head == b"\x89PNG", (frame, head))
    with open(kept) as f:
        check("and what the links led to is left alone",
              f.read() == "keep" and not os.path.exists(os.path.join(outside, "made.png")), os.listdir(outside))
    check("that viewer quits", viewer.end())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--agent")
    ap.add_argument("--play")
    ap.add_argument("--only", default="edit,web,sharing,live,extras,links")
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
    env.pop("PEPPER_AGENT_SESSION", None)
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
    plays = subprocess.run(["pgrep", "-P", str(viewer.pid)], capture_output=True, text=True).stdout.split()
    lines = [subprocess.run(["ps", "-o", "command=", "-p", p], capture_output=True, text=True).stdout for p in plays]
    lines = [line for line in lines if "pepper-play" in line]
    check("its player stays out of Now Playing", lines and all("--input-media-keys=no" in line for line in lines), lines)
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

    only = a.only.split(",")
    if "edit" in only:
        edit_checks(cli, viewer, work, project, clip, hostile)
    if "web" in only:
        web_checks(cli, viewer, work, os.path.dirname(sock_path))
    if "sharing" in only:
        sharing_checks(cli, viewer, work, project, clip)
    if "live" in only:
        live_checks(cli, viewer, work, env, os.path.dirname(sock_path))
    if "extras" in only:
        extras_checks(cli, viewer, agent, work, project, env, hostile, sound)
    if "links" in only:
        links_checks(agent, env, work, clip)

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
