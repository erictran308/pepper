# pepper-agent: helpers that let AI agents use Pepper

As of 2026-10-15. A working note for `pepper-agent` (the spec: `~/Downloads/pepper-agent-spec.md`), which comes after the player's own optimizations (`notes/player-perf.md`). What was checked before designing, and where the spec and the code disagree.

## Where the spec and the code disagree

- **The README's "45% less CPU, under a third of the memory"** was measured with `--hwdec=auto-safe` on both sides (tuitube's helper, `notes/Inline video.md`, 2026-10-14), not with both decoding on the CPU as the spec says. What *is* true: `pepper-play` run from a shell sets no `hwdec`, and libmpv's default is `no`, so `pepper-play video.mp4` decodes on the CPU. tuitube passes `--hwdec=auto-safe` itself.
- **`bench/measure.sh <command…>`**: it takes a clip and mpv options rather than a whole command, because it runs two players (pepper-play in a box, mpv in its own window) with the same options.

## Checked before designing (spec 0.3)

### A command an agent runs can't reach the tab (confirmed)

In Claude Code's shell tool, inside a Pepper tab (2026-10-15): `PEPPER_PORT` is set and `TMUX` isn't, but the command runs in a session of its own with no controlling terminal. `tty` says "not a tty", `open("/dev/tty")` fails with `ENXIO` (Device not configured), and stdin, stdout and stderr are pipes. So a command started by the agent can't write `OSC 777;pepper` to the tab, which is why the human runs `pepper-agent view` in a split and agents talk to it over a socket. Other agents (Aider, a custom one) may run commands with the terminal still theirs; nothing may rely on that either way.

`PEPPER_PORT` reaching the agent's commands does mean they can find the viewer's Pepper, and the user's Pepper, by name; that's no more than any program in the tab could.

### Mouse reporting: SGR (1006) and SGR pixels (1016) both supported

`src/terminal/modes.zig` has `mouse_format_sgr` (1006) and `mouse_format_sgr_pixels` (1016). In 1016, `Surface.zig` reports the position in the terminal's pixel coordinates (the surface's, converted with `Coordinate.convert(.terminal, …)`, so the padding is taken off), in device pixels. The viewer can use 1016 to map a click in a box to the source's pixels, against the box's pixel rectangle that Pepper reports (`size`).

### Screen Recording: Pepper would be the one asking (confirmed from sources)

TCC charges a request to the *responsible process*, which a child started with fork or posix_spawn inherits. A tool in a Pepper tab calling ScreenCaptureKit would make Pepper.app the app prompted and granted, and then every program in every tab would have it. Ghostty doesn't disclaim responsibility for its shells (no `responsibility_spawnattrs_setdisclaim` in `src/` or `macos/`).

Ways to make a separate helper the responsible one:

1. **Disclaim at spawn** (what LLDB, Chromium, Firefox and Qt Creator do; Chrome Remote Desktop does it for exactly this, its host's Screen Recording check): `posix_spawn` the helper with `responsibility_spawnattrs_setdisclaim(&attr, 1)`. Private, in libquarantine (re-exported by libSystem; `libquarantine.tbd` in the 27.0 SDK lists it), no header: look it up with `dlsym`. Signature, from LLDB: `errno_t responsibility_spawnattrs_setdisclaim(posix_spawnattr_t *attrs, bool disclaim)`. The helper keeps its stdio, fds and `waitpid`.
2. **Start it through LaunchServices** (`open -n -g -a`, `NSWorkspace.openApplication`): works for an .app, but loses stdio and inherited fds, so it talks back by Mach name or a socket.
3. Not a LaunchAgent, SMAppService login item or an XPC service inside Pepper.app: those are charged to Pepper.

What the helper needs:

- **Its own .app** (`CFBundleIdentifier`, `LSUIElement`): on macOS 26 a bare executable doesn't show in the Settings pane at all.
- **A stable signature for the grant to last.** TCC matches the designated requirement; an ad hoc signature's is its cdhash, so each build is a new identity and a new prompt (Pepper's is `cdhash H"…" or cdhash H"…"`). Apple Development (a free personal team), Developer ID, or a self-signed code-signing certificate would keep it. This Mac has no signing identity yet.
- **Expect the monthly alert**: since macOS 15, direct `SCStream` capture brings "… is requesting to bypass the system private window picker" (Allow For One Month). replayd on 27.2 still has it.
- **`SCContentSharingPicker`** (macOS 14+) avoids both TCC and the alert: the user picks the Simulator's window once a session. It needs a GUI app process, so the helper would present it, not the CLI. Prototype this first.
- Keep it narrow: capture only windows whose owner is `com.apple.iphonesimulator`, and send pixels only to Pepper's port with the viewer's token.

The permission's place: System Settings → Privacy & Security → Screen & System Audio Recording (macOS 15+).

Sources: Qt's "The curious case of the responsible process" (qt.io/blog), LLDB's `PosixSpawnResponsible.h`, Chromium's `base/process/launch.h` and `remoting/host/mac/permission_process_utils.cc`, Apple developer forums threads 731504, 765103, 766455, 795739 and 807898.

### YUV surfaces as `CALayer.contents`: not checked yet

Wanted only for the optional raw decoder path (player-perf 10.7). Chromium shows NV12 IOSurfaces as layer contents for its video overlays, which suggests it works; to be checked on this Mac with a test window and a known pattern before any code depends on it.

## Phase 1: the viewer, video review (2026-10-15)

Built as `pepper-agent/`, a Rust crate of its own beside pepper-play, with no dependencies (a small JSON reader and writer of its own, `json.rs`). `pepper-build.sh` puts it in the app as `Contents/MacOS/pepper-agent`, on every Pepper shell's PATH, signed with the hardened runtime and no entitlements. It never links libmpv: the viewer runs one pepper-play per source as an app's player (IPC on fd 3, the token over the IPC) and places the boxes itself, so pepper-agent stays MIT and needs nothing loaded.

What's in it: `view`, the socket, `status`, `open`/`close`/`sources`, `layout`/`focus`, `snapshot`, `say`, `events` (marks, notes, done), the playback commands, `probe`/`frames`/`sheet`, `guide`, and the AGENTS.md, CLAUDE.md and README sections. Tests: `cargo test` (40 unit tests) and `tests/integration.py` (57 checks: the real viewer, pepper-play and mpv in a pseudo-terminal, with a PEPPER_PORT no Pepper holds; hostile file names, tags, `say` text and typed and pasted notes; the socket's refusals).

### Where it differs from the spec, and why

- **The socket's folder** comes from `confstr(_CS_DARWIN_USER_TEMP_DIR)`, what `TMPDIR` says in a login shell, not from `TMPDIR` itself: an agent's tool can set its own `TMPDIR` for the commands it runs, and the viewer and the agent must find the same folder. A viewer's id is its pid.
- **`open` shows the first frame paused**; `--play` plays. Nothing starts making sound before the human is looking. Opening a `--name` that exists shows the new target in that source's box (a re-rendered file, the next take).
- **A `done` event and `events --until`**, not in the spec: with marks in a video, the first mark would end an `events --wait` while the human is still watching. The human presses Enter when they've finished; `--until done` returns everything up to it. Plain `--wait` still answers at the first event.
- **More error codes:** `BAD_ARGS` (a command line), `BAD_REQUEST` (the socket), `NOT_FOUND`, `NOT_IN_PEPPER` (`view` outside Pepper or in tmux), `LOAD_FAILED` (mpv couldn't play it), `FAILED` (anything else, with the message). `events --wait` that runs out is `TIMEOUT`.
- **`snapshot`**: the viewer asks the source's pepper-play for the frame it shows (`screenshot-to-file … video`: mpv's decoded frame, not Pepper's screen; 88 ms for 1080p, VideoToolbox included) into its own folder; the command then crops and shrinks it with ffmpeg into `.pepper/snapshots/` in the agent's folder. The viewer writes only in its own folders.
- **Events are kept** in `.pepper/events.jsonl` of the viewer's project: the folder it started in (a Ghostty split starts in the agent's), or `--project`.
- **Contact sheets' times** are drawn by pepper-agent in a 5×7 pixel font: Homebrew's ffmpeg 9 has no `drawtext` (no freetype) and no `subtitles`. ffmpeg decodes each tile (VideoToolbox) as raw RGB, pepper-agent lays them out, and ffmpeg writes the PNG.
- **The cache key** is a fingerprint (path, size, time, inode, first and last 64 KB), not a hash of the whole file: a 4K file would take seconds to hash on every command.
- **Keys passed to a source** are a fixed list of mpv's: pause, seeks, frame steps, speed, volume, subtitle and audio tracks, mpv's progress and stats. Not passed: `q` (the viewer's), `m` (mark; mpv's mute), `l`/`L` (mpv's A-B loop; the agent's `loop` owns it), mpv's console and screenshots.
- **Layout** came in Phase 1 already (`auto`, `single`, `split-h`, `split-v`, `grid`, `z`): two sources need somewhere to go. Each box takes its picture's shape in its tile, the label right over it.
- **Only media** in this phase: a localhost address (a web preview) and other schemes (streams) are `UNSUPPORTED` until their phases.
- **SECURITY.md** is the reporting policy, so it names pepper-agent among what's Pepper's; what pepper-agent does for safety is in the README's Security section and `pepper-agent/README.md`.

### Measured (debug build, M5 Pro)

| What | Time |
| --- | --- |
| `status`, `where`, `say`, `sources` | 6–20 ms |
| `seek` (answers once the frame shows) | about 220 ms |
| `snapshot` of 1080p (mpv's frame, then ffmpeg) | about 200 ms |
| `probe` (not cached) | 0.4 s |
| `frames`, three of 1080p | 0.3 s |
| `sheet`, 4×4 of a 70 s 2160p HEVC | 2.6 s |

The viewer itself, two 1080p sources playing: 0.9% of a core (it follows each player's `time-pos`, at the frame rate); 0.67% built for release, as Pepper.app has it. The players under it cost what they cost alone (9.9% for 1080p30, 18.5% for 1080p60, as `notes/player-perf.md`), and pepper-play didn't change, so the Phase 0 bench wasn't run again.

### Discovery

AGENTS.md and CLAUDE.md reach an agent working in this repo. In any other folder an agent learns of pepper-agent only if it tries `pepper-agent --help` (its first line says to run `guide`), from the line `pepper-agent/README.md` gives the human for their own agent instructions, or from the skill (`install-skill`, Phase 7).

### Checked

- **By hand, in a test Pepper:** two sources stacked, each label right over its box, the agent's line and the keys at the bottom. The user played, focused, zoomed, marked twice, wrote a note and pressed Enter; `events --wait --until done` answered with all four.
- **A fresh agent:** `claude -p "review video.mp4 with me in Pepper"` in a folder holding `video.mp4` and a `CLAUDE.md` of only the README's line, with Pepper's `PATH` (the build's folder last, as Pepper adds it). A viewer ran in a pseudo-terminal, and a script played the human: space, `m`, `m`, `n` and a note, Enter, once the agent waited. The agent's first command was `command -v pepper-agent && pepper-agent guide`. Then it ran `status`, `probe` and `sheet` (and looked at the sheet), then `open`, `say`, and `events --wait 580 --until done --since 0` with a 10-minute timeout on its shell tool. It had the two marks and the note 14 s later. On its own it then looked at the marked times (`frames`, `seek` and `snapshot`, the pixels' values). It found the file steady there, `loop`ed 0:03–0:07, and asked a follow-up with `say` and `events --since 4`, the guide's loop. It was stopped there, since the scripted human answers only once.

## Phase 2: web previews and pointing (2026-10-15)

A source can be a page: the viewer runs a headless Chrome of its own (an installed Chrome or Chromium, Playwright's, or `PEPPER_CHROME`), shows each page live in a box, and the agent drives it with `pepper-agent web …` (`open`, `goto`, `reload`, `back`, `click`, `type`, `press`, `scroll`, `hover`, `viewport`, `wait-for`, `shot`, `console`, `errors`, `dom`, `a11y`, `eval`). The human points: a click on any source is a `point` event, a drag a `region`, with pictures from the source and, for a page, what's there. Tests: 63 unit tests (and a measurement, ignored) and 87 integration checks, 30 of them on a local page in the real Chrome.

### Where it differs from the spec, and why

- **The DevTools protocol over a pipe, not a WebSocket** (`--remote-debugging-pipe`: commands on the browser's fd 3, answers on 4, NUL-separated JSON). No port is opened, so no other program on the Mac can reach the browser, and no WebSocket code is needed.
- **One Chrome per viewer, a browser context per page**: each page has its own cookies, storage and cache, and the browser starts with the first page and ends with the last. Its profile is new, in the viewer's own folder, and deleted after; it's never the user's. Downloads are refused, JavaScript dialogs accepted (nothing in a headless page would answer them) and logged to the console.
- **This machine only, by a proxy**: a page's context has a proxy that answers nothing, bypassed for `localhost`, `*.localhost`, `127.0.0.1` and `[::1]`, so the page and everything it loads (fetches, images, WebSockets) reach only this machine; WebRTC's UDP is kept to the proxy too. `open`, `goto` and `back` to an address elsewhere are `ORIGIN_BLOCKED` before they go, and `eval` runs only on a page of this machine. `--allow-remote` opens a context with no proxy.
- **The frame path is neither of the spec's two**: Chrome's screencast JPEGs are decoded by the graphics chip's JPEG decoder (VideoToolbox) and copied into an IOSurface of the frame's size, which goes to Pepper through pepper-play's own Mach client (`pepper.rs` and `surface.rs`, shared by `#[path]`, both MIT). Pepper fits it to the box (`.resizeAspect`), and Chrome sends frames no bigger than the box, so nothing is scaled. Measured below against ImageIO on the CPU and against mpv fed the same JPEGs.
- **Frames at twice the CSS size, 30 a second**: headless Chrome's screencast is at the browser's own scale whatever the page's emulated one, so it runs with `--force-device-scale-factor=2`, sharp on a Retina display. It sends every second frame of an animation: making 2880×1800 JPEGs costs Chrome 151% of a core at 60 a second, 85% at 30 (below). A still page sends nothing.
- **Points and regions on videos too**: with the media time, and pictures from the player's frame. The pictures (`snapshot`, the whole frame; `crop`, the part around) go in `.pepper/points/` of the viewer's project. After a click a note prompt opens: Enter sends the event with the note, Esc sends it without.
- **A click in a box is a point, not a click on the page**: the agent drives the page. The wheel scrolls it, `r` reloads, `↑` `↓` and Page Up/Down scroll. Clicking through to the page could come later as a mode.
- **What's at a point** is worked out in an isolated world (the page's scripts can't change what describes it): a selector that finds only it (an id, a test attribute, else a short path of classes and `:nth-of-type`), tag, classes, text, box, the styles that place it, its parent's selector and box; role and name from Chrome's accessibility tree. `source` comes from the app's dev build, in the page's own world and only on this machine's pages: React 17/18's `_debugSource`, the first app frame of React 19's `_debugStack` (its lines are those of the code the dev server sends), Vue's component file, Svelte's location, or an inspector plugin's `data-` attribute.
- **The title is the page's own `document.title`**: Chrome's target info gives the address until much later, if ever.
- **`open` takes a page too**: a localhost address or an `.html` file opens as a web source; `--kind web` for another address.
- **Device presets**: `desktop-1440` (the default, 1440×900 at 2), `desktop-1920`, `laptop-1280`, `ipad`, `iphone-15`, `pixel-8` (with their user agents and touch), or `WxH@DPR`.
- **Not here yet:** `schema` (spec section 3) comes with the edit project, Phase 3, the first thing with a schema of its own.

### The frame path, measured

60 JPEGs of 2880×1800 (ffmpeg's `testsrc2`, busier than a page), on the M5 Pro:

| Way | Time a frame | CPU a frame |
| --- | --- | --- |
| VideoToolbox decode, copy into the surface (used) | 5.1 ms median, 6.0 ms p95 | about 1 ms (the copy) |
| ImageIO on the CPU, drawn into the surface (for frames under 64 px a side) | 16.6 ms median (8.7 decode, 4.0 draw) | the same |
| mpv: pepper-play fed the JPEGs over a pipe (mpjpeg), no Pepper, drawing at 640×360 | 4.5 to 7.8 ms to mpv's frame count moving | about 10 ms |

mpv's number stops before pepper-play draws the frame and waits for the display's next refresh, and it decodes on the CPU and needs a player per page: the graphics chip's decoder is quicker to the screen and nearly free. It traps inside VideoToolbox, taking the process with it, on frames under about 64 pixels a side (4×2, 64×16, 17×63 and 1×1 did; 64×17, 49×17, odd sizes, a phone's 1179×2556, truncated and corrupted JPEGs didn't), so those go to ImageIO. Run again with `PEPPER_BENCH_JPEGS=<folder> cargo test --release draw_speed -- --ignored --nocapture`.

Chrome's own cost for a page animating all the time (a box sliding, 1440×900 CSS pixels):

| Scale | Frames a second | Chrome |
| --- | --- | --- |
| 2 | 60 | 151% of a core |
| 2 | 30 (used) | 85% |
| 1 | 60 | 49% |
| 1 | 30 | 40% |

JPEG quality 60 instead of 85 changed nothing (148%).

### Measured in a test Pepper (release build)

| What | Result |
| --- | --- |
| An agent's `web click` to its frame from Chrome | 33.7 ms median (27.6 to 44.9) |
| … to that frame shown in Pepper | 40.0 ms median (34.9 to 98.0); the spec's target is under 100 |
| A page animating, 30 frames a second | viewer 2.4%, Chrome 85.3%, the test Pepper 2.7% of a core |
| A still page | viewer 0.0%, Chrome 0.1% |
| `web open`, the browser starting | 0.44 s (4.4 s the very first time on this Mac) |
| `web open` of another page, `goto` | 0.13 s, 18 ms |
| `click` (100 ms of it waiting for the page), `type`, `eval`, `dom`, `a11y` | 116 ms; 6 to 70 ms |

At 60 frames a second the same click was 29.3 ms to Chrome's frame and 36.2 ms to Pepper (25.3 to 53.4).

### Checked

- **By hand, in a test Pepper:** the page sharp in its box (frames of 2870×1794 decoded by the graphics chip), its label and the keys under it. The user dragged around the misaligned card and wrote "this card is out of line", then dragged around the button and sent it without a note, then pressed Enter: two `region` events, each naming its element (`div[data-testid="second"]` with its 41 px left margin, `#b`), with a crop of the part and a picture of the whole view, and `done`.
- **A fresh agent:** `claude -p "Let's review this app's UI together in Pepper. Its dev server is running at http://localhost:5173."` in a Vite and React app (`npm create vite`) whose promo banner sat 18 px right of the header's title, with a `CLAUDE.md` of only the README's line and the viewer in a pseudo-terminal. A script played the human: once the agent waited for events, it clicked the banner and typed "align with the header", then pressed Enter. The agent ran `pepper-agent guide` first, then `status`, `web open`, `web shot` (and looked at it), `web errors`, `say`, and `events --wait 580 --until done`. From the point (`div.promo-banner`, its margin `24px 0px 24px 18px`, the note) it changed `margin: 24px 0 24px 18px` to `margin: 24px 0` in `src/App.css`. It then checked with `web eval` that the title, the stat cards, the banner and the table all start at 32 px, and looked at a new `web shot`. 38 s and $0.42 in all; the banner lined up.
