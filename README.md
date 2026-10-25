<p align="center">
  <img src="macos/Assets.xcassets/AppIconImage.imageset/macOS-AppIcon-256px-128pt@2x.png" alt="Pepper's icon" width="128">
</p>

<h1 align="center">Pepper</h1>

<p align="center">
  <b>Ghostty, with video inside the terminal.</b>
  <br>
  <a href="#install">Install</a>
  ·
  <a href="#play-video">Play video</a>
  ·
  <a href="#pepper-for-ai-agents">AI agents</a>
  ·
  <a href="#ghosttys-terminal-untouched">What it changes</a>
  ·
  <a href="#security">Security</a>
  ·
  <a href="pepper-play/README.md">For developers</a>
</p>

<p align="center">
  <img src="images/pepper-demo.gif" alt="In a Pepper tab, pepper-play big-buck-bunny.mp4 plays the video inside the terminal with mpv's status line under it; f makes it fill the tab, and when it ends the shell is back" width="800">
  <br>
  <sub><code>pepper-play</code> in a Pepper tab; <code>f</code> fills the tab. Big Buck Bunny © Blender Foundation, <a href="https://creativecommons.org/licenses/by/3.0/">CC BY 3.0</a>.</sub>
</p>

Pepper is [Ghostty](https://ghostty.org) 1.3.1, the terminal, with one thing added: a program can play a video in a box of cells inside a tab. The video goes straight from the graphics chip to the screen, with no copies. In a Pepper tab, a 1080p video at 30 frames a second costs about half the CPU and under half the memory of mpv playing it in its own window with the same settings (decoded on the graphics chip, timed to the display), and Pepper itself adds 1–2% of a core ([measured](pepper-play/README.md#performance)).

It's named after Pepper's ghost, the stage trick that lays a picture over the live scene.

## Ghostty's terminal, untouched

Pepper is Ghostty. It draws text, reads your keys and runs your shell with Ghostty's own code, unchanged. In what programs print, it understands one more escape sequence. Its only other change of its own to Ghostty's core is in the Metal renderer, which keeps custom shaders' textures once per split instead of once per frame in flight (1.7 GB less with two cursor shaders and five splits on a 6K screen). It also carries 21 fixes taken early from Ghostty's own later code, 17 for security and 4 for performance ([the list](notes/upstream-backports.md)). It reads your Ghostty config (`~/.config/ghostty/config`), and it installs next to Ghostty, which keeps working as before.

Everything Pepper adds to Ghostty 1.3.1 is in this table, and in [a handful of commits](https://github.com/ghostty-org/ghostty/compare/v1.3.1...erictran308:pepper:pepper) you can read in an evening:

| Where | What Pepper adds | Size |
| --- | --- | --- |
| Ghostty's core (Zig) | One escape sequence, `OSC 777;pepper`, which places a video box (with holes, so a popup over it shows); custom shaders' textures kept once per split; and most of the fixes from Ghostty's later code (next rows). | 1,581 lines added and 263 taken out, tests included; the fixes are 903 and 171 of them |
| The macOS app (Swift) | Two new files: [`PepperServer.swift`](macos/Sources/Features/Pepper/PepperServer.swift), which takes frames from a player and shows them over the box, and [`PepperTabBar.swift`](macos/Sources/Features/Pepper/PepperTabBar.swift), a tab bar for `macos-titlebar-style = hidden` that shows what goes on in each tab (video, a progress report, a bell). | 1,102 and 655 lines, plus 45 that hook them in |
| Fixes from Ghostty's later code | 21 fixes Ghostty made after 1.3.1, taken before a release has them: [the list](notes/upstream-backports.md). Most are in the core, counted above. In the app, a link from terminal output is checked before it opens, in two new files ([`UntrustedURL.swift`](macos/Sources/Helpers/UntrustedURL.swift), [`UntrustedURLAlert.swift`](macos/Sources/Helpers/UntrustedURLAlert.swift)), and three crashes are fixed. | 612 lines added and 13 taken out in the app, tests included |
| Ghostty's updater | Taken out: Sparkle and its update screens. Ghostty's releases would install over Pepper, and without it Pepper loads no code but its own and the system's. *Check for Updates…* says where new releases are. | About 2,100 lines fewer |
| The player (Rust) | [`pepper-play`](pepper-play/), a program of its own inside the app. Pepper never starts it: you do, or a program you run does. | Separate program |
| A helper for AI agents (Rust) | [`pepper-agent`](pepper-agent/), a program of its own inside the app: a viewer pane you run, and commands an AI agent runs to show you video or a live page there. Pepper never starts it either. | Separate program |
| The name and icon | "Pepper" in menus, About and alerts, its own app id and icon, so it can't be confused with Ghostty or replace it. | Text and images |
| The build | Builds with Zig 0.15.2 on Apple's newest SDKs, plus one fix from Ghostty's own later code. Signs Pepper and its player with macOS's hardened runtime, and checks what their signatures allow. | Build files only |

Nothing else changes. Pepper adds no new way to run a command, and it doesn't update itself.

Pepper follows Ghostty: each new Ghostty release, the same changes move onto it.

## Install

1. Download `Pepper.zip` from [Releases](https://github.com/erictran308/pepper/releases), unzip it, and drag `Pepper.app` to Applications.
2. Open it. macOS refuses the first time, because Pepper isn't notarized by Apple: it's signed ad hoc, without a paid Apple developer account. Open **System Settings → Privacy & Security**, scroll down to the message about Pepper, and click **Open Anyway**.
3. To play video, install mpv (and yt-dlp, for web addresses): `brew install mpv yt-dlp`.

It runs on Apple silicon Macs. It's built and tested on macOS 27; Ghostty itself runs on macOS 13 and later.

## Play video

In a Pepper tab:

```sh
pepper-play video.mp4
pepper-play 'https://www.youtube.com/watch?v=…'
```

The video fills the tab, with a status line at the bottom. mpv's keys work: space pauses, the arrows seek, `f` covers the status line too, and `q` quits and gives the screen back as it was. ⌘↩ makes Pepper's window full screen, and the video grows with it. It shows in macOS's Now Playing, and the media keys work.

[tuitube](https://github.com/erictran308/tuitube), a YouTube client for the terminal, plays its videos inside Pepper too: on the video's page, and in a small box while you browse.

Any program can do the same. [`pepper-play/README.md`](pepper-play/README.md) explains how, with examples in shell, Python and Rust.

## Pepper for AI agents

An AI coding agent (Claude Code, Codex, Gemini CLI, Aider, your own) can show you video, a cut it's editing, or the web app it's building, in Pepper and hear back what you point out. Open a split next to the agent (⌘D) and run:

```sh
pepper-agent view
```

That pane becomes the agent's screen. The agent opens a video or a page in it, tells you what to look at on the line at the bottom, and plays or clicks through what it wants you to see. You click anything that's wrong (or drag around it), type what's wrong with it, and press Enter when you're done. The agent gets where you clicked, a picture of it, and for a page the element itself: its selector, its styles, the source file it comes from. `m` marks a moment of a video; on a rough cut the agent is editing, it also learns which clip that is and where in its file, and changes the cut there. `y` approves what's shown, `x` rejects it: a demo the agent recorded of its page, or a before and after it put side by side. Several agents can share the pane, each labelled with its name, none driving another's. The agent sees what you see as pictures made from the file or the browser, never from your screen.

An agent finds out how with `pepper-agent guide`, and one line in its instructions tells it to look; or install it once as a skill (`pepper-agent install-skill`), or give it as MCP tools (`claude mcp add pepper -- pepper-agent mcp`). [`pepper-agent/README.md`](pepper-agent/README.md) has that line and the rest: the keys, the commands, how it works. It's built in stages: this one does video review, editing (a cut as a small JSON file, previewed with nothing rendered, then rendered with ffmpeg, or [video-use](https://github.com/browser-use/video-use)'s edit with its transcript: your marks come back with the words said there), finding a video's shot changes, pauses and words (transcribed on the Mac, nothing sent anywhere), live web pages, recordings, before and after, several agents at once, and live output (a file shown again whenever it's rendered again, frames a program writes, camera streams); devices come next.

## Security

A terminal sees everything you type and runs your commands, so what Pepper adds is kept small, and none of it can run anything:

- **The escape sequence only places a box.** It holds a token and four numbers: where the box goes, in cells; and up to four holes, four numbers each: cells of it the video leaves to the terminal, for a popup over it. No file names, no commands. Anything else is refused, and holes that don't read take the box away.
- **Pepper shows pictures, nothing more.** Its message port takes only frames already on the graphics chip, and the number of the one to show. Every message is checked for its size and kind, and the rest are dropped. A stream answers only to the program that started it, and goes when that program ends. There are limits on how many streams and boxes there can be (four streams a program), and a tab's output can use up only its own boxes. A program checks that the port is Pepper's before it sends a frame: the app's bundle id, and an answer the kernel says came from it.
- **A video shows only where it was placed, by the same random token.** A program must be able to write to that tab and know the token. Frames sent with any other token show nowhere, and a box never shows outside its tab.
- **The player runs on its own.** `pepper-play` is a separate program, started by you or by a program you run, with the same rights as any other. Pepper only tells your shell where it is.
- **An agent gets a viewer, not your screen.** `pepper-agent view` runs only when you start it, in a pane of yours. An agent's commands reach it through a socket only your user can open, and it does only what those commands say: open a file, a video's web address, a cut's preview or a page, play, seek, click and type in that page, take a frame from the file or the browser, record what a page shows or cut what played from the file, show a line of text. Nothing is passed on to mpv, and nothing an agent sends is run. What it shows the agent are files made from the video or the page, never a capture of the screen. A render never writes over your footage. An agent whose commands run in a sandbox gets through the viewer only what its sandbox lets it reach. Your keys are yours: text written into the pane, or a terminal's answer, is never taken for a key.
- **Pages are kept to this machine.** A page runs in a headless Chrome of the viewer's own, with a new profile (none of your cookies or logins) that's deleted after, reached over a pipe rather than a port. Unless the agent opens it with `--allow-remote`, the page and everything it loads reach only this machine's addresses, and the agent's JavaScript runs only in pages from here. A page from a file is served from its own folder, so it can't show your other files.
- **Transcripts stay on the Mac.** `pepper-agent transcribe` runs whisper.cpp here: no sound or text is sent anywhere. An agent given Pepper as MCP tools gets the same commands, nothing more: each call runs one, as its shell would.
- **A stream's password is never shown.** An agent gives it from an environment variable named for it (`PEPPER_CREDENTIALS_…`, which also names the host it's for: it's sent nowhere else) rather than on its command line, and the viewer takes it out of every answer, label, event, log and message it shows or writes (`***`); pepper-play gets the address over its IPC, never on a command line. The viewer checks servers' certificates.
- **A project's files stay files.** A playlist, a list of files or a link in a project can't make pepper-agent read other files of yours, and the tools it runs (ffmpeg, mpv, whisper.cpp, Chrome) start from `/`, so a project can't slip them a config file or a library.
- **Nothing a video says reaches your terminal raw.** A video's tags, subtitles, a playlist or a web page could hold escape sequences. From a shell, mpv prints nothing to the terminal: pepper-play draws the status line itself, from numbers, subtitles and mpv's on-screen text are drawn inside the video's box, and mpv's errors are shown with any control characters escaped. (Your own mpv options, such as `--term-title` or `--msg-level`, can turn mpv's terminal output back on.)
- **Signed to keep code out.** Pepper and pepper-play run with macOS's hardened runtime: no debugger can attach, and `DYLD_` variables can't put code in them. Pepper loads no libraries but its own and the system's.

The most a program can do with what Pepper adds: show a picture in a box of a tab it can already write to. It can't read the screen, type into a tab, or start anything.

In October 2026 everything Pepper adds, and tuitube's use of it, went through a strict security review. It found no way for terminal output or another program to run a command. It found seven weaknesses, all fixed before the first release:

1. pepper-play printed mpv's messages as they came, so a video's tags could send escape sequences to the terminal.
2. The build kept a debugging permission Xcode adds, so a debugger could attach to Pepper.
3. pepper-play ran without the hardened runtime.
4. Ghostty's updater, unused, kept macOS's library check off.
5. Output in one tab could use up the video boxes of every tab.
6. and 7. Two ways a local program could upset Pepper's bookkeeping of the Mach ports it holds.

A second review, of the changes after that release, found one more: from a shell, mpv's own status line also printed a video's subtitles, unescaped, when there was no video picture (a sound-only file). A subtitle could change the tab's title or, with Ghostty's default `clipboard-write = allow`, set the clipboard. Fixed: mpv now prints nothing to the terminal, as above.

A review of pepper-agent before its first release found four more, all fixed before its release:

1. A project could come with links in its `.pepper/` folder, and the viewer wrote through them: your marks could be added to any file of yours, a shell's startup file among them.
2. A camera's password could show in macOS's Now Playing.
3. `--human` printed the names in a page's own data raw.
4. A page kept to this machine could still reach link-local addresses on the network.

A third review, of everything Pepper adds as it stood after those (October 2026, from the source alone: it ran none of it), found 26 ways it might be misused that it couldn't confirm without running them. All are fixed as if real, not released yet:

1. Files in a project could get their own code run: a `yt-dlp.conf` when an agent opened a web video, a library when it transcribed. The tools now run from `/`.
2. A project's playlist, list of files or link, or a page from a file, could make pepper-agent show the agent other files of yours, under the name of the one it asked for; and the MCP server attached any picture a page's script named.
3. A program able to write the viewer's terminal could make it read your approval (`y`) from text or from a terminal's answer.
4. An agent in a sandbox could reach, through the viewer, what its sandbox kept from it; a command it ran could pose as the viewer, or lead the viewer's writes elsewhere.
5. The viewer played https and rtsps streams without checking the server's certificate, and `--credentials-env` read any variable.
6. A process could leave Pepper streams no one could end; a program could take a quit Pepper's port.
7. A page could slow or crash the viewer (a deep page, a large answer), another user could take the port a page's proxy used, and the viewer could use freed memory when a watched page closed.

Found a security problem in what Pepper adds? Please report it privately through [this repository's security advisories](https://github.com/erictran308/pepper/security/advisories/new), not in a public issue ([what's Pepper's](SECURITY.md)). Problems in Ghostty itself belong to [Ghostty](https://github.com/ghostty-org/ghostty/security).

## Build it yourself

Needs full Xcode (with its Metal Toolchain), Zig 0.15.2 exactly, Rust, and `brew install gettext`.

```sh
./pepper-build.sh        # → zig-out/Pepper.app
```

`pepper-build.sh` explains why plain `zig build` doesn't work with Apple's newest SDKs, and what it does about it.

## Credits and licence

Pepper is Ghostty, by Mitchell Hashimoto and Ghostty's contributors, under the MIT licence ([LICENSE](LICENSE)). All the credit for the terminal is theirs. Please don't report Pepper's problems to Ghostty: open them [here](https://github.com/erictran308/pepper/issues).

Pepper's own code is MIT too. pepper-play loads mpv's library at run time, which is GPL as Homebrew builds it ([details](pepper-play/README.md#licence)).

Ghostty's own README, with its documentation and roadmap: [github.com/ghostty-org/ghostty](https://github.com/ghostty-org/ghostty#readme).
