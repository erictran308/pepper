# pepper-play: what it costs, and what made it cheaper

As of 2026-10-15. Phase 0 of the agent spec (`notes/agent.md`): measured changes to `pepper-play` and `PepperServer.swift`, each kept only if the numbers support it.

## The machine and how it's measured

- Apple M5 Pro (18 cores, 24 GB), macOS 27.2 (26B5112a). One display: LG UltraFine, 6016×3384 at 60 Hz (2× scale).
- mpv 0.41.0 (Homebrew; FFmpeg 9.0.2, libplacebo 7.360.1). Rust 1.97.1.
- Pepper: a test instance of `zig-out/Pepper.app` (Ghostty 1.3.1 + Pepper's changes at `cedd00758`), with the user's Ghostty config: font size 15, `background-opacity = 0.9` (a see-through window, which WindowServer blends over the desktop), `window-colorspace = display-p3`.
- AeroSpace (a tiling window manager) runs on this Mac. It floats Pepper's windows (the user's config) and tiles everything else, so the bench floats mpv's window through `aerospace` before sizing it.

`pepper-play/bench/run.sh --new-pepper` builds this checkout's pepper-play, makes the clips (`bench/clips.sh`: lavfi `testsrc2`, 70 s, BT.709 limited range, a quiet sine), opens a Pepper of its own and, in its one tab, measures (`bench/measure.py`):

1. **idle**: Pepper and WindowServer with nothing playing, 20 s.
2. For each clip, **pepper**: pepper-play in a box of about 1920×1080 px (here 107×25 cells, 1926×1058 px), driven over its IPC as an app drives it; then **window**: mpv in its own window of the same width in pixels, with `--no-config --osc=no --terminal=no`, the same options, and pepper-play's own `hwdec` and `video-sync` as they were. 3 s after the first frame, 60 s are measured.

Columns: CPU as a share of one core (mean over the 60 s, and the 95th percentile of its seconds), from each process's CPU time (`proc_pid_rusage`; `ps` for WindowServer), not `ps`'s `%cpu`, which decays over many seconds. Memory: the most resident (RSS) and the most footprint (what Activity Monitor shows; it counts a process's IOSurfaces). The graphics chip's use is ioreg's Device Utilization %, sampled each second. Frame counts are mpv's, over the 60 s.

Raw results (JSON lines) stay out of git, in `pepper-play/bench/results/`.

## Before anything (baseline, `cedd00758` plus the debug counts)

Both players decode on the CPU: pepper-play sets no `hwdec`, libmpv's default is `no`, and the window gets the same.

| Clip | pepper-play % (p95) | mpv window % (p95) | pepper-play MB rss / footprint | mpv window MB rss / footprint |
| --- | --- | --- | --- | --- |
| 1080p30 H.264 | 33.2 (34.6) | 30.9 (32.3) | 273 / 414 | 676 / 893 |
| 1080p30 HEVC | 36.6 (38.6) | 32.5 (33.5) | 294 / 428 | 795 / 1007 |
| 1080p60 H.264 | 48.7 (51.8) | 41.1 (43.5) | 275 / 415 | 623 / 838 |
| 1080p60 HEVC | 45.5 (46.7) | 38.7 (39.6) | 299 / 433 | 700 / 912 |
| 2160p30 H.264 | 58.6 (59.7) | 42.1 (43.4) | 620 / 929 | 1108 / 1506 |
| 2160p30 HEVC | 69.9 (71.0) | 54.7 (55.6) | 730 / 1039 | 1173 / 1586 |
| 1080p24 H.264 | 26.4 (27.8) | 25.7 (27.2) | 274 / 408 | 678 / 895 |

- Decoding on the CPU, pepper-play cost *more* than mpv's window: up to 28% more at 4K. mpv's window (`gpu-next`, Vulkan) uploads software-decoded frames more cheaply than libmpv's OpenGL render API does.
- No frame was dropped or delayed in any run. pepper-play drew 1,899 frames in the 1080p30 run (63 s from start to end) and showed 1,898; 2 were drawn into a surface still on screen.
- Pepper: 4.6% idle, 2.4–4.7% while its box played. WindowServer: 47% idle with Pepper's see-through window on screen, 46–48% while it played (nothing added); 18–35% in the window runs, where mpv's opaque window covers Pepper's.

## 10.1 Hardware decoding: kept

`hwdec=auto-safe` is now set among pepper-play's own options, before the user's or the app's, so `--hwdec=no` still turns it off (unit test `own_options_first_so_the_users_win`).

| Clip | Baseline % | `auto-safe` % (p95) | Change | mpv window, `auto-safe` % (p95) | pepper-play MB rss / footprint | mpv window MB rss / footprint |
| --- | --- | --- | --- | --- | --- | --- |
| 1080p30 H.264 | 33.2 | 9.9 (10.5) | −70% | 14.3 (15.2) | 167 / 301 | 543 / 708 |
| 1080p30 HEVC | 36.6 | 9.3 (9.8) | −75% | 15.0 (15.9) | 173 / 307 | 627 / 800 |
| 1080p60 H.264 | 48.7 | 17.2 (18.3) | −65% | 26.2 (27.9) | 171 / 305 | 532 / 682 |
| 1080p60 HEVC | 45.5 | 17.6 (18.3) | −61% | 25.5 (27.6) | 179 / 313 | 538 / 688 |
| 2160p30 H.264 | 58.6 | 11.3 (11.8) | −81% | 15.9 (17.0) | 259 / 549 | 631 / 869 |
| 2160p30 HEVC | 69.9 | 11.3 (11.8) | −84% | 15.5 (16.1) | 275 / 566 | 655 / 880 |
| 1080p24 H.264 | 26.4 | 8.1 (8.7) | −69% | 12.4 (13.0) | 168 / 302 | 579 / 872 |

- `hwdec-current` was `videotoolbox` for every clip, H.264 and HEVC alike. No frame dropped or delayed.
- Against mpv's own window with the same decoding, pepper-play now costs 27–38% less CPU. Its resident memory is 28–33% of the window's for 1080p clips and 41–42% for 4K; its footprint 35–45% for 1080p and 63–64% for 4K (the decoder's 4K surfaces are the same for both).
- Pepper itself: 4.3–5.9% while its box played, against 4.7% idle, so about +1% at 60 frames a second (10.7 looks at that).

## 10.2 A fence instead of `glFinish`: reverted

Three ways to know a frame is whole before Pepper shows it, picked for the run by a temporary `PEPPER_PLAY_SYNC`, Pepper side only, 60 s each:

| Variant | 1080p60 H.264 % (p95) | 2160p30 HEVC % (p95) | Waiting on the graphics chip, 1080p60 |
| --- | --- | --- | --- |
| `glFinish` after each frame (as it was) | 15.9 (16.4) | 11.9 (12.3) | 0.30 s for 3,802 frames |
| `glFenceSync` + `glFlush` + `glClientWaitSync` (100 ms bound) | 18.9 (19.4) | 11.1 (11.6) | 0.33 s |
| Pipelined: fence, show it at the next wakeup once it's passed | 16.1 (17.1) | 11.8 (12.3) | 0.05 s |

- With hardware decoding the render pass is tiny: `glFinish` waits about 0.08 ms a frame, and blocks without spinning. Nothing to win.
- The differences are within run-to-run noise: the same `glFinish` build measured 17.2% and 15.9% for 1080p60 in two runs.
- Kept `glFinish`; the fence code is gone. No torn-frame check was needed for a variant that wasn't kept.

## 10.3 Frames shown at the display's refresh: kept

A `CVDisplayLink` on the active displays (`src/vsync.rs`) wakes the render thread at each refresh while a frame waits for one. pepper-play tells mpv the display's rate (`display-fps-override`, which libmpv can't ask for itself) and sets `video-sync=display-resample` among its own options, so `--video-sync=audio` still wins. The render context is made with `MPV_RENDER_PARAM_ADVANCED_CONTROL`.

How a frame goes, by mpv's `NEXT_FRAME_INFO`:
- **`BLOCK_VSYNC`** (display-synced): drawn when mpv has it, shown at the next refresh, and `mpv_render_context_report_swap` called then, as a window's swap would be.
- **`REPEAT`** (display-sync draws a frame again at each refresh it lasts, 2 or 3 for 24 fps on 60 Hz; mpv documents it as "reproduce the previous frame perfectly"): not drawn (`MPV_RENDER_PARAM_SKIP_RENDERING`); what's on screen stays, and the swap is still told at the refresh. Not with `interpolation`, whose frames differ.
- Anything else (audio-timed playback, redraws while paused): shown at once, as before.
- The link stops after 30 refreshes with nothing to show, so a paused video costs no wakeups.
- The render thread runs at `QOS_CLASS_USER_INTERACTIVE`.

Getting it right took one bug found by tracing each swap: reporting a swap for a redraw, which mpv doesn't wait for, left mpv one swap ahead. From then on each new frame arrived while the last still waited, and reporting that one too kept mpv racing ahead (90 draws a second, jitter 0.33, mistimed and delayed frames). Now only mpv's own frames are reported, a frame replaced before its refresh isn't, and the swaps line up with the refreshes: estimated display rate 60.000, jitter 0.03.

| Clip, 60 s | 10.1 % | 10.3 % (p95) | vsync-jitter | mistimed / delayed / dropped | wakeups/s | mpv window, display-resample % | its jitter |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1080p24 H.264 | 8.1 | 8.5 (9.0) | 0.028 | 1 / 1 / 0 | 87 (was 38) | 19.8 | 0.072 |
| 1080p30 H.264 | 9.9 | 10.3 (10.6) | 0.027 | 0 / 0 / 0 | 88 (was 46) | 21.2 | 0.029 |
| 1080p60 H.264 | 17.2 | 16.3 (16.9) | 0.047 | 0 / 0 / 0 | 87 (was 77) | 24.8 | 0.065 |

- The same build with `--video-sync=audio`, in the same session: 8.4% (24 fps) and 9.6% (30 fps). So timing to the display costs +0.1% and +0.7% of a core and twice the wakeups at 24–30 fps, and nothing at 60. Kept: within the noise, for an even cadence.
- 1,522 frames drawn for the 24 fps clip: the repeats weren't drawn.
- mpv's own window pays much more for display sync (12.4% → 19.8% at 24 fps): it draws every refresh.
- Each frame now waits about 2.8 ms for the graphics chip, against 0.08 ms before: it's drawn right after a refresh, while the window server composites. That's waiting, not CPU.

## 10.4 No new surfaces at every size during a drag: kept

Pepper reports the box's size each time it changes, and each new size made a new ring of surfaces. Now new ones are made once the size has stayed the same 80 ms, or 500 ms after the old ones stopped fitting during a long drag; until then frames go into the old surfaces, which Pepper scales to the box (`resizeAspect`). The first size Pepper reports is taken at once. (The spec said "or on the next mpv frame, whichever comes first": during playback a frame comes every 17–42 ms, so that would make new surfaces as often as before.) Unit test: `surfaces_wait_for_the_size_to_settle`.

`bench/measure.sh --drag 5` places the box again every 16 ms for 5 s, its size swinging between 60% and 100%, as an app does through a window drag. 1080p30, display-synced:

| | Rings made | Frames drawn / shown | Drawn into a surface still on screen | vsync-jitter |
| --- | --- | --- | --- | --- |
| Before | 222 (888 surfaces) | 565 / 493 | 222 | 0.36 |
| After | 12 (2 at the start, 10 in the drag) | 405 / 400 | 12 | 0.08 |

## 10.5 A ring of three surfaces: not kept, four stay

The client now sends however many surfaces its ring has (`Client::attach` is generic over 1 to `MAX_SURFACES`, with a layout test for 3 and 4), and the ring is a constant, `RING`. Tried at 3, A/B with the same code otherwise, 60 s, Pepper side:

| | 1080p60 H.264 % | Drawn into a surface still on screen | Footprint MB | 2160p30 HEVC % | Drawn into one on screen | Footprint MB |
| --- | --- | --- | --- | --- | --- | --- |
| 4 surfaces | 15.8 | 3 of 3,799 | 305 | 11.8 | 2 | 567 |
| 3 surfaces | 16.1 | 1,495 of 3,801 | 297 | 11.6 | 2 | 558 |

At 60 fps, shown at each refresh, one surface is on screen, the one before is still the window server's for a moment, and the next is being drawn: three are busy, so a ring of three draws into one still showing 40% of the time. The saving is 8 MB for this box (2.7 MB per 1080p surface; 83 MB per surface only at the 6K cap). `RING` stays `MAX_SURFACES` (4).

## 10.6 Colour space: kept

Surfaces were untagged BGRA. They're now tagged sRGB (`kIOSurfaceColorSpace`, the colour space serialized with `CGColorSpaceCopyPropertyList`, as IOSurfaceRef.h asks), which is what mpv draws.

Checked with `bench/run.sh --new-pepper --look` (`bench/look.py`): the sRGB patches clip (`colors-1080p30-h264`), paused, in Pepper boxes whose surfaces were tagged four ways, next to mpv's own window; one screenshot of the whole screen (6016×3384, the display's profile), each patch averaged over 9×9 pixels:

| | Red | Green | Blue | Cyan | Magenta | Yellow | Grey (128) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Pepper, untagged (before) | 255, 1, 0 | 0, 255, 1 | 1, 0, 255 | 0, 254, 255 | 255, 0, 253 | 254, 255, 0 | 128 |
| Pepper, tagged by name (`kCGColorSpaceSRGB`) | 229, 48, 30 | 110, 251, 57 | 1, 0, 246 | 109, 249, 253 | 229, 46, 246 | 254, 255, 64 | 117 |
| Pepper, tagged with the ICC profile's data | same | same | same | same | same | same | 117 |
| Pepper, tagged with the serialized colour space | same | same | same | same | same | same | 117 |
| mpv's own window | 229, 48, 30 | 110, 251, 57 | 1, 0, 246 | 109, 249, 253 | 229, 46, 247 | 254, 255, 64 | 117 |

Untagged, the window server sent mpv's sRGB values to the display as they were, so on this wide-gamut display every colour was too saturated (pure red shown as the display's own red). Tagged any of the three ways, Pepper matches mpv's window to the unit.

## 10.7 Optional items

- **Pepper's main-thread cost:** not changed. At 60 fps Pepper used 5.9–6.5% of a core against 4.7% idle (10.3, 10.5): at most 1.8% more, about 0.3 ms per frame for the whole process, which is under the spec's 1 ms. Its receive source stays on the main queue.
- **HDR:** deferred. A half-float IOSurface (`RGhA`) binds as an OpenGL framebuffer here (`CGLTexImageIOSurface2D` with `GL_RGBA16F`/`GL_HALF_FLOAT`, complete, and a clear to (2, 0.5, 0.25) reads back 2.000, 0.500, 0.250), so libmpv's OpenGL render API could draw HDR into it (`OpenGlFbo.internal_format = GL_RGBA16F`). But this display has no headroom to show it (`maximumPotentialExtendedDynamicRangeColorComponentValue` is 1), so nothing could be checked here. What it would take: half-float surfaces tagged with an extended-range colour space, mpv's `target-trc`/`target-peak` set from the display's headroom, PepperServer accepting `RGhA` surfaces (an optional, checked addition) and `wantsExtendedDynamicRangeContent` on the box's layer. For a Mac with an HDR display.
- **YUV surfaces as layer contents:** checked, in the same screenshot, with `bench/yuv-layer.swift`: the six patches as a BGRA surface tagged sRGB and as an NV12 one (`420v`, BT.709 limited range, tagged as VideoToolbox tags its frames: `IOSurfaceYCbCrMatrix`, `ColorPrimaries` and `TransferFunction` `ITU_R_709_2`). Every colour matched to within 1. Grey didn't: 110 for NV12 against 117 for BGRA and mpv, consistently. CoreAnimation gets the matrix and range right, but reads the BT.709 transfer function as a display's (BT.1886, darker midtones) where mpv draws it as sRGB.
- **Raw decoder path:** not done. libmpv gives no access to its decoded frames: with hardware decoding they never leave mpv's render pass. Handing VideoToolbox's own surfaces to the layer would need a player of its own (demuxing, decoding with `VTDecompressionSession`, audio and its sync), no subtitles or filters, PepperServer accepting planar surfaces, and the midtones above corrected by its tags. What it would save is mpv's render pass, which the graphics chip finishes in about 0.08 ms a frame (10.2). Not worth it.

## After Phase 0 (the final run)

Everything kept (10.1, 10.3, 10.4, 10.6), the full bench (`bench/run.sh --new-pepper --label phase0-final`), both players with `--hwdec=auto-safe --video-sync=display-resample`:

| Clip | Baseline % | pepper-play % (p95) | mpv window % (p95) | pepper-play MB rss / footprint | mpv window MB rss / footprint |
| --- | --- | --- | --- | --- | --- |
| 1080p24 H.264 | 26.4 | 9.2 (10.6) | 17.8 (18.5) | 168 / 304 | 531 / 691 |
| 1080p30 H.264 | 33.2 | 10.0 (10.4) | 20.8 (22.5) | 169 / 305 | 601 / 761 |
| 1080p30 HEVC | 36.6 | 11.1 (11.9) | 20.0 (21.5) | 174 / 310 | 709 / 874 |
| 1080p60 H.264 | 48.7 | 17.8 (18.9) | 24.6 (27.1) | 171 / 305 | 623 / 1000 |
| 1080p60 HEVC | 45.5 | 17.5 (18.5) | 24.7 (27.2) | 165 / 313 | 521 / 790 |
| 2160p30 H.264 | 58.6 | 11.6 (12.4) | 18.7 (22.9) | 253 / 546 | 657 / 901 |
| 2160p30 HEVC | 69.9 | 12.4 (14.0) | 19.6 (23.6) | 272 / 565 | 503 / 910 |

- pepper-play costs 62–82% less than before Phase 0, and 28–52% less than mpv's own window with the same settings (about half at 24–30 fps, 28% less at 60), with 30–62% of its footprint.
- The frames repeated at 24–30 fps weren't drawn (1,858–2,277 a clip).
- This run is noisy past its first clip: from there Pepper's own CPU fell from 5.8% to about 1%, which happens only when its window isn't on screen (the Mac was in use: another workspace or app in front), and both players dropped or mistimed frames in some clips (pepper-play 21 in 1080p60 HEVC, mpv's window 25 mistimed in 2160p30 HEVC). The runs for each item, with the machine left alone, had none, and their CPU agrees with this run's to within 1%. Pepper's own cost is from those: 5.5–6.5% while its box plays at 60 fps against 4.6–4.7% idle, 1–2% of a core.
- The README's sentence ("about half the CPU and under half the memory … at 30 frames a second", "1–2% of a core") is from these numbers.

## On Ghostty 1.4.0 (1.4.0-pepper.1, 2026-10-28)

Pepper moved onto Ghostty 1.4.0, with pepper-play unchanged (0.3.0). The full bench (`bench/run.sh --new-pepper --label 1.4.0-pepper.1`) on the release build (`3c5be3341`), with the Mac left alone; macOS 27.2 (26B118), mpv 0.41.0, Rust 1.99.1, the same config as above:

| Clip | pepper-play % (p95) | mpv window % (p95) | pepper-play MB rss / footprint | mpv window MB rss / footprint |
| --- | --- | --- | --- | --- |
| 1080p24 H.264 | 8.7 (9.0) | 18.9 (19.8) | 168 / 312 | 515 / 678 |
| 1080p30 H.264 | 10.3 (10.8) | 20.7 (21.6) | 169 / 305 | 529 / 682 |
| 1080p30 HEVC | 10.5 (11.1) | 19.6 (21.0) | 174 / 318 | 675 / 846 |
| 1080p60 H.264 | 16.2 (16.9) | 26.1 (27.2) | 171 / 305 | 675 / 969 |
| 1080p60 HEVC | 16.5 (17.2) | 26.2 (27.3) | 179 / 313 | 714 / 1009 |
| 2160p30 H.264 | 11.7 (12.2) | 21.2 (21.9) | 259 / 551 | 633 / 878 |
| 2160p30 HEVC | 11.9 (12.4) | 21.6 (22.8) | 275 / 568 | 659 / 889 |

- pepper-play's CPU is within 1.6% of a core of the 1.3.1 run's, and 37–54% under mpv's window (about half at 24–30 fps, 37–38% less at 60), with 31–64% of its footprint.
- Pepper itself: 4.9% idle, 5.7–7.4% while its box played (its window on screen the whole run): about +1% at 24–30 fps, +2.5% at 60. WindowServer 44–46% (the see-through window), as before.
- Frame timing: at 1080p, at most 1 frame dropped and 4 late a clip. At 4K this run had pepper-play drop 2–4 and show about 2% of its frames late (24–28 mistimed, 30–34 delayed), mpv's window at most 4; a second run of the two 4K clips (`--label 1.4.0-4k-recheck`) had pepper-play at 0 dropped and 9/14 and 1/4 mistimed/delayed, against mpv's window's 3–4 and 5–8: noise, not a change.
- The README's sentence still holds on 1.4.0.

## Upscaling: bilinear, no sigmoid (2026-10-31)

Full screen on this Mac's 6016×3384 display, pepper-play draws the video at the box's size (6003×3249 px), so a 4K video is upscaled 1.56×. With mpv's defaults (`scale=lanczos`, `sigmoid-upscaling=yes`, a 16-bit FBO) it didn't keep up. Measured with `notes/fullscreen-bench/bench.py --variants` (untracked): 4K YouTube `KLuTLF3x9sA` (`--ytdl-format=401+251`, AV1 8-bit SDR, decoded by VideoToolbox) in a full-screen test Pepper with `custom-shader-animation = false`; each setting alone, 20 s of warmup, then 120 s measured, a cooldown between runs; medians of the whole Mac's power (SMC `PSTR`) against a full-screen test Pepper idle at a prompt (9.12 W). One run each; the browsers were left open (the same load in every run).

| Settings | W over idle | GPU W | Dropped / late frames | Display link | GPU wait a frame | Footprint MB |
| --- | --- | --- | --- | --- | --- | --- |
| mpv's defaults | 4.58 | 2.55 | 226 / 519 | 52.9 fps | 7.9 ms | 1154 |
| `--sigmoid-upscaling=no` | 4.03 | 2.44 | 258 / 541 | 53.4 fps | 8.5 ms | 1154 |
| `--scale=bilinear` | 2.81 | 1.91 | 12 / 101 | 59.1 fps | 4.2 ms | 945 |
| `--fbo-format=rgba8` | 3.54 | 2.45 | 207 / 510 | 53.9 fps | 8.0 ms | 961 |
| `--scale=bilinear --sigmoid-upscaling=no` | 2.84 | 1.89 | 0 / 5 | 59.9 fps | 3.9 ms | 996 |
| `--profile=fast --fbo-format=rgba8` | 1.66 | 2.08 | 0 / 0 | 60.0 fps | 2.0 ms | 703 |

- The bilinear filter carries the saving; without the sigmoid pass too, no frame dropped. These two are now pepper-play's own options, before the user's and the app's, which still win (`--scale=lanczos --sigmoid-upscaling=yes` brings mpv's back).
- Both only change upscaling. A video in a box smaller than it (tuitube's usual case, 4K in about 1926×1058) keeps mpv's downscaling (`dscale`, `correct-downscaling`, `linear-downscaling`), which `--profile=fast` would have changed.
- What's left between them and the profile, about 1.2 W, is `rgba8` and `dither=no`, which apply at every size and risk banding on 10-bit and HDR video: not taken.

The full bench with them (`bench/run.sh --new-pepper --label new-upscale-defaults`), the Mac left alone:

| Clip | pepper-play % (p95) | mpv window % (p95) | pepper-play MB rss / footprint | mpv window MB rss / footprint |
| --- | --- | --- | --- | --- |
| 1080p24 H.264 | 8.6 (9.5) | 7.3 (17.8) | 172 / 287 | 563 / 765 |
| 1080p30 H.264 | 9.8 (10.3) | 17.8 (19.4) | 168 / 276 | 719 / 867 |
| 1080p30 HEVC | 10.2 (10.9) | 17.5 (19.7) | 172 / 288 | 676 / 837 |
| 1080p60 H.264 | 16.6 (18.0) | 25.3 (26.3) | 170 / 283 | 556 / 948 |
| 1080p60 HEVC | 15.8 (16.8) | 24.7 (25.9) | 178 / 291 | 773 / 1065 |
| 2160p30 H.264 | 11.3 (11.8) | 20.0 (22.1) | 254 / 446 | 612 / 1078 |
| 2160p30 HEVC | 11.6 (12.3) | 21.0 (21.9) | 271 / 463 | 782 / 1168 |

- pepper-play's CPU is within 0.7% of a core of the 1.4.0-pepper.1 run's (its box, 1909×1083 px this time, barely scales 1080p; 4K is downscaled), its footprint 25–30 MB less at 1080p and about 105 MB less at 4K. No frame dropped; at most 2 mistimed and 3 delayed a clip (the 1.4.0 run's 4K clips had 2–4 dropped and about 30 late).
- Not a clean run past its first clip: Pepper's own CPU fell from 6.0% to about 1%, as it does when its window isn't on screen (seen once before, "After Phase 0"); the cause wasn't found (the user was away). mpv's window dropped 23–33 frames at 1080p60, and its 1080p24 median (7.3%) is far under its p95 (17.8%). pepper-play's options don't reach mpv's window (`--no-config`, only `hwdec` and `video-sync` passed).
- Pepper itself: 4.8% idle; WindowServer 46% (the user's cursor shader animating, `custom-shader-animation` not set in the test Pepper).

## Repeating it

- The whole bench: `pepper-play/bench/run.sh --new-pepper` (about 17 minutes; leave the Mac alone, with the test window on screen). One item's clips: `--clips 1080p60-h264,2160p30-hevc --no-window --no-idle`. A change against what's built now: build it elsewhere and pass `--player PATH`.
- Hardware decoding: every run records `hwdec-current`; it was `videotoolbox` for H.264, HEVC, and (checked once, by hand) VP9, AV1 and ProRes.
- The resize test: `--clips 1080p30-h264 --seconds 5 --drag 5 --no-window --no-idle`, and look at the rings made (`PEPPER_PLAY_DEBUG`'s last line).
- The eyes-only checks (colours against mpv's window, YUV layers, a live window drag): `--look`. On 2026-10-15 the drag never went blank (the user dragged a test window's corner about; 42 rings were made in all).
- Torn frames: not checked with the frame-counter clip, since the fence variant it was meant for wasn't kept. Frames are still whole before they're shown (`glFinish`), and never drawn into a surface on screen but when all four are (`PEPPER_PLAY_DEBUG`: 2–3 a minute).
- Unit tests (`cargo test` in `pepper-play/`): the options' order, the resize debounce's timing, the ring's size, the attach message's layout for 3 and 4 surfaces.
