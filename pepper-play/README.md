# pepper-play

Pepper's video player. It plays a file or URL inside Pepper's terminal, straight from the graphics chip: libmpv draws each frame into an IOSurface, and Pepper shows it over a box of cells with no copies. It comes inside Pepper (`Pepper.app/Contents/Helpers/Pepper.app`), and Pepper tells every shell where it is.

It needs mpv's library: `brew install mpv`.

## From a shell

```sh
pepper-play video.mp4
pepper-play 'https://www.youtube.com/watch?v=…'     # through yt-dlp, as in mpv
pepper-play --start=1:30 --volume=50 video.mp4    # any of mpv's options
pepper-play -- "$name"                            # all after -- is a file or URL
```

Put `--` before a name you didn't write yourself: one that starts with `--` would otherwise be taken as one of mpv's options, and some of those run commands.

The video takes the terminal (the alternate screen, as a full-screen program does) but for its last row, where the status line is (time, length, paused), and follows the window as it resizes. mpv's keys work: `q` quits, space pauses, the arrows seek, and `f` makes the video fill the whole tab, over the status line (again to bring it back). For the whole screen, make Pepper's window full screen too (⌘↩). When it ends, the terminal is as it was.

mpv prints nothing to the terminal: its messages hold the video's tags, file names and URLs, and its own status line would carry subtitles and on-screen text, all of which could hold escape sequences. pepper-play draws the status line itself, from numbers only, and keeps a video output even for sound alone (a black box), so subtitles and mpv's on-screen text are drawn in the box. If something couldn't play, mpv's errors are shown once the screen is back, with control and invisible format characters escaped (`\u{1b}`, `\u{202e}`). Your own options can turn mpv's terminal output back on (`--msg-level` turns on only the modules it names, `--term-osd=force`, `--term-title`); don't put text from a video in them. As in mpv, a web address goes through yt-dlp, which reads a `yt-dlp.conf` in the folder pepper-play runs from: run it from a folder of yours.

Pepper puts `pepper-play` on every shell's `PATH`. A shell that sets its own `PATH` from scratch can run `"$PEPPER_PLAY"`.

## From an app

An app shows video in a box of its own, laid out among its text, and controls playback over mpv's JSON IPC. pepper-play acts as an app's player when it gets a token (`PEPPER_TOKEN`) or an IPC option (`--input-ipc-client`, `--input-ipc-server`): then it leaves the screen and the keys alone, and writes mpv's warnings to its stderr, with any control characters escaped. It also loads no cover art of its own (`cover-art-auto=no`; an app's option wins).

### Is this Pepper?

Pepper sets these in every shell:

| Variable | What it is |
| --- | --- |
| `PEPPER_PORT` | Pepper's Mach port, by name. Set: the terminal is Pepper. |
| `PEPPER_PLAY` | pepper-play's path. |

Inside tmux they're set too, but tmux keeps Pepper's escape sequences from the terminal: check that `TMUX` isn't set.

### Placing the box

Pepper shows a stream only where it was placed, by an escape sequence written to the terminal:

```text
ESC ] 777 ; pepper ; place ; TOKEN ; COL ; ROW ; COLS ; ROWS ESC \
ESC ] 777 ; pepper ; remove ; TOKEN ESC \
```

- `COL`, `ROW`: the box's top left cell, from 0. `COLS`, `ROWS`: its size in cells. Place it again to move or resize it.
- `TOKEN`: 1 to 64 of `A-Z a-z 0-9 _ -`; use a random one per playback (32 hex digits of `/dev/urandom`). Only a program that can write to the tab and knows the token can put video there, so keep it off command lines and out of logs.
- The video is drawn at the box's size in pixels, which Pepper tells pepper-play; the app never works out pixels. The picture keeps its shape, with black bars where the box's shape differs.
- Other terminals ignore both sequences.

There are two ways to do it:

1. **The app writes them.** It makes the token, gives it to pepper-play over the IPC (`script-message-to main pepper-token TOKEN`, best: a program's environment can be read by the user's other programs, `ps -E`) or in `PEPPER_TOKEN`, and writes the sequences with the rest of its output. This suits a full-screen app, which redraws often and knows when the box moves.
2. **pepper-play writes them**, to `/dev/tty`, when the app asks over the IPC:

   ```text
   script-message-to main pepper-place COL ROW COLS ROWS
   script-message-to main pepper-remove
   ```

   pepper-play makes its own token unless it was given one, and takes the box away when it ends.

A box drawn over by a popup should be taken away while the popup is open, and placed again after.

### Controlling it

pepper-play takes mpv's options and its [JSON IPC](https://mpv.io/manual/stable/#json-ipc): `loadfile`, `set pause yes`, `seek`, `observe_property`, `quit`, and the rest.

- `--input-ipc-client=fd://N`: a socket the app passes as file descriptor `N` (a socketpair). pepper-play quits when the app closes it.
- `--input-ipc-server=PATH`: a socket pepper-play makes at `PATH`. Put it in a folder only you can open.

Files and URLs on the command line play at once, and pepper-play quits after them, as mpv does. With none, it waits for `loadfile` (`--idle`).

### Environment

| Variable | What it does |
| --- | --- |
| `PEPPER_TOKEN` | The token the app places the box with. pepper-play takes it out of its environment at once, so nothing it starts (yt-dlp) gets it. Giving it over the IPC is better still. |
| `PEPPER_LIBMPV` | libmpv's absolute path, if it isn't in Homebrew's or MacPorts' `lib` (`/opt/homebrew`, `/opt/local`, `/usr/local`). The library, and every folder above it, must belong to you or root and not be writable by others, and it mustn't be a download (quarantined). |
| `PEPPER_PLAY_DEBUG` | Set: mpv's messages from verbose up on stderr, not only its warnings, and at the end what it drew and showed (frames, surfaces made, time waiting on the graphics chip, the decoder). |

### Examples

Python, with a socketpair, asking pepper-play to place the box:

```python
#!/usr/bin/env python3
"""Plays a video in 60×20 cells at the top left of the terminal."""
import json, os, socket, subprocess, sys

play = os.environ.get("PEPPER_PLAY")
if not play or "TMUX" in os.environ:
    sys.exit("run this in pepper (not in tmux)")

ours, theirs = socket.socketpair()
player = subprocess.Popen(
    [play, f"--input-ipc-client=fd://{theirs.fileno()}", "--", sys.argv[1]],
    pass_fds=[theirs.fileno()],
)
theirs.close()

def send(*command):
    ours.sendall(json.dumps({"command": list(command)}).encode() + b"\n")

send("script-message-to", "main", "pepper-place", "0", "0", "60", "20")
player.wait()
```

Rust, writing the escape sequences itself:

```rust
use std::io::{Read, Write};
use std::process::Command;

fn main() -> std::io::Result<()> {
    let play = std::env::var_os("PEPPER_PLAY").expect("run this in pepper");
    let file = std::env::args().nth(1).expect("a file or URL to play");
    let mut random = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let token: String = random.iter().map(|b| format!("{b:02x}")).collect();

    // 60×20 cells at the top left.
    let mut out = std::io::stdout();
    write!(out, "\x1b]777;pepper;place;{token};0;0;60;20\x1b\\")?;
    out.flush()?;
    let status = Command::new(play)
        .env("PEPPER_TOKEN", &token)
        .arg("--")
        .arg(file)
        .status()?;
    write!(out, "\x1b]777;pepper;remove;{token}\x1b\\")?;
    out.flush()?;
    std::process::exit(status.code().unwrap_or(1));
}
```

## Performance

pepper-play sets these among its own options, before yours, so yours win (it reads no `mpv.conf`, as libmpv doesn't):

| Default | What it does | To turn it off |
| --- | --- | --- |
| `--hwdec=auto-safe` | Decodes on the graphics chip (VideoToolbox) what it can: H.264, HEVC, ProRes, VP9, and AV1 on M3 and later; the CPU decodes the rest. mpv hands the decoded frames to its render pass with no copy. A video from the web is then parsed by Apple's decoder, as in mpv's own window with this setting or in Safari. | `--hwdec=no` |
| `--video-sync=display-resample` | Shows each frame at one of the display's refreshes and plays the sound a hair faster or slower to match, as mpv's own window can: a steady cadence, a 24 fps film too. pepper-play tells mpv the display's rate (`--display-fps-override`, the main display's). | `--video-sync=audio` |

And, with no option: frames are drawn into a ring of four IOSurfaces tagged sRGB, so a wide-gamut display shows mpv's colours as they are; a frame that only repeats the one on screen isn't drawn again; new surfaces aren't made at every size while a window is being dragged, but once the size settles.

Measured on an Apple M5 Pro (macOS 27.2, mpv 0.41.0), test clips in a 1926×1058 px box, 60 s each: CPU as a share of one core, and memory as Activity Monitor counts it. mpv's own window is the same size with the same settings (`--hwdec=auto-safe --video-sync=display-resample`).

| Clip | pepper-play before these defaults | pepper-play | mpv's window | pepper-play memory | mpv's window memory |
| --- | --- | --- | --- | --- | --- |
| 1080p24 H.264 | 26.4% | 9.2% | 17.8% | 304 MB | 691 MB |
| 1080p30 H.264 | 33.2% | 10.0% | 20.8% | 305 MB | 761 MB |
| 1080p60 H.264 | 48.7% | 17.8% | 24.6% | 305 MB | 1000 MB |
| 2160p30 HEVC | 69.9% | 12.4% | 19.6% | 565 MB | 910 MB |

`bench/run.sh --new-pepper` measures it on yours, in a Pepper of its own (`bench/run.sh --help`); `notes/player-perf.md` has every number and what each change did.

## Without pepper-play

A program can also hand Pepper frames itself, without mpv. It looks up `PEPPER_PORT` with `bootstrap_look_up`, checks that it's Pepper's (below), makes BGRA IOSurfaces (4 bytes a pixel), and sends Mach messages. Each starts with the token, NUL padded to 64 bytes:

| Message | `msgh_id` | Body after the header |
| --- | --- | --- |
| attach | `0x70700001` | 1 to 4 IOSurfaces (`IOSurfaceCreateMachPort`) as port descriptors, then the token. `msgh_local_port` is a receive right of the program's own, sent as `MAKE_SEND`: Pepper watches it to learn when the program ends, and answers on it. A stream also ends when the process that attached it exits or runs another program, and a process has at most 4. |
| present | `0x70700002` | The token, then the surface to show, a `UInt32` from 0. |
| detach | `0x70700003` | The token. |
| hello | `0x70700004` | Any token, with `msgh_local_port` a send-once right of the program's (`MAKE_SEND_ONCE`). Pepper answers at once. |
| size, from Pepper | `0x70700101` | The token, then the box's width and height in pixels, whether it's placed, and the display it's on (a `CGDirectDisplayID`, 0 if not known) (`UInt32`s; an older Pepper sends no display). Sent when the stream and its box both exist, and when the box moves, resizes or goes, or its window goes to another display. |
| refused, from Pepper | `0x70700102` | The token, when an attach isn't taken (too many streams, a surface it can't show): offer the surfaces again later. |
| hello's answer, from Pepper | `0x70700103` | The token. |

The port's name is `io.github.erictran308.pepper.<pid>`: before sending frames, a program checks that the pid runs `<X>.app/Contents/MacOS/ghostty` in an app whose bundle id is the name's, then says hello and asks for the answer's audit trailer. The answer must come from that pid, and from the same program (the pid's version); after that, it takes size reports only from that process. Someone else holding the name, a quit Pepper's pid taken by another program, never gets a frame.

Draw each frame into a surface the window server isn't showing (`IOSurfaceIsInUse`), wait for it to finish, then present it. `src/pepper.rs` is a client in Rust, and `macos/Sources/Features/Pepper/PepperServer.swift` is Pepper's side.

## Licence

pepper-play's source is MIT, as the rest of Pepper. libmpv, which it loads at run time, is GPL as Homebrew builds it, so a pepper-play running with it is under the GPL's terms. An app that drives it over the IPC isn't linked with it.
