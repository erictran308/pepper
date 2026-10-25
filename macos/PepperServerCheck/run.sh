#!/bin/sh
# Checks macos/Sources/Features/Pepper/PepperServer.swift without the app:
# builds it with this folder's program, which runs the server under a port
# name of its own (pepper.server-check.<pid>, so it never meets a running
# Pepper) and copies of itself as its clients, and runs the checks in
# main.swift. Nothing is shown on screen.
#
# Usage: macos/PepperServerCheck/run.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
build=$(mktemp -d "${TMPDIR:-/tmp}/pepper-server-check.XXXXXX")
trap 'rm -rf "$build"' EXIT
/usr/bin/xcrun swiftc -O -o "$build/check" \
    "$here/main.swift" "$here/Stubs.swift" \
    "$here/../Sources/Features/Pepper/PepperServer.swift" \
    -Xlinker -sectcreate -Xlinker __TEXT -Xlinker __info_plist -Xlinker "$here/Info.plist"
env -u PEPPER_PORT -u PEPPER_PLAY -u PEPPER_FEATURES "$build/check"
