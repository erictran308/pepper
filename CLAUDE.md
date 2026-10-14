# CLAUDE.md

Pepper: a personal fork of [Ghostty](https://github.com/ghostty-org/ghostty) (MIT) that shows video inside the terminal straight from the graphics chip, with no copies, for tuitube (`../tuitube`, a terminal YouTube client in Rust). Named after Pepper's ghost, the stage trick that lays a picture over the live scene. For this Mac only for now (macOS, Apple M5 Pro); the aim is the least the machine can spend, about what mpv's own window costs.

The research behind it, with measurements, the routes weighed and sources: `notes/Inline video.md`. The test scripts and clients it used: `notes/inline-video/` (`pepper_play.c`, a raw client of the Mach port and the escape sequence; `bench_pepper.py`, against mpv's window; `nowplaying_dump.m` and `nowplaying_name.m`, what Now Playing holds and what it calls the player).

**Where it stands (2026-10-14):** released, [Pepper 1.3.1-pepper.1](https://github.com/erictran308/pepper/releases/tag/v1.3.1-pepper.1) (`pepper` is the default branch) and tuitube 0.10.0, which plays inside it. A strict security audit of everything Pepper adds came before the release; its report is outside the repo (`~/security-audit-skill/pepper/run-1/`), and the README's Security section says what it found and fixed. Next: each new Ghostty release (see Releasing).

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
./pepper-build.sh    # zig build -Doptimize=ReleaseFast → zig-out/Pepper.app
```

Plain `zig build` fails on this Mac with every libc symbol undefined. Apple's 26.4+ SDKs (here 26.5 and 27.0) list only `arm64e-macos` in their `.tbd` stubs, and Zig 0.15.2 can't link against those (fixed in Zig 0.16, ziglang/zig#31673). `pepper-build.sh` gives Zig a patched clone of the SDK (`~/.cache/pepper/sdk`, plus an `xcrun` that reports it). The clone's `math.h` also gets the INFINITY/NAN fallback that Xcode 27's headers need (upstream `1c861e3c4`). The branch also carries upstream's libtool fix `a83a82b3f`: Apple's libtool silently drops archive members Zig doesn't align. Drop all three once the patch moves to a Ghostty release on Zig 0.16. Zig 0.15.2 is at `~/.local/share/zig/`, linked from `~/.local/bin/zig` (Homebrew's is 0.17). Ghostty's build passes only `PATH` to `xcodebuild`, so `xcode-select` must point into Xcode; `DEVELOPER_DIR` alone isn't enough.

The app is called Pepper, with bundle id `io.github.erictran308.pepper` (Ghostty's is `com.mitchellh.ghostty`), so it installs next to the real Ghostty, which stays the fallback. The app is renamed in the Xcode project (`PRODUCT_NAME = Pepper`, with `PRODUCT_MODULE_NAME = Ghostty`, since the xibs name that module) and in what it shows: menus, window placeholders, About (with "Based on Ghostty (MIT)"), alerts, permission prompts; Help is "Ghostty Docs", since the config and its docs are Ghostty's. Internal names, settings keys and the `.sdef` stay Ghostty's. `src/build_config.zig`'s `bundle_id` stays Ghostty's, so both apps share the config (`~/.config/ghostty/config`) and `~/Library/Application Support/com.mitchellh.ghostty`. Ghostty's updater (Sparkle, `macos/Sources/Features/Update`) is taken out: Ghostty's signed releases would install over Pepper, and without it the app needs no `disable-library-validation`. *Check for Updates…* (`AppDelegate.checkForUpdates`) only says where releases are; the `auto-update` config key does nothing. `pepper-build.sh` signs the app (`macos/GhosttyReleaseLocal.entitlements`) and pepper-play (`pepper-play/pepper-play.entitlements`) with the hardened runtime and fails the build if a signature allows a debugger (`get-task-allow`), `DYLD_` variables, or, for the app, other libraries or made code. The build comes out as `zig-out/Pepper.app`; install it as `/Applications/Pepper.app`.

The name is "Pepper", capitalized, wherever it's shown or written in prose, as "Ghostty" is. Paths, bundle ids, env names (`PEPPER_PORT`), the `pepper-play` command, the `pepper` branch, the `777;pepper;` keyword and the `pepper:` tags in code comments stay lowercase.

## Releasing

Each Ghostty release, Pepper's changes move onto it, and then:

1. Keep the README's numbers true: the core's lines (`git diff vX.Y.Z..pepper -- src include ':!src/build'`), `PepperServer.swift`'s, the 22 that hook it in.
2. Set `pepper_version` (Ghostty's version, then Pepper's count: `1.3.1-pepper.1`) and `pepper_build` in `pepper-build.sh`, and commit.
3. Build from nothing: `rm -rf ~/.cache/pepper macos/build zig-out && ./pepper-build.sh` (Xcode's output keeps stale files; the SDK view is reused on a marker alone). The build fails if a signature allows more than it should.
4. `ditto -c -k --keepParent zig-out/Pepper.app zig-out/Pepper.zip`, and its `shasum -a 256` for the release notes: an ad hoc build has no publisher identity.
5. Check it as a download: copy the zip to `~/Downloads`, mark it (`xattr -w com.apple.quarantine "0083;$(printf %x $(date +%s));Safari;" ~/Downloads/Pepper.zip`), unzip in Finder, drag to Applications, Open Anyway, then `pepper-play` in a tab (the user does this). For 1.3.1-pepper.1 Open Anyway was enough, pepper-play included.
6. Only when the user says so, at that moment: push `pepper` and the tag `v<version>` on the built commit, and `gh release create` with `Pepper.zip` and the SHA-256 in the notes.

## Gotchas on this Mac

- **Shell:** zsh has a `log` builtin, so use `/usr/bin/log`. `ls` is aliased to eza, so use `command ls`.
- **The user's Pepper:** its pid is `pgrep -f '^/Applications/Pepper.app/Contents/MacOS/ghostty$'`, its port `io.github.erictran308.pepper.<pid>`. Never test against it, its port or its tabs. A test instance of a build: `open -n zig-out/Pepper.app --args --command=… --window-save-state=never --quit-after-last-window-closed=true` (other config keys work the same way); macOS's focus rules keep it in the background, and Ghostty opens its first window only once it's active, so activate it by pid (`NSRunningApplication(processIdentifier:)?.activate()`).
- **Installing a build:** the user quits Pepper, then `rm -rf /Applications/Pepper.app && ditto zig-out/Pepper.app /Applications/Pepper.app && open /Applications/Pepper.app`. Replace it whole: `ditto` into an existing app keeps files the new build no longer has, which breaks its signature, and on this case-insensitive disk keeps an old folder name.
- **Now Playing's name for the player is cached:** `mediaremoted` keeps the name it first saw for a bundle id (the bundle's folder, as LaunchServices had it). After renaming the helper's folder: `sudo killall mediaremoted`. LaunchServices keeps every copy it saw, test copies too: `lsregister -u PATH` (`/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister`), check with `lsregister -dump | rg erictran308`.
- **`git commit -- paths`** takes those paths from the working tree: a removal staged with `git rm --cached` comes back if the file is still on disk. Commit such a removal without paths.
- **The README's GIF** (`images/pepper-demo.gif`): a test instance running a scripted demo (`pepper-play` on Big Buck Bunny, CC BY, credited under it), recorded with `screencapture -v -R<window bounds>`, made into a GIF with ffmpeg (10 fps, 960 px, a palette).
