# CLAUDE.md

Pepper: a personal fork of [Ghostty](https://github.com/ghostty-org/ghostty) (MIT) that shows video inside the terminal straight from the graphics chip, with no copies, for tuitube (`../tuitube`, a terminal YouTube client in Rust). Named after Pepper's ghost, the stage trick that lays a picture over the live scene. For this Mac only for now (macOS, Apple M5 Pro); the aim is the least the machine can spend, about what mpv's own window costs.

The research behind it, with measurements, the routes weighed and sources: `notes/Inline video.md`. The test scripts and clients it used: `notes/inline-video/`. **Where the work stands and what's next: `notes/Handoff.md`. Read it first.**

Ghostty's own guide for agents holds here too (its build, test and format commands, where things are):

@AGENTS.md

Its rule against creating issues or PRs is about Ghostty's repository; anything ever sent upstream also falls under `AI_POLICY.md` (AI use disclosed, and the human understands all of it).

## Why a fork

The kitty graphics protocol can't take a picture that's already on the graphics chip: every frame goes graphics chip → memory → terminal → graphics chip. Measured in Ghostty 1.3.1 (1080p30): mpv's own window ≈16% of a core; mpv `--vo=kitty` in a small box ≈75% and +670 MB in Ghostty; in a large box ≈113% and only 21 of 30 fps. Ghostty has no plugin system (AppleScript only controls windows, tabs, splits and input; custom shaders only change Ghostty's own screen), so the terminal side has to be a patch.

## Branches

- `main`: the fork's copy of Ghostty's `main`, left alone.
- `upstream`: remote for ghostty-org/ghostty. The patch is based on the release tag `v1.3.1` (fetched from it), not `main`: steadier for a terminal used every day.
- `pepper`: the patch branch, made from `v1.3.1` (`332b2aefc`), not pushed yet. It's moved onto each new Ghostty release. Keep it small and mostly in the Swift app (`macos/`), with Zig only where the escape sequence needs it, so moving it stays easy.

## Design (route 3 in the notes)

1. **Handing over buffers.** Pepper opens a named Mach port per process, as Chromium hands IOSurfaces between its processes (`bootstrap_check_in` with a name holding the pid). Tested 2026-10-13: it works from the app (registered as `io.github.erictran308.pepper.<pid>`, given to shells as `PEPPER_PORT`), and programs in its tabs reach it. `bootstrap_check_in` isn't in any Swift module, so `PepperServer.swift` looks it up with `dlsym`. IOSurfaces cross once as Mach ports (`IOSurfaceCreateMachPort` / `IOSurfaceLookupFromMachPort`), never by global id; after that each frame is only "slot k ready" (porthole, flotilla-org/porthole #80, does the same with an `MTLSharedEvent` fence).
2. **Placing them.** tuitube prints a short escape sequence with a random token and the cell rectangle, so Pepper knows which tab and box; only a program that can write to that tab and knows the token can place video there. The helper's Mach messages carry the same token. Form: `OSC 777;pepper;…`, the rxvt extension form Ghostty already parses for `notify`; other terminals ignore names they don't know.
3. **Showing them.** A layer over the terminal view at that rectangle, `contents` set to the IOSurface of each frame, composited by macOS with no copies and no redraw of the terminal. Later, if measured worth it: `AVSampleBufferDisplayLayer` fed VideoToolbox's NV12 buffers, as Safari shows video.

The other end, in tuitube: a video helper on libmpv (Homebrew's `libmpv.2.dylib`), written in Rust as its own binary (linking libmpv makes it GPL; tuitube itself stays MIT), so sound, sync, subtitles, chapters, SponsorBlock seeks, the visualizer filter and the same `input-ipc-client=fd://3` control stay. It renders through mpv's render API (OpenGL FBO) into a ring of 3 IOSurface-backed textures. Risk: mpv's Now Playing and media keys may not work as a library.

Security: what Pepper accepts on its port is pixels only; tie each stream to its tab by the token and refuse the rest. The helper decodes YouTube's bytes, so it runs as tuitube runs mpv (own process, cleaned environment, nothing private on its command line). tuitube's rules are in `../tuitube/CLAUDE.md`.

## Building

Needs, on this Mac: full Xcode with the macOS SDK, iOS SDK and Metal Toolchain (Xcode → Settings → Components; `xcode-select -p` must point into Xcode, not `/Library/Developer/CommandLineTools`); Zig **0.15.2** exactly (`build.zig.zon`'s `minimum_zig_version`; Ghostty 1.3.x and tip build with no other); `brew install gettext`.

```sh
./pepper-build.sh    # zig build -Doptimize=ReleaseFast → zig-out/Ghostty.app
```

Plain `zig build` fails on this Mac with every libc symbol undefined. Apple's 26.4+ SDKs (here 26.5 and 27.0) list only `arm64e-macos` in their `.tbd` stubs, and Zig 0.15.2 can't link against those (fixed in Zig 0.16, ziglang/zig#31673). `pepper-build.sh` gives Zig a patched clone of the SDK (`~/.cache/pepper/sdk`, plus an `xcrun` that reports it). The clone's `math.h` also gets the INFINITY/NAN fallback that Xcode 27's headers need (upstream `1c861e3c4`). The branch also carries upstream's libtool fix `a83a82b3f`: Apple's libtool silently drops archive members Zig doesn't align. Drop all three once the patch moves to a Ghostty release on Zig 0.16. Zig 0.15.2 is at `~/.local/share/zig/`, linked from `~/.local/bin/zig` (Homebrew's is 0.17). Ghostty's build passes only `PATH` to `xcodebuild`, so `xcode-select` must point into Xcode; `DEVELOPER_DIR` alone isn't enough.

The app is called Pepper, with bundle id `io.github.erictran308.pepper` (Ghostty's is `com.mitchellh.ghostty`), so it installs next to the real Ghostty, which stays the fallback. Only the Xcode project's settings changed. `src/build_config.zig`'s `bundle_id` stays Ghostty's, so both apps share the config (`~/.config/ghostty/config`) and `~/Library/Application Support/com.mitchellh.ghostty`. Sparkle's feed is off, because Ghostty's signed releases would install over Pepper. The menu bar still says Ghostty, and the build still comes out as `zig-out/Ghostty.app`; install it as `/Applications/Pepper.app`.

The name is "Pepper", capitalized, wherever it's shown or written in prose, as "Ghostty" is. Paths, bundle ids, env names (`PEPPER_PORT`), the `pepper-play` command, the `pepper` branch, the `777;pepper;` keyword and the `pepper:` tags in code comments stay lowercase.

## Steps

1. Install Xcode (the user) and its components; Zig 0.15.2; gettext.
2. Build `v1.3.1` unchanged and run it, so the build is known to work before any patch.
3. Spike: Pepper shows one IOSurface, made by a small test program, in a box of a tab (answers the Mach port question).
4. The helper in tuitube: libmpv → IOSurface ring → Pepper.
5. tuitube: a setting, the video hidden under popups, resizes.
6. Measure against mpv's window with `notes/inline-video/bench.py` (adapted).

Rough time: ½–1 day for the spike, 2–3 days each for the helper and tuitube's side.
