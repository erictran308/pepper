"""Starts tuitube-video as tuitube does, on a real YouTube stream, several
times; says how long each lived, mpv's end-file reason and its warnings."""
import glob, json, os, secrets, socket, subprocess, sys, threading, time

HELPER = glob.glob(os.path.expanduser(
    "~/Library/Application Support/tuitube/video/*/tuitube.app/Contents/MacOS/tuitube-video"))[0]
ARGS = ["--no-config", "--load-scripts=no", "--terminal=no", "--ytdl=no", "--tls-verify=yes",
        "--keep-open=no", "--save-position-on-quit=no", "--resume-playback=no", "--hwdec=auto-safe",
        "--audio-format=float", "--force-window=immediate", "--idle=once", "--input-ipc-client=fd://3"]
URL = "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
COVER, SUBS = sys.argv[2], sys.argv[3]
RUNS, SECONDS = int(sys.argv[1]) if len(sys.argv) > 1 else 5, 10

fmt = "bv*[height<=1080][vcodec!^=av01]+ba/b[height<=1080]/b"
urls = subprocess.run(["/opt/homebrew/bin/yt-dlp", "--ignore-config", "--no-playlist", "-f", fmt,
                       "-g", "--", URL], capture_output=True, text=True).stdout.split()
print(f"{len(urls)} stream URLs", flush=True)
video, audio = urls[0], (urls[1] if len(urls) > 1 else None)

def field(text):
    return f"%{len(text.encode())}%{text}"

for run in range(RUNS):
    token = secrets.token_hex(16)
    ours, theirs = socket.socketpair()
    env = {"HOME": os.environ["HOME"], "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
           "TMPDIR": os.environ.get("TMPDIR", "/tmp"), "PEPPER_PORT": os.environ["PEPPER_PORT"]}
    proc = subprocess.Popen([HELPER, *ARGS], env=env, cwd=os.environ["HOME"], stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, close_fds=False,
                            preexec_fn=lambda: os.dup2(theirs.fileno(), 3))
    theirs.close()
    start = time.monotonic()
    events = []
    events_sent = []

    def read():
        for line in ours.makefile("rb"):
            try:
                m = json.loads(line)
            except ValueError:
                continue
            e = m.get("event")
            if e == "playback-restart" and not events_sent:
                events_sent.append(1)
                ours.sendall((json.dumps({"command": {"name": "video-add", "url": COVER, "flags": "auto", "albumart": True}}) + "\n").encode())
            if e in ("file-loaded", "playback-restart", "end-file", "shutdown", "video-reconfig", "audio-reconfig"):
                extra = f" reason={m.get('reason')} error={m.get('file_error')}" if e == "end-file" else ""
                events.append(f"{time.monotonic() - start:5.1f}s {e}{extra}")
            if m.get("error") not in (None, "success"):
                events.append(f"{time.monotonic() - start:5.1f}s reply error: {m.get('error')}")

    threading.Thread(target=read, daemon=True).start()
    commands = [["set", "force-media-title", "Test video"], ["set", "title", "Test video — tuitube"],
                ["set", "volume", "0"], ["set", "mute", "no"], ["set", "sub-visibility", "no"],
                ["set", "loop-file", "no"]]
    commands += [["observe_property", i, p] for i, p in enumerate(["time-pos", "duration", "pause", "volume", "mute"], 1)]
    commands += [["keybind", "NEXT", "script-message tuitube next"], ["keybind", "PREV", "script-message tuitube previous"]]
    commands += [["script-message-to", "main", "pepper-size", "1280", "720"],
                 ["script-message-to", "main", "pepper-token", token]]
    commands.insert(0, ["set", "start", "30"])
    if audio:
        commands.append(["change-list", "audio-files", "append", audio])
    if SUBS:
        commands.append(["change-list", "sub-files", "append", SUBS])
    commands.append(["loadfile", f"edl://!no_clip;!no_chapters;!global_tags,artist={field('Rick Astley')};{field(video)};", "replace"])
    for c in commands:
        ours.sendall((json.dumps({"command": c}) + "\n").encode())
    try:
        proc.wait(timeout=SECONDS)
        how = f"DIED after {time.monotonic() - start:.1f}s (exit {proc.returncode})"
    except subprocess.TimeoutExpired:
        how = f"alive after {SECONDS}s"
        ours.shutdown(socket.SHUT_RDWR)
        ours.close()
        proc.wait(timeout=5)
    err = proc.stderr.read().decode(errors="replace").strip().splitlines()
    print(f"run {run + 1}: {how}", flush=True)
    for e in events:
        print(f"    {e}")
    for line in err[-8:]:
        print(f"    stderr: {line[:200]}")
