# pepper-play

Pepper's video player. It plays a file or URL inside Pepper's terminal, straight from the graphics chip: libmpv draws each frame into an IOSurface, and Pepper shows it over a box of cells with no copies. It comes inside Pepper (`Pepper.app/Contents/Helpers/Pepper.app`), and Pepper tells every shell where it is.

It needs mpv's library: `brew install mpv`.

## From a shell

```sh
pepper-play video.mp4
pepper-play 'https://www.youtube.com/watch?v=…'     # through yt-dlp, as in mpv
pepper-play --start=1:30 --volume=50 video.mp4    # any of mpv's options
```

The video takes the terminal (the alternate screen, as a full-screen program does) but for its last row, where mpv's status line is, and follows the window as it resizes. mpv's keys work: `q` quits, space pauses, the arrows seek. When it ends, the terminal is as it was.

Pepper puts `pepper-play` on every shell's `PATH`. A shell that sets its own `PATH` from scratch can run `"$PEPPER_PLAY"`.

## From an app

An app shows video in a box of its own, laid out among its text, and controls playback over mpv's JSON IPC. pepper-play acts as an app's player when it gets a token (`PEPPER_TOKEN`) or an IPC option (`--input-ipc-client`, `--input-ipc-server`): then it leaves the screen and the keys alone.

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

1. **The app writes them.** It makes the token, gives it to pepper-play in `PEPPER_TOKEN` (or over the IPC, `script-message-to main pepper-token TOKEN`), and writes the sequences with the rest of its output. This suits a full-screen app, which redraws often and knows when the box moves.
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
| `PEPPER_TOKEN` | The token the app places the box with. pepper-play takes it out of its environment at once, so nothing it starts (yt-dlp) gets it. |
| `PEPPER_LIBMPV` | Where libmpv is, if not in Homebrew's or MacPorts' `lib` (`/opt/homebrew`, `/usr/local`, `/opt/local`). |
| `PEPPER_PLAY_DEBUG` | Set: mpv's messages from verbose up on stderr, not only its warnings. |

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
    [play, f"--input-ipc-client=fd://{theirs.fileno()}", sys.argv[1]],
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
    let status = Command::new(play).env("PEPPER_TOKEN", &token).arg(file).status()?;
    write!(out, "\x1b]777;pepper;remove;{token}\x1b\\")?;
    out.flush()?;
    std::process::exit(status.code().unwrap_or(1));
}
```

## Without pepper-play

A program can also hand Pepper frames itself, without mpv. It looks up `PEPPER_PORT` with `bootstrap_look_up`, makes BGRA IOSurfaces, and sends Mach messages. Each starts with the token, NUL padded to 64 bytes:

| Message | `msgh_id` | Body after the header |
| --- | --- | --- |
| attach | `0x70700001` | 1 to 4 IOSurfaces (`IOSurfaceCreateMachPort`) as port descriptors, then the token. `msgh_local_port` is a receive right of the program's own, sent as `MAKE_SEND`: Pepper watches it to learn when the program ends, and answers on it. |
| present | `0x70700002` | The token, then the surface to show, a `UInt32` from 0. |
| detach | `0x70700003` | The token. |
| size, from Pepper | `0x70700101` | The token, then the box's width and height in pixels and whether it's placed (`UInt32`s). Sent when the stream and its box both exist, and when the box moves, resizes or goes. |

Draw each frame into a surface the window server isn't showing (`IOSurfaceIsInUse`), wait for it to finish, then present it. `src/pepper.rs` is a client in Rust, and `macos/Sources/Features/Pepper/PepperServer.swift` is Pepper's side.

## Licence

pepper-play's source is MIT, as the rest of Pepper. libmpv, which it loads at run time, is GPL as Homebrew builds it, so a pepper-play running with it is under the GPL's terms. An app that drives it over the IPC isn't linked with it.
