# tuitube: video inside the terminal

As of 2026-10-13. Not committed: a working note. Measured on the Mac (Apple M5 Pro, 18 cores), Ghostty 1.3.1, mpv 0.41.0, a 1080p30 H.264 test file. The scripts used are in `notes/inline-video/`; run them in Ghostty, not tmux. `bench.py` plays `test.mp4` beside it (or `TT_BENCH_VIDEO`), made with `ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 20 -c:v libx264 -preset veryfast -pix_fmt yuv420p -b:v 6M test.mp4`.

Picked on 2026-10-13: the shared-buffer route (3 below) in a Ghostty build of our own, for this Mac only for now, aiming for the least the machine can spend.

## What exists

- **mpv `--vo=kitty`** (since 0.36) draws video with the kitty graphics protocol, in a box of cells: `--vo-kitty-left/top/cols/rows/width/height`, `--vo-kitty-alt-screen=no`, `--vo-kitty-config-clear=no`. It writes to stdout even with `--terminal=no`, so it works into a pipe once given a size. `--vo-kitty-use-shm=yes` sends frames through shared memory, and only their name goes through the terminal.
- **mpv 0.41.0's shared memory shows nothing on macOS**: it sends the name without its leading `/` (`mpv-kitty-0x…`), macOS's `shm_open` refuses it, and `q=2` hides the error. Fixed in mpv master `e5a74aaaa` (2026-09-03), not released yet; kitty 0.49 shows a black screen with mpv 0.41 for the same reason. A relay that adds the `/` works in Ghostty (`inline-video/kitty_relay.py`).
- **What else mpv's kitty output does to the screen**, even with alt-screen and config-clear off: turns on all-motion mouse reporting (`\e[?1003h`), hides the cursor, deletes every image on screen (`\e_Ga=d`, so tuitube's thumbnails) at start and at exit, moves the cursor at exit, sends each frame as a new image without an id, marks shared-memory frames `m=1` with no `m=0`, and (0.41) leaves its last delete unterminated.
- **Others**: timg and notcurses (`ncplayer`) play video with the kitty protocol but want the whole terminal; vid (jhickner/vid) does too, with software decoding only; ratatui-image draws still images; kitty-graphics.el (Emacs) puts mpv's kitty output inside the editor; bunker-go hands the whole terminal to mpv while a video plays.
- **Ghostty**: its maintainers say its kitty parser is too slow for video ("fine for viewing static images, but for videos it is simply inadequate") and recommend `--vo-kitty-use-shm=yes --profile=sw-fast --really-quiet`. PR #14379 (open, waiting until after 1.4) still drops frames against kitty, and says shared memory broke on Ghostty's development branch after the spec change about shared-memory paths.
- **Ghostty has no plugin system** (1.3.1). AppleScript (1.3) controls windows, tabs, splits and input but draws nothing; custom shaders only change Ghostty's own screen image.

## Measured

Each run 10–15 s of the 1080p test file at 30 fps, with tuitube's `--hwdec=auto-safe`. CPU is a share of one core.

Whole pipeline, in Ghostty (`inline-video/bench.py`):

| How it played | mpv | Ghostty (above idle 9%) | Frames/s | Ghostty memory |
| --- | --- | --- | --- | --- |
| mpv's own window, 4806 px wide (`videotoolbox`, no copies) | 16% | ≈0 | – | – |
| kitty, small box 60×17 cells (1080×782 px, `videotoolbox-copy`) | 64% | +11% | 29 | +670 MB while playing |
| kitty, large box (4806×2162 px) | 98% | +15% | 21 (drops) | +790 MB while playing |

Ghostty's memory went back down afterwards (387 MB). WindowServer was 50% at idle, too noisy to compare.

Only the part that makes the frames (no terminal), to a pipe:

| Frames | CPU |
| --- | --- |
| mpv kitty, 1080×782 | 66% |
| mpv kitty, 540×391 | 58% |
| mpv kitty, 540×391, 15 fps | 33% |
| Software decode, CPU scale to 540×304 RGBA (ffmpeg) | 27% |
| **VideoToolbox decode and scale (`scale_vt`) to 540×304, then copy out** | **9.5%** |
| Same, 15 fps | 7% |

mpv's kitty output barely gets cheaper when smaller: it copies the whole 1080p frame off the graphics chip and scales it on the CPU. Scaling on the graphics chip first and copying out only the small frame is about 6× cheaper:

```sh
ffmpeg -re -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld -i in.mp4 \
  -vf scale_vt=540:304,hwdownload,format=nv12,format=rgba -f rawvideo -
```

So vid's claim that VideoToolbox's readback costs more than it saves holds only for a full-size readback.

## Why the kitty protocol can't beat mpv's window

The kitty protocol can't take a picture that's already on the graphics chip: every frame goes graphics chip → memory → terminal → graphics chip. mpv's window never leaves the graphics chip. Ghostty alone spent +11% receiving the small box, against 16% for the whole window at 4.5× the width. Best kitty estimate for a small preview: ≈20% (9.5% frames + ≈10% Ghostty), unmeasured end to end.

## Routes

| # | Route | Cost | Time | Catch |
| --- | --- | --- | --- | --- |
| 1 | Relay mpv's kitty output (fix the name, drop its mouse/cursor/delete codes, add an image id, let Ghostty scale with `c=`/`r=`) | ≈75% small box | 2–3 days | Not window level |
| 2 | Own frame helper: VideoToolbox decode + `scale_vt` → shared memory → kitty; mpv plays the sound and stays the clock | ≈20% (estimate) | 2–3 days | Not window level |
| 3 | **Shared graphics-chip buffers, terminal draws them** (chosen) | ≈ mpv's window | 1.5–2 weeks | Needs a Ghostty build of our own |
| 4 | Terminal decodes compressed video it's sent (works over SSH) | ≈ mpv's window | 3–5 weeks, terminal side | Untrusted video decoded in the terminal |
| 5 | mpv's own borderless window kept over the box | ≈ mpv's window | 2–4 days | macOS can't attach it to Ghostty's window: moves, Spaces, overlaps |

Precedent for 3: porthole (flotilla-org/porthole #80): IOSurfaces handed over once by XPC, then a ring slot and an `MTLSharedEvent` fence value per frame; aimed at a libghostty-vt fork; core done 2026-10-02, cost measurement still open; a protocol other terminals could take up is out of its scope.

## Plan for route 3

**Ghostty patch** (a branch built locally; mostly in its Swift app code, a small Zig part only if the escape sequence needs one):
1. Ghostty opens a named Mach port, per process, the way Chromium hands IOSurfaces between its processes (`bootstrap_check_in`). Whether a plain app can still do that is the first thing to test.
2. tuitube prints a short escape sequence with a random token and the cell rectangle, so Ghostty knows which tab and box; only a program that can write to that tab and knows the token can place video there.
3. Ghostty puts a layer over that box and shows each IOSurface it gets (`layer.contents`), composited by macOS with no copies. Later, if worth it: `AVSampleBufferDisplayLayer` with VideoToolbox's NV12 buffers, as Safari shows video.

**Video helper** (new, in tuitube): libmpv (already installed with mpv), so sound, sync, subtitles, chapters, SponsorBlock seeks, the visualizer filter and the same `input-ipc-client=fd://3` control stay, and `player.rs` barely changes. It renders through the render API (OpenGL FBO) into a ring of 3 IOSurface-backed textures, sends them once, then "slot k ready" per frame (after a fence or `glFinish`). Risk: mpv's Now Playing and media keys may not work as a library; tuitube might do them itself (`MPNowPlayingInfoCenter`).

**Rough time**: setup 1 h; Ghostty showing one IOSurface from a test program ½–1 day; the helper 2–3 days; tuitube (setting, hidden under popups, resizes) 2–3 days; then measure against mpv's window with `bench.py`. Plus moving the patch onto each Ghostty release.

**Security**: the helper decodes YouTube's bytes like mpv, so it runs the same way (own process, cleaned environment, nothing private on its command line, control over the socketpair only). Ghostty's port shows pixels only; tie each stream to its tab by the token.

**Missing on this Mac to build Ghostty**: full Xcode with the macOS SDK, iOS SDK and Metal Toolchain components (`xcode-select` points at the Command Line Tools now); Zig 0.15.2 exactly (Ghostty 1.3.x and tip); `brew install gettext`. Build: `zig build -Doptimize=ReleaseFast` → `zig-out/Ghostty.app`.

## Spike (2026-10-13)

pepper (`macos/Sources/Features/Pepper/PepperServer.swift`) shows 3 IOSurfaces of 640×360 BGRA, sent once over its Mach port by `inline-video/pepper_play.c`, in a 480×270-point layer over the front tab, switching surface on each "present" message.

| | pepper CPU |
| --- | --- |
| Idle | 4.2% |
| Showing 30 fps | 4.6% (+0.4%) |

All 300 frames arrived on time. Only 1 was drawn into a surface the screen was still using, so a ring of 3 looks enough. Against that, Ghostty spent +11% just receiving mpv's kitty output in a small box.

With the escape sequence and token (step 4), in a pepper tab, 20 s at 30 fps while resizing the window: 600 frames, 0 ms late. The box follows the cell grid as the window resizes. With `-k` (exit with no detach), pepper clears the frame by itself.

9 of the 600 frames (1.5%) went into a surface still in use (`IOSurfaceIsInUse`), more than the 1 in 300 earlier, possibly because of the resizing. Real video would tear there. So the helper should skip to the next free slot, or wait, rather than write blindly in turn. pepper accepts up to 4 surfaces.

## Helper (2026-10-13)

`tuitube-video` (tuitube's `video/` crate): libmpv's render API draws with OpenGL (a CGL context with no window) into 4 IOSurface-backed framebuffers. It always picks a surface the window server isn't showing (`IOSurfaceIsInUse`) and sends pepper "present slot k" after `glFinish`. It takes tuitube's mpv options and IPC on fd 3 unchanged, and gets the token and the box's pixel size as `script-message-to main pepper-token|pepper-size`. Driven by `inline-video/helper_try.py`.

The 1080p30 H.264 test file with sound, release build, played into a 139×31-cell box (2502×1426 px) in a pepper tab:

| | CPU |
| --- | --- |
| helper (decode with `videotoolbox`, scale, draw) | 7.4% |
| pepper (its idle was 3–4%) | 3.2% |

445 frames in 15 s, 1 drawn into a surface still on screen (all 3 others were showing). The picture is upright with `FLIP_Y` 0. mpv's own window was 16% at 4806 px wide, mpv's kitty output in a large box 98% + 15% in Ghostty. Not yet measured: WindowServer, and a like-for-like run with `bench.py`.

## Sources

- mpv: [vo.rst v0.41.0](https://github.com/mpv-player/mpv/blob/v0.41.0/DOCS/man/vo.rst), [vo_kitty.c](https://github.com/mpv-player/mpv/blob/master/video/out/vo_kitty.c), commit `e5a74aaaa` (shm name), [issue #16299](https://github.com/mpv-player/mpv/issues/16299)
- Ghostty: [discussion #7470](https://github.com/ghostty-org/ghostty/discussions/7470), [PR #14379](https://github.com/ghostty-org/ghostty/pull/14379), [build docs](https://ghostty.org/docs/install/build), [1.3.0 notes](https://ghostty.org/docs/install/release-notes/1-3-0), [1.3.1 notes](https://ghostty.org/docs/install/release-notes/1-3-1)
- kitty: [graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/), [issue #10588](https://github.com/kovidgoyal/kitty/issues/10588)
- Others: [porthole #80](https://github.com/flotilla-org/porthole/issues/80), [vid](https://github.com/jhickner/vid), [ueberzugpp](https://github.com/jstkdng/ueberzugpp), [kitty-graphics.el](https://github.com/cashmeredev/kitty-graphics.el), [bunker-go PR #12](https://github.com/reyer3/bunker-go/pull/12), [zellij #5626](https://github.com/zellij-org/zellij/issues/5626)
