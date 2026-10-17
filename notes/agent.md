# pepper-agent: helpers that let AI agents use Pepper

As of 2026-10-16. A working note for `pepper-agent` (the spec: `~/Downloads/pepper-agent-spec.md`), which comes after the player's own optimizations (`notes/player-perf.md`). What was checked before designing, and where the spec and the code disagree.

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
- **Keys passed to a source** are a fixed list of mpv's: pause, seeks, frame steps, speed, volume, subtitle and audio tracks (mpv's progress and stats, `o` and `i`, were until Phase 3 made them ranges). Not passed: `q` (the viewer's), `m` (mark; mpv's mute), `l`/`L` (mpv's A-B loop; the agent's `loop` owns it), mpv's console and screenshots.
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
- **Frames at twice the CSS size, 30 a second**: headless Chrome's screencast is at the browser's own scale whatever the page's emulated one, so it runs with `--force-device-scale-factor=2`, sharp on a Retina display. An animation comes at 30 frames a second: making 2880×1800 JPEGs costs Chrome 151% of a core at 60 a second, 85% at 30 (below). A still page sends nothing. (Until Phase 4 this was Chrome's `everyNthFrame: 2`, which lost a page's last change half the time: see there.)
- **Points and regions on videos too**: with the media time, and pictures from the player's frame. The pictures (`snapshot`, the whole frame; `crop`, the part around) go in `.pepper/points/` of the viewer's project. After a click a note prompt opens: Enter sends the event with the note, Esc sends it without.
- **A click in a box is a point, not a click on the page**: the agent drives the page. The wheel scrolls it, `r` reloads, `↑` `↓` and Page Up/Down scroll. Clicking through to the page could come later as a mode.
- **What's at a point** is worked out in an isolated world (the page's scripts can't change what describes it): a selector that finds only it (an id, a test attribute, else a short path of classes and `:nth-of-type`), tag, classes, text, box, the styles that place it, its parent's selector and box; role and name from Chrome's accessibility tree. `source` comes from the app's dev build, in the page's own world and only on this machine's pages: React 17/18's `_debugSource`, the first app frame of React 19's `_debugStack` (its lines are those of the code the dev server sends), Vue's component file, Svelte's location, or an inspector plugin's `data-` attribute.
- **The title is the page's own `document.title`**: Chrome's target info gives the address until much later, if ever.
- **`open` takes a page too**: a localhost address or an `.html` file opens as a web source; `--kind web` for another address.
- **Device presets**: `desktop-1440` (the default, 1440×900 at 2), `desktop-1920`, `laptop-1280`, `ipad`, `iphone-15`, `pixel-8` (with their user agents and touch), or `WxH@DPR`.
- **Not here yet:** `schema` (spec section 3) came with the edit project in Phase 3, the first thing with a schema of its own.

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

## Phase 3: editing (2026-10-16)

A cut is a JSON project the agent writes (`"pepper_edit": 1`: an output frame and clips of files, each with an in and out and optional audio fades). `edit validate` lists every problem at once, `edit preview` shows the cut in the viewer with nothing rendered, `edit timeline` maps the cut's time to the files', and `edit render` makes it with ffmpeg. On a preview, the human's marks, notes and points give the clip, its file and the time in that file as well as the cut's time. `schema` prints JSON Schemas (the project, events, errors, `edit timeline`'s and `edit render`'s answers), and `y`/`x` send `approve`/`reject`. Code: `src/edit.rs` (the project, its checks, the timeline, the EDL, the render), the viewer keeps a preview's timeline with its source. Tests: 71 unit tests and 128 integration checks, 41 of them new (a hostile file name among a cut's clips, every problem of a broken project, marks, notes and approval on a preview, a preview again after a change, renders frame by frame, the seek bar and the wheel).

### Where it differs from the spec, and why

- **What a project may leave out:** a clip's `in` (its file's start) and `out` (its end); `output` (the first clip's size, made even, and its rate, NTSC's 29.97 as 30000/1001). `fit` is `pad` (the default, bars) or `crop` (filled, edges cut) where the spec says "scale or pad": stretching isn't offered. A clip may have a `name`, given back with marks as `clip_name`.
- **Strict:** a member version 1 doesn't have is a problem, not ignored, so a typo (`outptu`) never silently does nothing. Overlays and titles come as members of a later `pepper_edit`. At most 300 clips: a preview's timeline goes to the viewer in one request (64 KB at most).
- **`src` is from the project file's folder**, written back without `.` and `..` when that's the same file.
- **The preview is the cut, not the render:** an mpv EDL (`.pepper/edits/<name>.edl`, each path length-prefixed so nothing in a file name is read as EDL syntax) plays each file at its own size, rate and sound; the output's size and rate and the fades are in the render only, and `edit preview`'s answer says so. mpv joins the clips' tracks, so a clip without sound among clips with it plays silent, and a silent first clip doesn't take the sound away from the rest. Its times are exact: at the cut's time t in clip k, mpv shows the file's frame at inₖ + (t − startₖ) (checked with a clip whose frames show their own number).
- **Events on a preview** have `project` instead of `file`, and `clip` (the index in `clips`), `clip_name`, `src`, `src_time`; `time` stays the cut's, as `seek`, `loop` and `review` take it. `where` and `snapshot` say the same. A cut's own instant belongs to the clip after it. `edit preview` again (the same name) shows the new cut in the same box.
- **`y` and `x` now, on any source:** the spec gives them to comparisons and recordings (Phase 4), but editing's "render final after approval" needs them now. After `x`, `n` says why.
- **The render:** draft (the default) is VideoToolbox's H.264 or HEVC at `-q:v 60`; final is x264 `slow` CRF 18 or x265 `medium` CRF 20 (`--codec hevc`, tagged `hvc1`). Each clip is read from its in for its length (`-ss`/`-t` on the input: ffmpeg decodes from the keyframe before and drops to the exact frame), made of square pixels, fitted, brought to the output's rate from its first frame (`fps=…:start_time=0`), trimmed to its length and joined with `concat`. 8-bit 4:2:0 BT.709, AAC at 192 kb/s, 48 kHz stereo; a clip without sound gets silence so the joins line up, and with no sound anywhere there's none. HDR footage gets a warning: it isn't tone-mapped. Frame-exact at the cuts in draft and final (checked frame by frame).
- **Where it goes:** `.pepper/renders/<project>.draft.mp4` (or `<project>.mp4` for final) unless `-o`. Written to a hidden `.<name>.part<pid>` beside it and renamed when done; one left by a killed render is cleared next time. Never over the project or a clip's file (compared by device and inode, so a link doesn't get around it); a file that exists outside `.pepper/` only with `--overwrite`. The answer has the render's length as probed, the project's, and a contact sheet of the render to look at.
- **`schema`** covers the project, events, errors, and the answers of `edit timeline` and `edit render`, not every command's: the guide has an example of the rest.
- **Not here:** a command from a file's time to the cut's (the table of `edit timeline` gives it); video fades and transitions (not in version 1).

### The seek bar (asked for after the hand check)

The user asked how an editor lands on a given frame: the mouse only pointed (a click on the picture is a `point`), `,` and `.` weren't on the keys row, and the label gave whole seconds. So (`viewer/seekbar.rs`):

- **A bar on the row under every video:** the played part bold, the rest dim, a cyan playhead, an edit preview's cuts as ticks. A click or a drag on it seeks: to keyframes while dragging, to the exact frame where it's let go. On a 1080p H.264 bench clip a keyframe seek took 33 to 45 ms, the exact one 73 to 229 ms (with the distance from the keyframe before). Pages have no bar; a video's tile gives it a row.
- **The wheel over a video, or its bar, steps a frame a notch**, paused (mpv's `frame-step N seek`).
- **One seek at a time:** the next goes once the player says it's done (`playback-restart`, or after 1 s), so a drag sends only its latest place and a trackpad's burst of notches adds up into one step.
- **Paused, or while seeking, the label gives the time to the millisecond and the frame number**, and on a preview the clip and the time in its file. The frame number is the time by the file's `container-fps`: mpv doesn't report changes of `estimated-frame-number`. `where` and `sources` have it as `frame`, except on a preview, whose clips' rates may differ.
- **Clicks on the picture still point**, and seeking with the mouse sends no events: `m` is how the human says "here".
- **The human's marks, notes and points show on the bar** as magenta diamonds (asked for next: an editor must see that a mark was taken). `<` and `>` go to the one before or after the playhead, paused, and paused on one the keys row shows what was written there. On a preview they're placed by their file and file time, not the cut's time, so after the agent revises the cut each keeps its place in the footage, and one on a moment that was cut out disappears: the human sees their note was dealt with.

The integration test checks a click on the bar (the frame shown, read from a clip whose frames show their own number, is the one at that place), a drag (it lands where it's let go), and four quick notches, three on and one back (exactly two frames on, paused).

### With video-use, and ranges (2026-10-17)

The user asked for helpers around [video-use](https://github.com/browser-use/video-use) (MIT; a skill for coding agents: ElevenLabs Scribe transcripts, `takes_packed.md`, an `edit/edl.json` of ranges, `helpers/render.py` with grade, 30 ms fades, overlays, subtitles last and loudness; read at its commit `43cfc56`) rather than another editor. It has no human in its loop: its only check is the agent's own `timeline_view` PNGs, and the user is told to open `preview.mp4`. So pepper-agent reads its files as they are and adds the review:

- **`edit validate`, `preview`, `timeline` take its `edl.json`.** Each range becomes a clip of a pepper_edit project (paths as `render.py` resolves them: absolute, or from the EDL's folder; `beat` as the clip's name; `quote` and the source's name kept), checked by the same code; problems are said in its terms (`ranges[1].end`, `ranges[0].source`), every one at once. Its own members (`grade`, `overlays`, `subtitles`, `reason`, `total_duration_s`) are taken, not refused; a `total_duration_s` that isn't the ranges' sum is a warning. `edit render` refuses it, pointing at `render.py`: grade, overlays, subtitles and loudness are video-use's.
- **`open <render> --edit <edl.json>`** ties a render (`render.py`'s, or `edit render`'s) to its cut: marks on the real thing give the range and the time in its source. `render.py` lays the ranges end to end (per-segment extract, `-c copy` concat), so the timeline is the same sum; each segment is whole frames, so on many ranges of odd lengths the map can drift by up to half a frame a range. Checked with `render.py` itself (it needs only Python's standard library; `--preview --no-subtitles`, as Homebrew's ffmpeg has no libass): on a two-range EDL of a clip whose frames show their own number, all 65 frames of its output were the EDL's, `edit timeline` mapped every one to its source frame, and the viewer, seeking it opened with `--edit`, showed the frame it named, at the cut too. Its file starts 21 ms in (AAC's priming); mpv counts from its first frame, so that doesn't shift the map. A file whose length differs from the cut's by more than 0.1 s is a warning.
- **`said`**: on events, `where`, snapshots and `edit timeline --at`, from `edit/transcripts/<name>.json` (Scribe's word list, by the EDL's name for the source or the file's stem; also `edit/` beside plain footage): the phrase and word at the moment and the silences either side, so a new cut edge goes in a pause, never inside a word (video-use's rule 6). Phrases break as its `pack_transcripts.py` breaks them. `pepper-agent` reads the transcript when it answers; the viewer never does.
- **Ranges, `i` then `o`**: an editor's commonest note is "from here to there". The event has both ends (`time`, `to`, `length`; on a cut each end's clip, file and file time; `said` and `to_said`), a note like a point's, and shows as a span on the bar. `i` and `o` were mpv's stats and progress overlays, passed through until now.
- **Deleting a mark** (asked for next): paused on a mark, note, point or range (inside a range counts), `d` asks on the keys row and a second `d` deletes the newest there; any other key keeps it and isn't acted on. It leaves the bar, `<` `>`, and every `events` answer after it; the agent, which may have read it already, gets a `delete` event (`deletes`: the ids, `deleted`: what it was). `.pepper/events.jsonl` keeps both, as the log it is.
- **`status`** says whether video-use is installed (`VIDEO_USE_DIR`, or `video-use` in `~/.claude/skills`, `~/.codex/skills`, `~/.agents/skills`), and the guide has a section on it. The guide is now 234 lines, past the spec's ~200.
- **Not checked against its transcription:** video-use isn't installed here and no ElevenLabs key is set; the integration test builds its layout (an `edit/edl.json`, a Scribe-shaped transcript) and renders the same cut with `edit render`.

- **A fresh agent on a video-use edit** (`claude -p "video-use made a rough cut of my video: it's in edit/edl.json (transcripts are in edit/). Show it to me in Pepper and fix what I mark."`, `VIDEO_USE_DIR` set to the clone so `status` finds it; the folder held a talking-head clip, a Scribe-shaped transcript, `takes_packed.md`, `project.md`, and an EDL whose middle range kept a false start, "So, uh, let me, let me start again." at 4.6–7.0 s). A script played the human: on the preview it marked that stretch with `i`…`o` and "cut this false start", then Enter; on the next wait, `y` if no range kept it. The agent read the guide, the EDL, `project.md` and `takes_packed.md`, ran `edit validate` and `edit preview edit/edl.json`, `say`, and `events --wait 580 --until done`. From the range (`clip_name` BODY, `src_time` 4.6 to `to_src_time` 6.99) and the words around it, it moved BODY's start from 4.5 to 7.4 s, inside the 7.0–7.6 s silence, validated, previewed again and `review`ed the join; the human approved. It then rendered with video-use's `render.py`, looked at a `sheet` of the render, opened it with `--edit`, and offered to cut an "um" no one had marked. 83 s and $0.57.

Tests: 74 unit tests and 155 integration checks (22 more: deleting marks, the EDL read and checked in its terms, `said` with the word and pauses, a transcript's escapes made inert, a mark on its preview and on its render, a range with both ends, `status` finding it).

### Measured (M5 Pro)

A 30 s cut at 1080p30 of three bench clips (1080p H.264, 1080p HEVC, 2160p HEVC, all with sound; fades on two), release build:

| What | Time | CPU |
| --- | --- | --- |
| `edit validate`, first time (three probes) | 0.48 s | |
| again (probes cached), `edit timeline` | under 10 ms | |
| `edit preview`, to its first frame | 0.39 s; 0.12 s for a new cut in the same box | |
| `seek` into the second clip, `where` | 80 ms, 7 ms | |
| `edit render`, draft (VideoToolbox H.264) | 3.9 s, 7.7× real time | 5.9 s (sheet included) |
| draft, `--codec hevc` | 4.0 s | 6.1 s |
| final (x264 slow, CRF 18) | 4.5 s, 6.7× | 47.6 s, about 8 cores |
| final, `--codec hevc` (x265 medium, CRF 20) | 10.9 s, 2.7× | 92 s, about 8 cores |

On this Mac a draft isn't much quicker than final x264 on the clock (decoding the 4K HEVC clip takes much of it), but it costs an eighth of the CPU, so the Mac stays free while an agent renders again and again. A render's answer includes its contact sheet (about 0.7 s of the above).

pepper-play didn't change in this phase, so the Phase 0 bench wasn't run again.

### Checked

- **By hand, in a test Pepper:** the acceptance's 13 s cut previewed in the viewer, with a line from "the agent". The user played it, marked once in the demo and approved with `y` at three places, then pressed Enter. Each event gave the right clip and file time: the mark at the cut's 0:05.333 was demo.mp4 at 0:09.333 (the demo starts at 0:04, from its 0:08), `y` at the very end was outro.mp4 at 0:02, at 0:00 intro.mp4 at 0:02.
- **A fresh agent** (the spec's acceptance: a 3-clip rough cut, two fixes the human marked on the preview): `claude -p "Let's make a rough cut together in Pepper from the clips in raw/: 4 seconds of intro.mp4 from 0:02, then demo.mp4 from 0:05 to 0:15, then the first 3 seconds of outro.mp4. Save the edit as cut.json in this folder, show it to me, and fix what I mark."` in a folder holding the three clips and a `CLAUDE.md` of only the README's line, the viewer in a pseudo-terminal. A script played the human: once the agent waited for events, it moved the preview to demo.mp4's 0:08 and pressed `m`, then `n` "start the demo here, cut what's before"; then to outro.mp4's 0:02, `m`, `n` "end the video here"; then Enter. On the agent's next wait it pressed `y` if both fixes were in the project. The agent ran `pepper-agent guide` first, then `status`, `probe` and `sheet` of each file (and looked at the sheets), wrote `cut.json`, `edit validate` and `edit timeline`, `edit preview` and `say`, and `events --wait 580 --until done`. From the marks' `clip` and `src_time` it set the demo's `in` to `0:08` and the outro's `out` to `0:02`, validated, previewed again, `review`ed 0:02 to 0:13 and asked again; the human approved. 70 s and $0.49; it offered to render rather than rendering unasked. An earlier run, by mistake with the guide from before this phase, got there too (53 s, $0.56), finding the format through `schema edit` and `--help`.

## Phase 4: recordings, before and after, several agents (2026-10-17)

An agent records what a source shows (`record start`/`stop`), or a page while a script of steps drives it (`record run`), and shows the file to the human with `play`, for `y` or `x`. `compare` puts two sources side by side as before and after, two videos locked together with `--sync`, two pictures' difference on `d`. Several agents share a viewer by sessions (`session new`): what each opens is its own, labelled with its name. Code: `src/recordings.rs` (the commands' side: the file into `.pepper/recordings/`, scripts, `compare`, `play`), `viewer/record.rs`, `viewer/compare.rs`, `viewer/sharing.rs`. Tests: 76 unit tests and 204 integration checks, 49 of them new (below).

### Where it differs from the spec, and why

- **A page's recording** is its screencast, the latest frame every 1/30 s from the first (Chrome sends a frame only when the page changes, so a still page is a still picture, not a gap), to an ffmpeg the viewer runs, H.264 by VideoToolbox, into the viewer's own folder; `record stop` moves it into `.pepper/recordings/<name>-<time>.mp4` and answers with its length, size and a contact sheet. While it records, the screencast runs at the page's own size (at most 1920 px on the long side), not its box's, which in a grid is a quarter of the pane: 1920×1200 for the default 1440×900 at 2.
- **A video's recording** is the stretch between where it was at `record start` and where it is at `record stop` (play it or `review` it in between), cut from the file itself with `edit render`'s code (a draft): never a capture of the screen. A preview's is rendered from its cut (the clips it spans, trimmed); a video's web address can't be cut (`UNSUPPORTED`). Devices and streams come with Phases 5 and 6.
- **`--max`** (120 s unless given, 600 at most) ends a recording by itself; `record stop` then answers with it.
- **`record run`** takes 4.2's verbs (`click`, `hover`, `type`, `press`, `scroll`, `goto`, `reload`, `back`, `wait`, `wait_for`, `viewport`) and `say`; device verbs come with Phase 6. A script is checked whole before anything runs, every problem at once as a project's are: 200 steps at most, a `wait` of 30 s at most. 0.6 s of the page still at each end. A step that fails ends it, the recording up to there kept (`completed: false`, `failed`: the step and its error).
- **`play <file>`** opens it as a source of kind `recording`, playing. `y` and `x` work on it as on any source (Phase 3).
- **`compare`** takes two sources' names, or files and pages, which it opens as `before` and `after`; they're side by side whatever the layout, labelled `[before]` and `[after]`. With `--sync` the focused one leads: the other follows its pause and its seeks (when the seek has landed), and while both play, a drift over 80 ms is corrected, at most every 0.5 s. On two pictures, `d` shows their difference in the after one's place (ffmpeg's `blend=difference`, four times as bright, so small changes show; black where they're the same). `y` and `x` on either carry `compare`, both names.
- **Sessions**: `session new` gives a 128-bit random token; a source opened with it is that session's, labelled with its owner's name (`--owner`, or `agent-N`). Another session, or none, can't drive, record or close it (`NOT_OWNER`) unless it says `--force`; looking (`snapshot`, `where`, `sources`, `events`, `web shot`, `console`, `errors`, `dom`, `a11y`) is open to all. Without `--source`, a session's command goes to its only source of the kind. The token begins with its viewer's id, so a session's commands go to that viewer when a newer one runs too (without `--viewer`, commands go to the newest). A token the viewer didn't give is refused, but for `session new` and `status`; a viewer started again forgets them. `say` shows whose line it is. `sources --owner` and `events --owner` give one agent's (`events --source`, the spec's way, works too). It keeps agents from stepping on each other by mistake, not out: they're the same user's programs, as the socket's peers are.
- **Tiling** was there since Phase 1 (`auto` and `grid` tile four; a fifth source is `LIMIT_REACHED`).
- **`--session` on every command**, or `PEPPER_AGENT_SESSION`: Claude Code's shell keeps no variables between calls, so the guide says to pass it each time. The subagents in the acceptance did.

### A bug in the screencast, found by the acceptance

In the first acceptance run, three of the sign-up subagent's four recordings stayed on the filled-in form and never showed the welcome that followed; it worked around it by opening the page again. Not a recording bug: the screencast never sent the welcome. Phase 2 had asked Chrome for every second frame (`everyNthFrame: 2`), and Chrome counts captures and drops every other one, whatever comes after: when a page goes still on a dropped one, its last change never arrives, live in the viewer too, and a still page's only frame can be the one dropped. Over a raw CDP pipe, Chrome 156:

| Screencast | Frames from a still page at start | 10 clicks | Started again on a still page |
| --- | --- | --- | --- |
| `everyNthFrame: 2` (Phase 2) | 0 | 5 | 0 |
| every frame, one out at a time | 1 | 10 | 1 |

Now: every frame, and from Chrome 154 (`maxFramesInFlight`) one out at a time, acked no sooner than 1/30 s after the last ack, so Chrome drops what it captures meanwhile before copying it. And once a page has sent nothing for 150 ms after a frame, its screencast starts again, which sends what it shows: the frame dropped last, if any, can't stay missing. One more frame each time a page goes still. Chrome's `sendLastFrame` (also 154) would keep the latest instead, but it copies every frame Chrome captures. A page animating all the time, in a box of about 3000×1900 pixels:

| Screencast | Frames a second | Chrome |
| --- | --- | --- |
| `everyNthFrame: 2` (Phase 2) | 30.0 | 81% of a core |
| one out, acks 1/30 s apart, started again when still (used) | 28.2 | 79% |
| `sendLastFrame`, acks 1/30 s apart | 27.3 | 136% |

An older Chrome gets every frame, acked at once: right, but an animation then costs what 60 a second does. The integration test now records a page as it turns black and checks the recording ends black.

### Measured (M5 Pro, release build)

| What | Result |
| --- | --- |
| A page animating, recorded at 1920×1200 | Chrome 41% of a core (32% not recording), ffmpeg 7%, the viewer 0.4% |
| `record stop` of 5 s of a page at 1920×1200 (ffmpeg finishing, the move, a sheet) | 1.36 to 1.44 s |
| `record stop` of 2 s of a video (a draft render from the file, a sheet) | 0.81 to 0.87 s |

### Checked

- **Integration test**: sessions (`NOT_OWNER` for driving, pausing and closing another's source, `--force`, a token the viewer didn't give, a session's command finding its own source, `sources` and `events --owner`, the owner on events and labels), a video's recording (the stretch that played, cut from the file, with its sheet), a preview's (rendered from its cut), `record run` on one of two agents' pages in a grid (every step, the page's own size, a bad script's every problem, a failing step, `--max`), the recording ending on the page's last state, `play` and `y` on it, `compare --sync` (a seek, playing and pausing follow the leader; a verdict names both), and two pictures' difference with `d` and back.
- **A fresh agent** (the spec's acceptance: two subagents each drive their own web source in a grid, and one hands in a recorded demo that the human approves with `y`): `claude -p "Two pages of my app need checking, at the same time, in Pepper: the sign-up form at <url> and the cart at <url>. Use two subagents in parallel, one per page: each opens its page and checks it works by using it. The sign-up one should also record a short demo of signing up and hand it to me to approve."` in a folder with a `CLAUDE.md` of only the README's line, the viewer in a pseudo-terminal, both pages served on this machine. A script played the human: when an agent waited for events, a recording on show got `y`, then Enter. The main agent read the guide and briefed two subagents to take a session each (`session new --owner signup-agent`, `cart-agent`) and pass `--session` on every command, as the guide says. Both opened their page in the viewer at once (`auto` tiled them with the recording), drove only their own and checked it with `web eval`, `web dom` and `web shot`; the sign-up one wrote a script, ran `record run`, looked at the recording's sheet, showed it with `play` and waited; the human approved it with `y`. The first run, before the screencast fix, found it (above): 4 recordings, 3 of them frozen, $2.20. After the fix, twice: one recording, its sheet showing the empty form, the form filled in, then the welcome, approved; 3.6 min and $1.71, then 2.9 min and $1.65. In the second, a viewer started for a measurement during the run became the newest, and the subagent's session was refused there until it passed `--viewer`: since then a session's commands go to the viewer that gave it.

## Phase 5: live output and streams (2026-10-17)

A source can be live: a file shown again each time it's rendered again (`watch`), a FIFO whose frames show as a program writes them (`pipe new`), or a stream (`open rtsp://…`) that reconnects by itself, its password given as an environment variable (`--credentials-env`) and never shown. Code: `viewer/live.rs` (when each loads again, and how), `text.rs` (passwords taken out of any text, put into an address), the commands in `main.rs`, and `tests/rtsp_server.py`, a test camera. Tests: 80 unit tests and 233 integration checks, 29 of them new (below).

### Where it differs from the spec, and why

- **`watch`** follows the file's folder with FSEvents (a render may write a new file and rename it over) and loads the file again once its size, time and inode haven't changed for 250 ms, or 2 s after it began changing if it keeps changing: never half a render, and one that failed to load (half written) is shown once whole. Videos and GIFs loop (`loop-file=inf`), pictures stay, a paused video stays where it was. SVG and HTML open in the viewer's browser, reloaded when their folder changes (`web open --watch`): mpv shows no SVG. `where` says `reloads` and `reloaded_at`; the human is told on the bottom row when a file is shown again after 10 s or more without (a file rendered every 2 s isn't announced every 2 s).
- **`pipe new`** makes the FIFO in the viewer's own folder (0700), only the user's (0600), gone with its source; y4m (the default), MJPEG, or raw BGRA of a given size and rate. mpv reads it through libavformat (`ffmpeg://file:<fifo>`): its own file reading probes a FIFO as if it could seek and stalls (`Trying demuxers` for ever, found by testing). Frames show as they come (`untimed`, mpv's `low-latency` profile, no cache). At the writer's end the last frame stays and the player loads the pipe again, which waits for the next writer (`status` waiting, `writers`). Closing one while its player waits in `open()` for a writer, the viewer opens and closes the FIFO for writing, which lets it go. A pipe can't be recorded yet (`UNSUPPORTED`, with a hint): mpv's recorder, started in the middle, writes raw frames with the wrong pixel format (`rgb555le`, no codec tag).
- **Streams**: rtsp(s), rtmp(s), srt, udp, and HLS (an http(s) `.m3u8`, a localhost one too, which opened as a page before). mpv's `low-latency` profile (but for HLS, whose segments are seconds long), with the streams' setup read after all (`demuxer-lavf-probe-info=yes`, half a second at most: without it mpv's recorder can't write a header), and a network timeout of 10 s. One that ends or fails is tried again after 1 s, then 2, 4, 8, 10 (`status` reconnecting, `reconnects`, `last_error`, `retry_in`); a first `open` that can't connect answers `LOAD_FAILED` and keeps trying. `snapshot` is mpv's frame, what the human sees, rather than a grab of ffmpeg's own (the spec's): one connection to the camera, not two. `record start`/`stop` is mpv's `stream-record`: the packets as they came, from the next keyframe, kept in Matroska and copied into an MP4. srt:// needs an ffmpeg with libsrt, which Homebrew's isn't: here it fails with mpv's message.
- **Passwords** are taken out (`***`) of everything the viewer and the commands show or write: answers, labels and names, events, the viewer's record, every line of its log, mpv's and ffmpeg's messages (as they're read), errors, and anything after an HTTP `Authorization:`. `--credentials-env VAR` reads `user:password` from the environment and percent-encodes it into the address (ffmpeg decodes it: checked); one in the address works too. The viewer gives the address to pepper-play over its IPC, so it's on no command line.

### Bugs found on the way

- At a stream's end mpv (`keep-open`) pauses. The viewer unpaused it and then loaded the stream again, and 1 time in 3 mpv paused again before the load took over: the stream came back paused and its recording empty. It's unpaused after the load now (4 runs out of 4 since).
- The y4m pipe that stalled in mpv's probing (above).

### Measured (M5 Pro, release build)

| What | Result |
| --- | --- |
| A watched PNG renamed over, to shown again | 0.30 to 0.31 s (FSEvents' 0.1 s and the 250 ms of settling) |
| An RTSP stream (640×360 H.264 on this machine, behind a password), `open` to its first frame | 0.68 to 0.70 s |
| A pipe at 1080p30, y4m | pepper-play 14% of a core |

### Checked

- **Integration test**: a watched picture shown again after a render in place and one renamed over, and not when nothing changed; a watched video looping; `pipe new`'s FIFO (0600, in the viewer's folder) waiting, a writer's y4m frames, a snapshot of it, recording refused, waiting again at its end, a second writer at another size, raw BGRA frames from Python, a waiting pipe closed at once with its FIFO gone; an RTSP stream behind Basic authentication (`tests/rtsp_server.py`) from `--credentials-env`, its address shown with `***`, the password reaching the camera, a snapshot, a mark, the camera dropping it and the stream coming back, its recording, wrong credentials (`LOAD_FAILED`, tried again), a password in the address; the password in no answer, event, record, log, on the screen or on a command line.
- **The README's examples**: its Python and Rust programs writing y4m into a pipe, each run against a viewer: 320×240, playing, and the pipe waiting again after.
- **A fresh agent** (the spec's acceptance: a watch on a re-rendered PNG updates live, a Python script streams y4m frames through a pipe, and an RTSP test stream plays with its credentials redacted in all output): `claude -p "Show me three live things in Pepper at once: plot.png as render.py keeps re-rendering it (start render.py; it runs until stopped), a small animation of your own that a Python script draws frame by frame (a bouncing ball is fine), and my camera at rtsp://127.0.0.1:<port>/live, whose login is in the CAM environment variable. Tell me when all three are up."` in a folder holding `render.py` (a sine wave re-rendered every 2 s, written aside and renamed) and a `CLAUDE.md` of only the README's line; the viewer in a pseudo-terminal, `CAM` set for the agent only, the test camera behind that login. The agent read the guide, started `render.py` in the background and `watch`ed `plot.png`, made a pipe and wrote a bouncing ball into it as y4m from a Python script of its own (standard library only), opened the camera with `--credentials-env CAM`, and looked at a snapshot of each before saying all three were up. 63 s and $0.48. `plot` was shown again 294 times while it ran; `ball` played, then waited when its script was stopped; the camera played with no reconnect. The password was in none of the agent's transcript, the screen, the command lines sampled every 2 s, or the viewer's record and log.

## Phase 7: extras (2026-10-17)

Phase 6 (devices) is skipped for now: this Mac has no Android SDK, emulator or adb, and only the iOS simulator's runtime. It comes back once they're installed.

Three commands on a file, `scenes`, `silences` and `transcribe` (`src/analyze.rs`), and two other ways in for agents: `install-skill` (`src/skill.rs`, `pepper-agent/skills/pepper/SKILL.md`) and `mcp` (`src/mcp.rs`). `status` lists whisper.cpp and its model under `deps`. Tests: 91 unit tests and 270 integration checks, 37 of them new (below).

### Where it differs from the spec, and why

- **`scenes`** is ffmpeg's `select` scene score (0 to 1, the spec's `--threshold 0.3`), on a 320-pixel copy, decoded by VideoToolbox. It answers with the scenes between the cuts (start, end, how sure each cut is) and the `frames --at` that shows their first frames.
- **`silences`** is ffmpeg's `silencedetect` (`--db -35 --min 0.5`). It also gives `sound`, the stretches between: what an agent makes into clips to cut the quiet out.
- **`transcribe`**: the spec names no engine. It's whisper.cpp (`brew install whisper.cpp`), on this Mac, nothing sent anywhere, with a model the human downloads into `~/.cache/pepper-agent/whisper/` (or `--model`, `PEPPER_WHISPER_MODEL`): the error says which and how big, and the guide tells the agent to ask first. The answer has the segments; a JSON file has them and every word (in Scribe's shape, so `said` reads it: the human's marks on a transcribed file, or on a cut made of it, say what was said there), with an `.srt` beside it.
  - Word times: whisper's token times are rough. In a test, "transcription", said at 2.0 to 2.7 s, came out at 4.12 s, inside the 1.7 s pause after it. Its VAD mode (`--vad`) times the words after a pause in the speech-only timeline, wrong by the pause's length. Its alignment (`-dtw`, the model's alignment heads; flash attention off) puts each word's end within about 0.05 s of the sound. So a word ends there, and starts where the one before ended or where a pause between them ends (ffmpeg's `silencedetect`, 0.12 s); words aligned to one time share it by their length. Without a known model name for the alignment, the token times are kept, cut at pauses.
- **The skill** is at `pepper-agent/skills/pepper/SKILL.md`, not `skill/SKILL.md`: the Agent Skills format names a skill after its folder. Its body is guide.md, kept the same by a test (`PEPPER_AGENT_BLESS=1 cargo test skill` writes it again). Its description leaves out simulators and emulators until Phase 6. `install-skill` writes `<dir>/pepper/SKILL.md` (`~/.claude/skills` by default); it updates its own, and replaces another skill called pepper only with `--force`.
- **`mcp`**: a thin wrapper, as the spec asks. Each call runs pepper-agent with the arguments as a command line, and answers with its JSON and the PNGs it names, as images. The tools' arguments are named from the usage lines (`open`'s `file`, `name`, `play`), with `viewer`, `session` and `force` on every one. It keeps no state: one server can serve several subagents, so a session's token is an argument. `view`, `mcp` and `install-skill` aren't tools. Protocol versions 2024-11-05 to 2025-11-25.

### A bug found on the way

`where`, `snapshot` and `events` look for a transcript of a source's file. For a pipe, that's its FIFO: taking its fingerprint opened it, which waited for a writer (the command hung) and would have taken frames from mpv. Phase 5's integration checks found it. Only a plain file is looked at now.

### Measured (M5 Pro, release build)

| What | Result |
| --- | --- |
| 60 s of 1080p30 H.264 with speech: `scenes` | 3.2 s, once (0.02 s from the cache) |
| The same: `silences` | 0.06 s |
| The same: `transcribe` (ggml-base, on the graphics chip) | 1.4 s: 168 words, none inside a pause |

### Checked

- **Integration test**: on a clip of three shots and two sentences spoken by macOS's `say` with 1.5 s of quiet between, the two cuts, the pause and the sound around it, bad arguments, sound-only and silent files. Also the transcript's words (none in the pause, the word before it ending before it), its .srt, both kept in the cache, a mark on the file saying what was said, a missing model (`NOT_FOUND`, `MISSING_DEP` with how to get one), and `install-skill` (written, nothing to do again, another skill left alone). `mcp` as a client drives it: initialize, every command as a tool, the command's JSON, a snapshot as an image, errors, an unknown argument and method, a ping while a call waits, a cancelled call stopped and not answered, the server ending with its client.
- **Two fresh agents**, each with a viewer in a pseudo-terminal (the spec has no acceptance for this phase). The talk was 12 s: three shots, two sentences, 3.2 s of quiet between.
  - **The skill alone.** A folder with no CLAUDE.md, only `install-skill --dir .claude/skills`. Prompt: *"In talk.mp4 I say two sentences with an awkward long pause between them. Cut the pause out (keep everything I say, and don't cut into a word), show me the result in Pepper so I can approve it, and once I approve, render it to talk-cut.mp4."*
    - The agent loaded the skill and ran `transcribe` and `silences`. It wrote a two-clip project cutting at 2.85 and 5.77 s, inside the pause (2.71 to 5.92), 0.15 s from the words.
    - It previewed it, waited for the human's `y`, and rendered it final. Then it transcribed the render to check it: all 21 words, 0.4 s between the sentences.
    - Another viewer ran at the same time; the agent picked its own by its project. 68 s and $0.48.
  - **MCP alone.** No shell (Bash not allowed), only `--mcp-config` with `pepper-agent mcp`. Prompt: *"Look at clip.mp4 for me: where do its shots change, and what is said in it? Then show it to me in Pepper, paused at the start of the second shot, and wait until I've looked."*
    - The agent called `guide` and `status`, then `probe`, `scenes`, `transcribe` and `sheet`. It `open`ed the clip, `seek`ed to 4.0 and looked at a `snapshot` (an image) to be sure it was the second shot.
    - It told the human with `say` and waited (`events`). Then it gave the shots (0, 4 and 8 s) and the words, with the pause across the first cut. 37 s and $0.45.

## Security review before the first release (2026-10-17)

Everything since 1.3.1-pepper.2 (pepper-agent, and `pepper-build.sh`'s lines for it) was reviewed in four parts: the socket, CLI and MCP server; media, editing and the programs run on files; pages; and what reaches the terminal and the files written. Each finding was then checked on its own. One passed the review's bar; three more were real but scored under it, and two were noted on the way. All are fixed, each with a check in the integration test.

- **Links in a project's `.pepper/`** (the one over the bar). A repository or an archive could ship `.pepper/events.jsonl` as a link. The viewer appended each event through it, to any file the user can write, with text a file name or a page chose: a `$(…)` in a clip's name ran when a shell read its startup file. Now (`src/files.rs`):
  - every folder under `.pepper/` must be the user's own and not a link (refused otherwise; made 0700 if others could open it);
  - a line is added only to a plain file (`O_NOFOLLOW`), and the events go to the viewer's own folder if `events.jsonl` is anything else;
  - files are written to a new name and moved over, and the name a program (ffmpeg, whisper.cpp) writes is cleared first;
  - a cached answer is read only from a plain file.
- **A stream's password in Now Playing.** The viewer's pepper-play published mpv's `media-title`. For a stream with no title, that's the address's last part: `user:password@host` for `rtsp://cam:554`. The viewer's players now run with `--input-media-keys=no` (no Now Playing, no media keys). Also, pepper-play's messages are redacted before they're cut to 400 characters: a cut inside a long password left it unmarked.
- **`--human` printed keys raw.** A page's own object (from `web eval`, or a point's source, which a dev build's metadata gives) could carry an escape sequence in a key's name. Keys are made inert now, and a point's source is made again in Rust, as a file name and numbers.
- **Link-local addresses.** Chrome's proxy bypass always adds 169.254/16 and fe80::/10, so a page without `--allow-remote` could reach them on the network. The bypass list starts with `<-loopback>` now (rules are read last to first), and names 127.0.0.0/8 itself.
- **`-o .pepper/../x.mp4`** counted as inside `.pepper/`, so an existing file could be replaced without `--overwrite`. The output's folder is resolved first.

The integration test has a `links` group for the first: a viewer of its own in a project whose `.pepper/` holds links (the events file, the snapshots folder, a cached frame and the name it's made under). Before the fixes, the new checks failed; after, they pass.
