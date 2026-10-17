# Security

Pepper is a terminal, so a security problem in it can matter a lot. Thank you for reporting one.

## Reporting

Report a problem privately through [this repository's security advisories](https://github.com/erictran308/pepper/security/advisories/new), not in a public issue. Say what you did, what happened and on which macOS. A small example (a file, a few lines of shell or a short program) helps most.

Pepper is one person's project, with no bounty. Reports are read, answered and fixed as soon as they can be, and credited in the fix unless you'd rather not be named.

## What's Pepper's

Report here problems in what Pepper adds to Ghostty:

- the escape sequence `OSC 777;pepper`, and how Ghostty's core takes it to the app;
- the macOS app's video server, `macos/Sources/Features/Pepper/PepperServer.swift`, and its Mach port (`PEPPER_PORT`);
- `pepper-play`, the player inside `Pepper.app`;
- `pepper-agent`, inside `Pepper.app` too: the viewer the human runs for AI agents, its control socket, the headless Chrome it runs for pages, its pipes (FIFOs), and the files it writes (`.pepper/`);
- how Pepper is built and signed (`pepper-build.sh`), and the releases here.

Above all, anything that lets terminal output or another program run a command, read the screen or type into a tab, or show a picture where it wasn't placed; and anything that lets an agent's commands, or a video, a page, a file name or a note, do more through `pepper-agent` than its commands say (a page reaching the network when it wasn't opened with `--allow-remote`, among them), or a stream's password shown or written anywhere.

## What isn't

- **Ghostty itself**, the terminal Pepper is: report it to [Ghostty](https://github.com/ghostty-org/ghostty/security). If you aren't sure whether it's Ghostty's or Pepper's, report it here and it'll be passed on.
- **mpv, FFmpeg and yt-dlp**, which pepper-play uses: report them to those projects. How pepper-play uses what they give it (their messages, the files they open) is Pepper's.
- **tuitube**: [its own repository](https://github.com/erictran308/tuitube).

## Versions

Only the latest release is fixed. Each release follows a Ghostty release, with Pepper's changes moved onto it.
