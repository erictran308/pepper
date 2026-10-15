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
