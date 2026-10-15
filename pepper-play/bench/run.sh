#!/bin/sh
# The player's bench, in one command: builds this checkout's pepper-play,
# makes the test clips (clips.sh), and measures each clip in a Pepper box and
# in mpv's own window (measure.sh), then prints the table.
#
#   bench/run.sh [--new-pepper [APP]] [--seconds N] [--clips A,B,...]
#                [--label TEXT] [--no-window] [--no-idle] [--drag N]
#                [--env NAME=VALUE]... [--player PATH] [--look]
#                [-- MPV-OPTION...]
#
# Run it in a Pepper tab, or with --new-pepper from any terminal: that opens
# a Pepper of its own (APP, by default the repo's zig-out/Pepper.app), runs
# the bench in its one tab, and quits it after, so the Pepper you work in
# isn't measured or touched. Either way, leave the machine alone while it
# runs: each clip takes about twice --seconds (60) plus a few.
#
# Results: bench/results/<time>-<label>.jsonl (one JSON object a run) and
# .txt (the table). Clips by default: every clip clips.sh makes for the
# bench, and clips/real-1080p.* if there is one. --drag N first drags the
# box's size about for N seconds (measure.sh --drag); --env sets a variable
# for the players; --player measures that pepper-play instead of building
# this checkout's. --look runs the checks only eyes can make instead
# (look.py): a screenshot of colours, and a window drag.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
seconds=60
clips=1080p30-h264,1080p30-hevc,1080p60-h264,1080p60-hevc,2160p30-h264,2160p30-hevc,1080p24-h264
label=
new_pepper=
app=
window=
idle=yes
drag=
envs=
player=
look=
while [ "$#" -gt 0 ]; do
    case "$1" in
    --new-pepper)
        new_pepper=yes
        case "${2:-}" in --* | "") ;; *) app=$2 && shift ;; esac
        ;;
    --seconds) seconds=$2 && shift ;;
    --clips) clips=$2 && shift ;;
    --label) label=$2 && shift ;;
    --no-window) window=--no-window ;;
    --no-idle) idle= ;;
    --drag) drag=$2 && shift ;;
    --player) player=$2 && shift ;;
    --look) look=yes ;;
    --env)
        case "$2" in [A-Za-z_]*=*) ;; *) echo "run.sh: --env NAME=VALUE" >&2 && exit 2 ;; esac
        envs="$envs $2" && shift
        ;;
    --stamp) stamp=$2 && shift ;;
    --) shift && break ;;
    -h | --help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//' && exit 0 ;;
    *) echo "run.sh: unknown option $1 (--help)" >&2 && exit 2 ;;
    esac
    shift
done
mkdir -p "$here/results"
stamp=${stamp:-$(date +%Y%m%d-%H%M%S)${label:+-$(printf %s "$label" | tr -c 'A-Za-z0-9_.-' -)}}
out="$here/results/$stamp"

if [ -n "$new_pepper" ]; then
    app=$(cd "${app:-$here/../../zig-out/Pepper.app}" && pwd)
    # The tab's command: this script again, with the same PATH (an app opened
    # by LaunchServices gets launchd's), its output kept, then a mark that
    # it's done.
    dir=$(mktemp -d "${TMPDIR:-/tmp}/pepper-bench.XXXXXX")
    {
        echo '#!/bin/sh'
        printf 'export PATH=%s\n' "'$PATH'"
        for env in $envs; do printf "export '%s'\n" "$env"; done
        printf '%s' "exec >'$out.log' 2>&1; '$here/run.sh' --stamp '$stamp' --seconds '$seconds' --clips '$clips'"
        [ -n "$label" ] && printf " --label '%s'" "$label"
        [ -n "$window" ] && printf ' --no-window'
        [ -n "$idle" ] || printf ' --no-idle'
        [ -n "$drag" ] && printf " --drag '%s'" "$drag"
        [ -n "$player" ] && printf " --player '%s'" "$player"
        [ -n "$look" ] && printf ' --look'
        printf ' --'
        for option in "$@"; do printf " '%s'" "$option"; done
        printf "; echo \$? >'%s.done'\n" "$out"
    } >"$dir/bench"
    chmod 700 "$dir/bench"
    rm -f "$out.done"
    open -n "$app" --args --command="$dir/bench" --window-save-state=never \
        --quit-after-last-window-closed=true --confirm-close-surface=false \
        --window-width=160 --window-height=50
    # It opens its first window only once it's the active app.
    pid=
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
        pid=$(pgrep -n -f "^$app/Contents/MacOS/ghostty" || true)
        [ -n "$pid" ] && break
        sleep 0.5
    done
    [ -n "$pid" ] || { echo "run.sh: $app didn't start" >&2 && exit 1; }
    sleep 1
    osascript -l JavaScript -e "ObjC.import('AppKit');
        \$.NSRunningApplication.runningApplicationWithProcessIdentifier($pid).activateWithOptions(2)" \
        >/dev/null
    echo "run.sh: benching in Pepper $pid; log: $out.log" >&2
    while [ ! -f "$out.done" ] && kill -0 "$pid" 2>/dev/null; do sleep 2; done
    rm -rf "$dir"
    [ -f "$out.txt" ] && cat "$out.txt"
    status=$(cat "$out.done" 2>/dev/null || echo 1)
    [ "$status" = 0 ] || echo "run.sh: the bench failed ($status); see $out.log" >&2
    exit "$status"
fi

if [ -z "${PEPPER_PORT:-}" ] || [ -n "${TMUX:-}" ]; then
    echo "run.sh: run it in a Pepper tab (not tmux), or with --new-pepper" >&2
    exit 1
fi
if [ -z "$player" ]; then
    echo "run.sh: building pepper-play" >&2
    cargo build --release --locked --quiet --manifest-path "$here/../Cargo.toml"
    player=$here/../target/release/pepper-play
fi
if [ -n "$look" ]; then
    "$here/clips.sh" colors-1080p30-h264 1080p30-h264
    swiftc -O "$here/yuv-layer.swift" -o "$out-yuv-layer"
    python3 -I "$here/look.py" --player "$player" --yuv "$out-yuv-layer" --out "$out.json"
    exit 0
fi
names=$(printf %s "$clips" | tr , ' ')
# shellcheck disable=SC2086
"$here/clips.sh" $names
# What Pepper and WindowServer cost with nothing playing, to set the rest
# against.
for env in $envs; do export "${env?}"; done
[ -z "$idle" ] || "$here/measure.sh" --idle --seconds 20 --label "$label" --out "$out.jsonl"
for name in $names; do
    "$here/measure.sh" --seconds "$seconds" --label "$label" --out "$out.jsonl" $window \
        ${drag:+--drag "$drag"} --player "$player" \
        "$here/clips/$name.mp4" "$@"
done
for real in "$here"/clips/real-1080p.*; do
    [ -f "$real" ] || continue
    "$here/measure.sh" --seconds "$seconds" --label "$label" --out "$out.jsonl" $window \
        ${drag:+--drag "$drag"} --player "$player" \
        "$real" "$@"
done
python3 -I "$here/measure.py" --table "$out.jsonl" | tee "$out.txt"
