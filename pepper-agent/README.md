# pepper-agent

Lets AI coding agents (Claude Code, Codex, Gemini CLI, Aider, your own) use Pepper. The human runs a **viewer** in a Pepper pane; the agent, from its own shell, opens video, a cut it's editing or the web app it's building in it, steers it, and reads back what the human marked, pointed at and wrote. The agent sees the same pictures as files made from the source (a frame of the file, a screenshot from the browser, never a capture of the screen).

It comes inside Pepper (`Pepper.app/Contents/MacOS/pepper-agent`), on every Pepper shell's `PATH`. It needs ffmpeg (`brew install ffmpeg`), mpv's library to play (`brew install mpv`, as pepper-play does), and for pages a Chrome or Chromium (Google Chrome, Chromium, Edge, Brave, Playwright's, or `PEPPER_CHROME`).

## For the human

Open a split next to your agent (⌘D) and run:

```sh
pepper-agent view
```

The pane waits for an agent. When the agent opens something, it shows here, with a label over each picture, a line from the agent at the bottom (yellow), and the keys under it:

| Key | Does |
| --- | --- |
| click, drag | Point at something, or mark a region: the agent gets where, a picture of it, and for a page the element there (its selector, styles, source file). A prompt opens for a note: Enter sends it, Esc sends the point without |
| `m` | Mark this moment of a video for the agent |
| `i`, `o` | A range of a video: `i` at its start, `o` at its end, then a note ("cut this") |
| `n` | Write a note for the agent (a video pauses while you type; Enter sends, Esc drops) |
| `Enter` | Tell the agent you're done |
| `y`, `x` | Approve or reject what's shown (a cut, a render): the agent hears it |
| `space`, `,` `.`, `←` `→`, `↑` `↓` | A video: pause, a frame back or on, 5 s, 1 min (mpv's keys) |
| the bar under a video, the wheel | Click or drag the bar to go anywhere (on a cut, its clips are marked on it); the wheel over a video steps a frame a notch. Paused, the label gives the time to the millisecond and the frame number (on a cut, the clip and the time in its file) |
| `<`, `>` | A video: to your previous or next mark, note, point or range. Each shows as ◆ on the bar, a range as a span (on a cut that the agent revised, they keep their place in the footage; one on a moment that was cut out goes); paused on one, the bottom row shows what you wrote |
| `d`, then `d` again | Delete the mark, note, point or range you're paused on (`d` once asks, any other key keeps it); the agent is told |
| wheel, `↑` `↓`, Page Up/Down, `r` | A page: scroll, reload |
| `Tab`, `1`–`4` | Focus another source |
| `z` | Zoom the focused one to the whole pane, and back |
| `q` | Quit (asks first if anything is open) |

A page is shown as the agent drives it: your clicks point at things rather than click them. On a video, clicks on the picture point too; the bar under it is for seeking.

Several viewers can run, each in its own pane; an agent picks one with `--viewer <id>`. Your marks, points and notes are also kept in `.pepper/events.jsonl` in the folder the viewer started in (`--project DIR` for another), the points' pictures in `.pepper/points/`.

So that an agent knows about all this in your own projects, put this line in its instructions (`~/.claude/CLAUDE.md`, a project's `AGENTS.md`, …):

```text
If `pepper-agent` is on the PATH and the task involves video, a web UI, a mobile app, visual output or streams, run `pepper-agent guide` first.
```

## For agents

Run `pepper-agent guide`: everything is there ([guide.md](guide.md)). In short:

```sh
pepper-agent status                    # is there a viewer? what's next?
pepper-agent sheet video.mp4           # one PNG of 16 frames, with their times
pepper-agent open video.mp4            # shows it in the viewer, paused
pepper-agent say "Press m at anything to change, Enter when done"
pepper-agent events --wait 300 --until done --since 0
pepper-agent frames video.mp4 --at 12.48   # look at what was marked

pepper-agent edit validate cut.json    # a cut as a JSON project: every problem at once
pepper-agent edit preview cut.json     # the cut in the viewer, nothing rendered
pepper-agent events --wait 300 --until done --since 0    # marks give the clip and the time in its file
pepper-agent edit render cut.json --quality final -o cut.mp4
pepper-agent open edit/preview.mp4 --edit edit/edl.json    # with video-use: its render, marks map to its ranges
pepper-agent web open http://localhost:5173 --name app   # your dev server, live
pepper-agent say "Click anything that looks wrong, Enter when done"
pepper-agent events --wait 300 --until done --since 0    # points: selector, crop, note
```

Every command prints one JSON object; errors are JSON on stderr with a `code` and a `hint` to act on, and a non-zero exit. `--human` prints for people. `pepper-agent <command> --help` has an example.

| Command | Does |
| --- | --- |
| `guide`, `status`, `schema` | The guide; viewers, sources, what's installed, what to do next; JSON Schemas (the edit project, events, errors, answers) |
| `view` | The viewer (the human runs it) |
| `open`, `close`, `sources`, `layout`, `focus` | Sources in the viewer, how they share it |
| `snapshot` | A PNG of the frame a source shows, from the source |
| `say` | A line for the human |
| `events` | The human's marks, points, notes, approvals and done; long-polls |
| `play`, `pause`, `toggle`, `seek`, `loop`, `where`, `review` | Playback |
| `probe`, `frames`, `sheet` | A file's streams, frames and contact sheet, without a viewer |
| `edit validate`, `preview`, `timeline`, `render` | A cut as a JSON project: checked (all problems at once), shown in the viewer with nothing rendered, mapped between the cut's time and the files', rendered with ffmpeg (a quick draft on the graphics chip, or final) |
| `web open`, `goto`, `reload`, `back` | A page, live: a dev server's address or an HTML file, at a device's size (`--device iphone-15`) |
| `web click`, `type`, `press`, `scroll`, `hover`, `viewport`, `wait-for` | Drive it, in view of the human |
| `web shot`, `console`, `errors`, `dom`, `a11y`, `eval` | See it: a PNG from the browser, its console, exceptions and failed requests, HTML, accessibility tree, JavaScript |

Files it makes go in `.pepper/` in the current folder (`snapshots/`, `cache/`, `points/`, `edits/`, `renders/`), which gets a `.gitignore` of its own. Pictures are at most 1568 px on their long side.

## With video-use

[video-use](https://github.com/browser-use/video-use) is a skill that edits video for coding agents: it transcribes the footage (ElevenLabs Scribe, word by word), the agent writes an `edit/edl.json` of ranges, and its `helpers/render.py` makes the video (grade, subtitles, overlays, loudness). What it lacks is a place for the human to watch and point; pepper-agent is that, and reads its files as they are:

```sh
pepper-agent edit preview edit/edl.json                    # its cut, live, before any render
pepper-agent open edit/preview.mp4 --edit edit/edl.json    # render.py's output, tied to the EDL
pepper-agent events --wait 300 --until done --since 0      # marks: range, time in the source, words said
```

A mark on either says which range (`clip`, its `beat` as `clip_name`), the time in its source, and from `edit/transcripts/<name>.json` what was being said and the pauses either side (`said`), so a new cut edge goes in a silence, never inside a word (video-use's rule 6). `i` and `o` mark a range with both ends. `edit validate` checks the EDL and names problems in its own terms (`ranges[1].end`); `edit render` leaves rendering to `render.py`. `pepper-agent status` says whether video-use is installed (`~/.claude/skills/video-use`, `~/.codex/skills/`, `~/.agents/skills/`, or `VIDEO_USE_DIR`).

## How it works

```text
agent's shell                         Pepper pane (the human's)
pepper-agent open a.mp4 ──socket──▶   pepper-agent view
pepper-agent web click …                │ OSC 777;pepper;place (its own tty)
                                        │ pepper-play per video (IPC fd 3)
                                        │ headless Chrome for pages (pipe, fds 3 and 4)
                                        ▼
                                      Pepper shows the frames in the box
```

- An agent's command can't reach the terminal (its shell tool has no controlling tty: `notes/agent.md`), so only the viewer, which the human runs in the pane, places boxes. It runs one `pepper-play` per source as an app's player (`../pepper-play/README.md`, "From an app"), gives it the box's token over the IPC, and writes the escape sequences itself.
- Commands reach the viewer through `<user temp>/pepper-agent-<uid>/<id>.sock`: a folder only the user can open (0700), a socket of 0600, the peer's user checked. A request is one line of JSON (`{"v": 1, "cmd": …, "args": {…}}`, 64 KB at most) and gets one line back. Only the viewer's own commands are taken, never mpv's: nothing passes through to mpv, so no `run`, `load-script` or `subprocess`. `<id>.json` next to it says the viewer's pid, start, project and sources; both go when it ends, and stale ones are cleared.
- `open` takes an absolute path to a file, or an `http(s)` address; nothing else reaches mpv's `loadfile`, whose own protocols (`av://`, `edl://`, …) stay out.
- Pages: the viewer runs a headless Chrome of its own, with a new profile in its own folder (never yours), deleted when the last page closes. It talks to it over a pipe (`--remote-debugging-pipe`), not a port, so no other program can reach it. Each page has a browser context of its own whose proxy answers nothing but this machine's addresses (localhost, 127.0.0.1, ::1): neither the page nor what it loads reaches the network unless the agent opened it with `--allow-remote`, and `eval` runs only on this machine's pages. Downloads are refused. Chrome's screencast JPEGs are decoded by the graphics chip (VideoToolbox) into an IOSurface that goes to Pepper as pepper-play's frames do: 5 ms a frame, 40 ms from an agent's click to the screen (`../notes/agent.md`).
- What's at a point is worked out in a world of pepper-agent's own in the page, which the page's scripts can't change; the source file and line, when a dev build has them, only on this machine's pages.
- `snapshot` asks the viewer's player for the frame it shows (mpv's `screenshot-to-file … video`, the decoded frame), then shrinks it with ffmpeg. `probe`, `frames` and `sheet` run ffprobe and ffmpeg on the file (as `file:<path>`), cached by the file's fingerprint. Contact sheets are put together here, with times drawn in a small built-in font (Homebrew's ffmpeg has no text filter).
- An edit project is checked whole before anything uses it: every member it doesn't know, every time out of its file and every missing file is listed at once. `edit preview` writes it as an mpv EDL in `.pepper/edits/` (each file's path length-prefixed, so nothing in a name is read as EDL syntax) and opens that as a source, with the clips' spans: the viewer uses them to say where a time of the cut is in the footage. `edit render` runs ffmpeg (VideoToolbox's encoder for a draft, x264 or x265 for final) into a hidden file next to the output, renamed once done; it never writes over a clip's file or the project, nor over a file outside `.pepper/` without `--overwrite`.
- Text from outside (file names, tags, mpv's and ffmpeg's messages, a page's title, console and errors, the agent's `say`, the human's notes) is made inert before it's drawn: control and invisible characters are shown escaped, a row is cut to the pane, autowrap is off, and bracketed paste is on so a paste is never taken as keys. JSON output escapes them too.
- The keys passed to a source are a fixed list (pause, seeks, frame steps, speed, volume, tracks): never mpv's console, screenshots or quit. `i` and `o` (mpv's stats and progress) are the viewer's, for ranges.
- `said`, on events and answers: the transcript beside the edit or the footage (video-use's `edit/transcripts/<name>.json`: Scribe's word list, by the EDL's name for the source or the file's stem) read for the moment, by `pepper-agent` itself, not the viewer. Phrases break as video-use's `pack_transcripts.py` breaks them (0.5 s of silence, a new speaker); pauses are gaps of 0.15 s or more between words. Its text is made inert like the rest.

It's Rust with no dependencies but the system's (ffmpeg and pepper-play are run, not linked), MIT licensed like the rest of Pepper.

## Testing

```sh
cargo test                     # unit tests
tests/integration.py           # the viewer in a pseudo-terminal, driven as an agent and a human
```

The integration test runs the real viewer, pepper-play and mpv on generated clips (a hostile file name and tags among them) and Chrome on a local page (a hostile title and console), with a `PEPPER_PORT` no Pepper holds, so nothing shows: it checks the commands, the keys and the mouse, the events, what reaches the terminal, the pages kept to this machine, and the socket's refusals. Seeing the picture needs a test Pepper (`../CLAUDE.md`, Gotchas). `PEPPER_BENCH_JPEGS=<folder> cargo test --release draw_speed -- --ignored --nocapture` times the web frames' decoding.
