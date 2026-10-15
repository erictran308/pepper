#!/bin/sh
# Measures one clip, in a Pepper tab and in mpv's own window: measure.py.
# Usage: bench/measure.sh [options] CLIP [MPV-OPTION...]   (--help)
exec python3 -I "$(dirname "$0")/measure.py" "$@"
