# Handoff (2026-10-14)

Where the work stands, for the next session. Read this, then CLAUDE.md and `notes/Inline video.md`.

## Done and committed

- **Pepper** (branch `pepper`, not pushed): Zig 0.15.2 build on macOS 27 (`pepper-build.sh`), own name and bundle id, Mach-port IOSurface server (`macos/Sources/Features/Pepper/PepperServer.swift`), `OSC 777;pepper;place|remove;TOKEN;…` with a token, cells→pixels in Zig. Measured: the player costs ~45% less CPU and 3.5× less memory than mpv's own window; Pepper and WindowServer add nothing. Last commit `f9256db21`.
- **tuitube** (`main`, not pushed): watch page (Enter), picture-in-picture (`P`), inline playback through `tuitube-video` (`video/` crate: libmpv → IOSurfaces → Pepper), Now Playing published by the helper itself (`video/src/nowplaying.rs`), helper runs as an accessory NSApplication with mpv on a second thread (`video/src/runloop.rs`), helper bundled as `tuitube.app` (`src/bundle.rs`), one automatic retry when YouTube refuses the stream addresses (HTTP 403), `TT_VIDEO_LOG`. Last commit `c651808`. 237 + 2 tests pass, clippy clean, live tests pass.
- Security review of both: no findings ≥ 8/10 (two hardening notes: forged dead-name notices on the public port; boxes not clipped to their split).

## Step 1, pepper-play: done (2026-10-14)

Committed: Pepper `65b845d07` (the player), `755925af9` (the name), `ac6f6e03e` (notes); tuitube `fee10f9`.

- **Pepper:** `pepper-play/` (the helper, moved from tuitube; libmpv by `dlopen`; shell and app modes; `README.md` is the integration guide); `PepperServer.swift` sends the size message (`0x7070_0101`) and sets `PEPPER_PLAY`; `pepper-build.sh` builds it into `Contents/Helpers/Pepper.app`, links `Contents/MacOS/pepper-play` (on PATH; it re-runs itself by its real path so macOS sees its bundle), signs nested then outer.
- **tuitube:** `video/` and the workspace gone; `bundle.rs` back to mpv only; `Tools::find_pepper_play` (`pepper_play` setting, `TT_PEPPER_PLAY`, `PEPPER_PLAY`); no more `pepper-size` or pixel sizes; `PEPPER_PLAY_DEBUG`; README and CLAUDE.md. 236 tests pass, clippy clean.
- **Checked:** 9 unit tests; a 5 s file plays through the loaded libmpv (153 frames drawn and shown); shell mode in a pty (`q`, Ctrl-C, the end, a missing file → exit 2) restores the screen and removes the box; by the link, macOS sees `io.github.erictran308.pepper.play`.
- **Checked by the user in Pepper (11:13):** `pepper-play FILE` in a tab, and tuitube through `PEPPER_PLAY`, Now Playing included. The notch app showed "pepper-play": MediaRemote names the player by its bundle's folder, as LaunchServices has it for the bundle id (tested: not by `CFBundleName`, a localized name or the executable's name), so the helper is now `Contents/Helpers/Pepper.app`. Pepper's log showed the re-attach the size message causes. Fixed after: mpv's harmless errors (CoreAudio's channel layout) were shown at the end; now only when something couldn't play. Pepper logged the send-once notice a re-attach causes (id 0o107); now ignored. Installed and checked again by the user (the notch says "Pepper").
- **Named Pepper (the user, 2026-10-14):** the app's name is "Pepper", capitalized, wherever it's shown or written in prose, as "Ghostty" is (`CFBundleDisplayName = Pepper`; installed as `/Applications/Pepper.app`, whose folder name is what macOS shows). Paths, bundle ids, env names, the `pepper-play` command, the `pepper` branch and the `777;pepper;` keyword stay lowercase, and so do the `pepper:` tags in code comments.
- Leftover on this Mac: tuitube's old `<data>/video/` bundles (tuitube-video's), no longer used.

## Step 2, the full rename: done (2026-10-14)

Committed: `38e79c9d8`. Checked by the user: the Dock shows the icon and "Pepper".

- `PRODUCT_NAME = Pepper` (`PRODUCT_MODULE_NAME = Ghostty` for the xibs), so the build is `zig-out/Pepper.app` (`GhosttyXcodebuild.zig`, test hosts, `pepper-build.sh`, docs). The executable stays `ghostty`.
- Shown text: the app menu (About, Hide, Quit, "Make Pepper the Default Terminal"), Help → "Ghostty Docs", "👻 Pepper" placeholders (6 xibs, 2 window styles), About ("Pepper", what it is, "Based on Ghostty (MIT)" linking ghostty.org, GitHub → erictran308/pepper, "Ghostty Docs"), alerts (quit, close, "Allow Pepper to execute"), `main.swift`, ErrorView, SettingsView, the debug-build notice, the update popover and command, Shortcuts (IntentPermission, GhosttyIntentError), AppleScript errors, every permission prompt ("A program running within Pepper…").
- **Icon (the user chose 2026-10-14):** Ghostty's bezel and blue screen with "> ▶" (a prompt, then a play symbol) in the ghost's style, where the ghost was. `images/Pepper.icon`, made by `images/make-pepper-icon.py` from `images/Ghostty.icon`; the flat copies in `AppIconImage.imageset` (About, Settings, the error view, the Dock tile) rendered from the built app by `images/render-app-icon.swift`. Ghostty's alternate icons (`macos-icon`) stay, opt-in, and the About window still cycles through them.
- `pepper-build.sh` deletes `zig-out/Pepper.app` before building: the build copies the app over the old one and kept files it no longer has. After renaming a resource, Xcode's own output keeps the old one too (`macos/build/Ghostty.build/ReleaseLocal/Ghostty.build/assetcatalog_output`): delete it once.

## Decided by the user, not done yet (in this order)

1. **pepper-play: the helper for any app**, not just tuitube. (Done; see above.) Move `tuitube/video/` into this repo as `pepper-play/` (Rust; links libmpv, so the binary is GPL; keep the source's license note), shipped inside `Pepper.app` as a nested helper app (`Contents/Helpers/Pepper.app`, its own bundle id, `io.github.erictran308.pepper.play`, name "Pepper", `LSUIElement`), and Pepper sets `PEPPER_PLAY` (its path) for every shell next to `PEPPER_PORT`.
   - Shell use: `pepper-play FILE|URL` plays in the terminal (alt screen, box = window minus mpv's status line, mpv's keys through `terminal`/`input-terminal`, `ytdl` on), removes the box on exit.
   - App use: token from env `PEPPER_TOKEN` (or the existing IPC `script-message-to main pepper-token`); the app prints `OSC 777;pepper;place;…` itself (or asks pepper-play to, over IPC `pepper-place COL ROW COLS ROWS` / `pepper-remove`, which writes to `/dev/tty`); control is mpv's JSON IPC (`--input-ipc-client=fd://N` or `--input-ipc-server`).
   - **Pepper reports the box's size**: when a token's placement and stream both exist, or the placement moves/resizes/goes, Pepper sends the client (its reply port, kept for the dead-name notice) a Mach message (e.g. id `0x7070_0101`: width px, height px, visible) without blocking (send timeout 0). pepper-play gets a receive thread on its port and redraws at that size; apps never compute pixels, and `pepper-size` goes away.
   - Load libmpv with `dlopen` (`PEPPER_LIBMPV`, `/opt/homebrew/lib/libmpv.2.dylib`, `/usr/local/lib/…`, MacPorts), with a clear "brew install mpv" error, instead of linking it.
   - `pepper-build.sh`: build pepper-play with cargo, put it in the nested app, sign nested then outer (ad hoc).
   - tuitube becomes a client: drop `video/`, the workspace entry, `bundle::put_video_in` and `pepper-size`; find the player from `PEPPER_PLAY` (setting/env override, e.g. `pepper_play` / `TT_PEPPER_PLAY`); docs.
   - An integration guide (`pepper-play/README.md`): the escape sequence, env, IPC, short examples (shell, Python, Rust).
2. **Full rename for the public download.** (Done; see above.)
   - **Product name:** set `PRODUCT_NAME = Pepper` for the Ghostty target (Debug/Release/ReleaseLocal), plus `PRODUCT_MODULE_NAME = Ghostty`, since 16 xibs use `customModule="Ghostty"`.
   - **Paths:**
     - `src/build/GhosttyXcodebuild.zig:52`: `Ghostty.app` → `Pepper.app`.
     - Test `TEST_HOST`s and the product file reference in the pbxproj.
     - Docs that say `zig-out/Ghostty.app`.
   - **Menus** (`MainMenu.xib`): About/Hide/Quit/"Make … the Default Terminal" become Pepper; Help becomes "Ghostty Docs".
   - **Window placeholders:** "👻 Ghostty" in 7 xibs and 2 Swift window styles.
   - **About** (`Features/About/AboutView.swift`): name, description, credit "Based on Ghostty (MIT)", GitHub link to erictran308/pepper.
   - **Alerts and messages:** `AppDelegate.swift` (Quit, Close, "Allow … to execute"), `main.swift`, `ErrorView`, `SettingsView`, `TerminalView` debug notices, `IntentPermission`, `GhosttyIntentError`, AppleScript errors.
   - **Icon:** a new default icon. `ASSETCATALOG_COMPILER_APPICON_NAME = Ghostty`; find that asset, and also `Assets.xcassets/AppIconImage.imageset` used by About. Ghostty's alternate icons can stay as opt-in.
   - **Leave alone:** internal names, settings keys, the `.sdef`.
3. **README** (drafted 2026-10-14, replaces Ghostty's README.md; Ghostty's is linked upstream). The user asked for it to be bold that Pepper leaves Ghostty's terminal alone and only adds the video, so people aren't afraid to install it, and careful about security, since it's a terminal (above all, command execution). It says exactly what changes: one escape sequence in the core (290 lines with tests, `git diff v1.3.1..pepper -- src include ':!src/build'`), one new Swift file (`PepperServer.swift`, 468 lines, plus 22 that hook it in), pepper-play as a separate program, the name and icon, the build. Keep those numbers true whenever the patch changes. Never claim the core is untouched: it isn't.
4. **Security audit, strict** (the user, 2026-10-14), before the release: everything Pepper adds (the OSC parser and its path to the app, `PepperServer.swift`'s Mach messages, pepper-play, tuitube's use of it), above all anything near command execution, and the earlier hardening notes (forged dead-name notices; boxes not clipped to their split). Then:
   - The README's Security section says how it was audited and what was found and fixed (the user wants that told plainly, and only after the audit is real).
   - A `SECURITY.md` of Pepper's own (Ghostty's sends reports to Ghostty).
   - Turn on private vulnerability reporting on `erictran308/pepper` (the README links to it).
5. **Release:** the user chose **ad-hoc signing**, a **public repo** (the fork `erictran308/pepper` is already public; only `main` is there), and a **full rename**. Steps:
   - Build and zip with `ditto -c -k --keepParent` (`Pepper.zip`, the name the README gives).
   - Check on a quarantined copy that "Open Anyway" is enough, pepper-play included (it runs from a shell, so Gatekeeper may ask about it separately); if not, say so in the README (`xattr -dr com.apple.quarantine /Applications/Pepper.app`).
   - Push the `pepper` branch only after the user confirms at that moment. The README's compare link (`ghostty-org/ghostty/compare/v1.3.1...erictran308:pepper:pepper`) works once it's pushed.
   - Publish a GitHub Release.
   - Maybe make `pepper` the default branch, so the README shows.
6. **tuitube:** a note under the "Pepper" setting (settings tab, `ui::settings_lines`) on where to get Pepper and pepper-play. Then its release (0.10.0) by the steps in its CLAUDE.md, also only when the user says. Pepper's README already says tuitube plays inside Pepper: release them together.

## Open issues

- Enter on a card: a resumed video takes ~4 s to show (seeking the stream).
- The notch app (Vorssaint) shows a paused browser tab whenever tuitube isn't playing; that's the app, not us.
- An orphan idle `mpv` (pid 32643) from another session's tuitube tests may still be running.

## Tools (all in `notes/inline-video/`)

- **Players and Pepper:**
  - `helper_try.py`: drives the helper like tuitube, on a file.
  - `helper_real_stream.py`: the same with tuitube's exact options on a real YouTube stream (yt-dlp), with a cover and a resume point.
  - `pepper_play.c`: a raw client for Pepper's Mach port and the OSC sequence.
  - `machtest.c`: the first Mach handoff test.
- **Measuring:** `bench_pepper.py` (against mpv's window, in a Pepper tab).
- **Now Playing:**
  - `nowplaying_dump.m` + `nowplaying_dump.pl`: what macOS Now Playing holds right now. Build with `clang -dynamiclib -fobjc-arc -framework Foundation -o np.dylib nowplaying_dump.m`, run with `/usr/bin/perl nowplaying_dump.pl "$PWD/np.dylib"` (a relative path is refused). Apple's perl may read MediaRemote; other programs may not.
  - `nowplaying_name.m`: the name Now Playing gives the app playing (MediaRemote's, AppKit's, the folder's), built and run the same way.
  - mediaremoted's log: `/usr/bin/log stream --level debug --predicate 'process BEGINSWITH "mediaremote"'`.
- **Test video:** `ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=30 -f lavfi -i "sine=frequency=440:sample_rate=48000,volume=0.1" -t 30 -c:v libx264 -preset veryfast -pix_fmt yuv420p -b:v 6M -c:a aac -shortest test.mp4`

## Gotchas on this Mac

- **Shell:** zsh has a `log` builtin, so use `/usr/bin/log`. `ls` is aliased to eza with colors, so use `command ls`.
- **Finding Pepper:** its pid is `pgrep -f '^/Applications/Pepper.app/Contents/MacOS/ghostty$'`, and its port is `io.github.erictran308.pepper.<pid>`.
- **Testing a Pepper build:** quit Pepper, then `rm -rf /Applications/Pepper.app && ditto zig-out/Pepper.app /Applications/Pepper.app && open /Applications/Pepper.app`. Replace it whole: `ditto` into an existing app keeps files the new build no longer has, which breaks its signature, and on this case-insensitive disk it keeps the old folder name (`pepper.app`).
- **Now Playing's name for the player is cached:** `mediaremoted` (root, runs from boot) keeps the name it first saw for a bundle id, from the bundle's folder as LaunchServices had it. After renaming the helper's folder, `sudo killall mediaremoted` (launchd starts it again) or a restart; a new bundle id shows the new name at once. LaunchServices also keeps every copy it saw, test copies included: remove one with `lsregister -u PATH` (`/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister`), and check with `lsregister -dump | rg erictran308`. To see the name: `nowplaying_name.m` (built and run as `nowplaying_dump.m`).
- **tuitube's helper bundle:** it's remade when tuitube starts in pepper; `tuitube --check` with `PEPPER_PORT` set also remakes it.
