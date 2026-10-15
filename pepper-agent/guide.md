# pepper-agent: a guide for AI agents

Pepper is a terminal (Ghostty, with video added) that plays video inside its
panes, straight from the graphics chip. `pepper-agent` lets you, an agent,
show the human video in Pepper, steer what they watch, and hear back what
they point out.

**You can't see Pepper's screen**, and nothing here lets you: frames never
come back through the terminal. So:

- the human watches in Pepper, in a pane called the **viewer**;
- you see the same pictures as files made from the source: `snapshot` (the
  frame a source shows now), `frames` and `sheet` (from the file);
- the human answers with **events**: marks (a moment), notes (text), done;
- you steer the viewer with commands, and tell the human what to do with `say`.

Every command prints one JSON object on stdout. Errors are JSON on stderr,
`{"error": {"code", "message", "hint"}}`, with a non-zero exit: do what the
hint says. `--human` prints for people; `<command> --help` has an example.

## Always first

```sh
pepper-agent status
```

- `viewer` is null: no viewer runs. **Never start one yourself**: your shell
  can't show video. Ask the human: *"Open a split in Pepper (Cmd-D) and run:
  pepper-agent view"*. Then run `status` again until `viewer` is there.
- `in_pepper` false only means your own shell isn't a Pepper tab; a viewer
  can still run, and `probe`, `frames` and `sheet` work anywhere.
- `deps` lists what's missing, with the install command. `next` says what to
  do now.
- If your commands run in a sandbox, it must let them connect to the Unix
  sockets in `socket_dir` (`status` says where).

## Reviewing a video with the human

```sh
pepper-agent probe video.mp4                  # length, size, codecs
pepper-agent sheet video.mp4                  # 16 frames on one PNG: look at it
pepper-agent open video.mp4                   # shows it, paused on frame one
pepper-agent say "Space plays. Press m at anything to change, n to add a note, Enter when done"
pepper-agent events --wait 300 --until done --since 0
```

`events --until done` waits until the human presses Enter, then answers with
every mark and note since `--since`. Your shell tool may stop a command
sooner than `--wait` (Claude Code's: after 2 minutes unless you give it a
longer timeout, 10 at most): give it one, or wait less and run it again.
Nothing is lost between calls. Each has `time` (seconds in the media)
and `time_text`. Look at a marked moment with `frames video.mp4 --at <time>`,
or `seek <time>` then `snapshot`. To ask about one part:

```sh
pepper-agent review 0:12 0:18                 # plays 12 s to 18 s, then pauses
pepper-agent say "Is the cut at 0:15 too early? n to answer, Enter when done"
pepper-agent events --wait 300 --until done --since <last_id>
```

Pass `--since` the `last_id` of the answer before, so you get only new events.
If `--wait` runs out with nothing, the error is `TIMEOUT`: ask again with
`say`, or wait again.

## Looking at footage yourself (no viewer needed)

- `sheet` first: the cheapest look at a whole file, or `--from`/`--to` a part.
- `frames --at` for the moments that matter, `--every` for a steady sample.
- Pictures are PNG files, at most 1568 px on the long side: read the path.
  Results are kept in `.pepper/cache/`, so asking again is free.

## Commands

| Command | Example | Does |
|---|---|---|
| `status` | `pepper-agent status` | Viewers, sources, what's installed, `next` |
| `guide` | `pepper-agent guide` | This guide |
| `open <file\|url>` | `pepper-agent open raw/a.mp4 --name a` | Shows a video, picture or video URL (yt-dlp), paused on its first frame (`--play` plays). Answers once it shows. The same `--name` again replaces what that box shows |
| `close <name>` | `pepper-agent close a` | Takes a source away |
| `sources` | `pepper-agent sources` | Each source: status, time, size, decoder |
| `layout <mode>` | `pepper-agent layout split-h` | `auto`, `single`, `split-h` (side by side), `split-v` (stacked), `grid` (up to 4) |
| `focus <name>` | `pepper-agent focus b` | The human's keys go to it; `single` shows it |
| `snapshot [name]` | `pepper-agent snapshot a --region 0,0,960,540` | PNG of the frame shown now, from the source; `--region X,Y,W,H` crops, in the source's pixels |
| `say <text>` | `pepper-agent say "Watch the logo at 0:42"` | A line for the human under the video; `say ''` clears it |
| `events` | `pepper-agent events --wait 300 --until done --since 3` | The human's marks, notes, done (below) |
| `play` / `pause` / `toggle` | `pepper-agent play --source a` | Playback |
| `seek <time>` | `pepper-agent seek 1:23.5` | `83.5`, `1:23.5`, `00:01:23.500`; `+5` and `-2` from where it is. Answers once that frame shows |
| `loop <from> <to>` | `pepper-agent loop 12 15` | Plays from..to over and over; `loop off` stops |
| `where [name]` | `pepper-agent where` | Time, length, paused, status |
| `review <from> [to]` | `pepper-agent review 1:02 1:09` | Plays that part (5 s with no end), then pauses |
| `probe <file>` | `pepper-agent probe a.mp4` | Length, streams, codecs, size, fps, keyframe interval, `hw_decodable` |
| `frames <file>` | `pepper-agent frames a.mp4 --at 12,1:03` | PNGs at those times (`--every 2 --from 0 --to 30`; 100 at most) |
| `sheet <file>` | `pepper-agent sheet a.mp4 --cols 4 --rows 4` | One PNG of evenly spaced frames, each with its time on it, and the times |

Commands act on the focused source unless given `--source NAME` (or a name,
for `snapshot` and `where`). With several viewers, add `--viewer <id>`.
A viewer shows 4 sources at most.

## Events

What the human does in the viewer, oldest first. Every event has `id`, `type`
and `at` (when, UTC). With a source in focus, also `source`, `kind`, `file`,
and the media `time` and `time_text`.

```json
{"id": 4, "type": "mark", "at": "2026-10-15T12:00:03.120Z", "source": "a", "kind": "media", "file": "/p/a.mp4", "time": 12.48, "time_text": "0:12.480"}
{"id": 5, "type": "note", "at": "2026-10-15T12:00:09.800Z", "text": "the logo jumps here", "source": "a", "kind": "media", "file": "/p/a.mp4", "time": 15.2, "time_text": "0:15.200"}
{"id": 6, "type": "done", "at": "2026-10-15T12:00:20.002Z"}
```

`events` options: `--wait SECONDS` (up to 3600) waits for one; `--until done`
waits for that type and returns all before it too; `--type mark,note` and
`--source NAME` filter; `--since ID` skips what you've seen. The answer has
`events`, `last_id` and `events_file` (`.pepper/events.jsonl` in the viewer's
project, every event ever, one JSON a line).

## Keys to tell the human

| Key | Does |
|---|---|
| `space`, `←` `→` | Pause, seek 5 s (`↑` `↓`: 1 min; `,` `.`: a frame) |
| `m` | Mark this moment (an event for you) |
| `n` | Write a note (playback pauses while they type; Enter sends, Esc drops) |
| `Enter` | Done: tells you they've finished (`--until done`) |
| `Tab`, `1`–`4` | Focus another source |
| `z` | Zoom the focused source to the whole pane, and back |
| `q` | Quit the viewer (asks first if anything is open) |

## Errors

| Code | Do this |
|---|---|
| `NO_VIEWER` | Ask the human to open a split in Pepper and run `pepper-agent view` |
| `NO_SOURCE` | `pepper-agent sources` for names; `open` it first |
| `NOT_FOUND` | Check the path (relative paths are from your shell's folder) |
| `LOAD_FAILED` | It couldn't be played: `probe` it; for a URL, is yt-dlp installed? |
| `LIMIT_REACHED` | Close a source, or ask for a second viewer and use `--viewer` |
| `TIMEOUT` | Nothing yet: wait again, or `say` something to the human |
| `BAD_TIME` | Times look like `83.5`, `1:23.5`, `00:01:23.500` (seek: `+5`, `-2`) |
| `BAD_ARGS` | See the hint's example, or `<command> --help` |
| `MISSING_DEP` | Install what the message names (`brew install ffmpeg`, `brew install mpv`) |
| `UNSUPPORTED` | Not in this version (web pages, streams, devices come later) |
| `NOT_PLAYABLE` | That has no picture or no timeline |
| `FAILED` | Read the message; `status` shows the state |

## Rules

- Never start the viewer, and never try to show video from your own shell:
  ask the human.
- Never change source media in place. Keep what you make under `.pepper/` or
  where the human says.
- Snapshots and frames are the human's: don't upload them anywhere without
  asking.
- Prefer showing (`open`, `review`) to rendering new files.
- Ask before deleting anything.
- Don't put passwords in commands; they're logged.
