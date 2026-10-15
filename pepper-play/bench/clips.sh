#!/bin/sh
# Makes the bench's test clips in bench/clips/ (kept out of git), with
# ffmpeg: 70 s each, so a 60 s measurement fits after the start.
#
#   1080p30-h264  1080p30-hevc   the common case
#   1080p60-h264  1080p60-hevc   twice the frames
#   2160p30-h264  2160p30-hevc   four times the pixels
#   1080p24-h264                 film's rate on a 60 Hz display (pacing)
#   counter-1080p60-h264         a frame number and a bar that moves a step
#                                each frame: a torn frame shows two of them
#   colors-1080p30-h264          sRGB colour patches, tagged BT.709 (10.6)
#
# Pictures are lavfi's testsrc2, tagged BT.709 limited range; sound a quiet
# sine. A real 1080p file can go next to them as clips/real-1080p.mp4 (any
# container works if named so); run.sh measures it too.
#
# Usage: bench/clips.sh [NAME...]   (default: all; existing ones are kept)
set -eu
cd "$(dirname "$0")"
mkdir -p clips
command -v ffmpeg >/dev/null || {
    echo "clips.sh: needs ffmpeg (brew install ffmpeg)" >&2
    exit 1
}

seconds=70
tags="-color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv"
to709="scale=out_color_matrix=bt709:out_range=tv,format=yuv420p"

# make NAME SOURCE-FILTER VIDEO-CODEC-OPTIONS
make() {
    out="clips/$1.mp4"
    [ -f "$out" ] && return 0
    echo "clips.sh: making $out" >&2
    # shellcheck disable=SC2086
    ffmpeg -hide_banner -loglevel error -y \
        -f lavfi -i "$2" \
        -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$seconds" \
        -vf "$to709" -af "volume=0.1" -t "$seconds" \
        $3 $tags -c:a aac -b:a 128k -movflags +faststart \
        "$out.tmp.mp4"
    mv "$out.tmp.mp4" "$out"
}

h264="-c:v libx264 -preset veryfast -crf 20 -g 120"
hevc="-c:v libx265 -preset fast -crf 22 -x265-params log-level=error:keyint=120 -tag:v hvc1"

names="$*"
pick() {
    [ -z "$names" ] && return 0
    case " $names " in *" $1 "*) return 0 ;; esac
    return 1
}

pick 1080p30-h264 && make 1080p30-h264 "testsrc2=size=1920x1080:rate=30" "$h264"
pick 1080p30-hevc && make 1080p30-hevc "testsrc2=size=1920x1080:rate=30" "$hevc"
pick 1080p60-h264 && make 1080p60-h264 "testsrc2=size=1920x1080:rate=60" "$h264"
pick 1080p60-hevc && make 1080p60-hevc "testsrc2=size=1920x1080:rate=60" "$hevc"
pick 2160p30-h264 && make 2160p30-h264 "testsrc2=size=3840x2160:rate=30" "$h264"
pick 2160p30-hevc && make 2160p30-hevc "testsrc2=size=3840x2160:rate=30" "$hevc"
pick 1080p24-h264 && make 1080p24-h264 "testsrc2=size=1920x1080:rate=24000/1001" "$h264"

# The frame number in binary, twice: 12 boxes along the top and the same
# along the bottom, white for a 1 bit (the lowest on the left); and a white
# bar 1/60 of the width wide that moves one bar's width each frame and wraps
# each second. A torn frame has top and bottom numbers that differ, or the
# bar in two places. (Homebrew's ffmpeg has no drawtext.)
# (drawbox can't move with time; overlay can.)
counter="color=c=black:size=1920x1080:rate=60[bg];\
color=c=white:size=32x680:rate=60[bar];\
[bg][bar]overlay=x='mod(n,60)*32':y=200:eval=frame"
for bit in 0 1 2 3 4 5 6 7 8 9 10 11; do
    for y in 40 940; do
        counter="$counter,drawbox=x=$((60 + bit * 150)):y=$y:w=100:h=100:\
color=white:t=fill:enable='eq(mod(floor(n/pow(2,$bit)),2),1)'"
    done
done
counter="$counter[out0]"
pick counter-1080p60-h264 && make counter-1080p60-h264 "$counter" "$h264"

# Six sRGB patches (primaries, secondaries) over mid grey: what Pepper shows
# should measure as what mpv's own window shows.
colors="color=c=0x808080:size=1920x1080:rate=30,format=rgb24,\
drawbox=x=160:y=240:w=400:h=300:color=0xFF0000:t=fill,\
drawbox=x=760:y=240:w=400:h=300:color=0x00FF00:t=fill,\
drawbox=x=1360:y=240:w=400:h=300:color=0x0000FF:t=fill,\
drawbox=x=160:y=600:w=400:h=300:color=0x00FFFF:t=fill,\
drawbox=x=760:y=600:w=400:h=300:color=0xFF00FF:t=fill,\
drawbox=x=1360:y=600:w=400:h=300:color=0xFFFF00:t=fill"
pick colors-1080p30-h264 && make colors-1080p30-h264 "$colors" "$h264"
exit 0
