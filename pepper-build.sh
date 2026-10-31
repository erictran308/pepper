#!/bin/sh
# Builds Pepper: Ghostty's build (Zig 0.17.0, which links against Apple's
# 26.4+ SDKs as they are), then pepper-play (pepper-play/, Rust), put in the
# app as Contents/Helpers/Pepper.app, and pepper-agent (pepper-agent/, Rust)
# as Contents/MacOS/pepper-agent, and pepper-update (pepper-update/, a shell
# script). It signs them all with Pepper's own certificate (or ad hoc, see
# below), with the hardened runtime and entitlements of their own
# (pepper-agent: none), and checks what the signatures say.
#
# Usage: ./pepper-build.sh [zig build options]   (default: -Doptimize=ReleaseFast)
#
# PEPPER_SIGN_IDENTITY: the name of the certificate to sign with (default:
# "Pepper Code Signing", self-signed, in the login keychain), or `-` to sign
# ad hoc, which a build without that certificate must ask for.
set -eu
cd "$(dirname "$0")"

# Pepper's version: Ghostty's, then Pepper's own count on it. The build
# number counts Pepper's releases.
pepper_version=1.4.0-pepper.1
pepper_build=5

zig_want=0.17.0
zig_have=$(zig version 2>/dev/null || true)
if [ "$zig_have" != "$zig_want" ]; then
    echo "pepper-build: needs zig $zig_want, found '${zig_have:-none}'" >&2
    exit 1
fi

# The signing identity, checked before anything is built. macOS keeps what
# it lets Pepper do (Files and Folders, Accessibility, App Management)
# against the signature's designated requirement: with a certificate that
# names the certificate, so the next build keeps it; ad hoc it names the
# build, so each build asks again, and pepper-update can't tell Pepper's own
# builds from anyone's. So no build is signed ad hoc unless asked to be.
identity=${PEPPER_SIGN_IDENTITY:-Pepper Code Signing}
if [ "$identity" != - ]; then
    case "$(security find-identity -p codesigning 2>/dev/null)" in
    *"\"$identity\""*) ;;
    *)
        echo "pepper-build: no \"$identity\" code-signing certificate in the keychain (CLAUDE.md, Building). PEPPER_SIGN_IDENTITY=- signs ad hoc: macOS then asks for Pepper's permissions again with each build, and pepper-update won't take it." >&2
        exit 1
        ;;
    esac
fi

[ "$#" -gt 0 ] || set -- -Doptimize=ReleaseFast
# The build copies the app over the one already in zig-out, keeping files
# the new build no longer has; so the old one goes first.
app=yes
case " $* " in
*emit-macos-app=false*) app=no ;;
*) rm -rf zig-out/Pepper.app ;;
esac
zig build "$@"

# Built without the app (-Demit-macos-app=false): nothing to put it in.
[ "$app" = yes ] || exit 0
app=zig-out/Pepper.app
[ -d "$app" ] || exit 0

echo "pepper-build: building pepper-play" >&2
cargo build --release --locked --manifest-path pepper-play/Cargo.toml
helper="$app/Contents/Helpers/Pepper.app"
rm -rf "$app/Contents/Helpers"
mkdir -p "$helper/Contents/MacOS" "$helper/Contents/Resources"
cp pepper-play/Info.plist "$helper/Contents/Info.plist"
cp pepper-play/target/release/pepper-play "$helper/Contents/MacOS/pepper-play"
version=$("$helper/Contents/MacOS/pepper-play" --version | cut -d' ' -f2)
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$helper/Contents/Info.plist"
# The app's icon, for Now Playing.
icon=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconFile' "$app/Contents/Info.plist" 2>/dev/null || true)
if [ -n "$icon" ] && [ -f "$app/Contents/Resources/$icon.icns" ]; then
    cp "$app/Contents/Resources/$icon.icns" "$helper/Contents/Resources/pepper.icns"
fi

echo "pepper-build: building pepper-agent" >&2
cargo build --release --locked --manifest-path pepper-agent/Cargo.toml
# On every shell's PATH, next to pepper-play.
cp pepper-agent/target/release/pepper-agent "$app/Contents/MacOS/pepper-agent"
agent_version=$("$app/Contents/MacOS/pepper-agent" --version | cut -d' ' -f2)

# pepper-update, a script: in Resources, where the app's seal covers it
# (a script in Contents/MacOS would carry its signature in extended
# attributes, which the release zip leaves out), and on every shell's PATH
# by a link in Contents/MacOS.
cp pepper-update/pepper-update "$app/Contents/Resources/pepper-update"
chmod 755 "$app/Contents/Resources/pepper-update"
ln -sf ../Resources/pepper-update "$app/Contents/MacOS/pepper-update"

# What About and Finder say (the Xcode project has a placeholder).
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $pepper_version" \
    -c "Set :CFBundleVersion $pepper_build" "$app/Contents/Info.plist"

# On every shell's PATH: Pepper adds Contents/MacOS to it. A link, which
# pepper-play follows to run from its bundle.
ln -sf ../Helpers/Pepper.app/Contents/MacOS/pepper-play "$app/Contents/MacOS/pepper-play"

# Both with the hardened runtime, which ignores DYLD_* variables and keeps
# debuggers out. The helper turns off library validation to load Homebrew's
# libmpv, signed by someone else (mpv.rs checks the file), and allows
# executable memory for mpv's Lua scripts: LuaJIT maps its code without
# MAP_JIT, so allow-jit isn't enough. The app gets its own entitlements, not
# the ones Xcode signs a local build with, which add get-task-allow (a
# debugger may attach).
codesign --force --sign "$identity" --timestamp=none --options runtime \
    --entitlements pepper-play/pepper-play.entitlements "$helper"
# pepper-agent loads nothing and makes no code: no entitlements at all.
codesign --force --sign "$identity" --timestamp=none --options runtime \
    "$app/Contents/MacOS/pepper-agent"
# Xcode's plugins (the dock tile), which its local build signs ad hoc: with
# the app's identity too, entitlements kept (the dock tile has none).
for plugin in "$app"/Contents/PlugIns/*; do
    [ -e "$plugin" ] || continue
    codesign --force --sign "$identity" --timestamp=none --options runtime \
        --preserve-metadata=entitlements "$plugin"
done
codesign --force --sign "$identity" --timestamp=none --options runtime \
    --entitlements macos/GhosttyReleaseLocal.entitlements "$app"
codesign --verify --deep --strict "$app"

# codesign --verify checks the seal, not what the signatures allow: these
# must hold for every build that may be released.
fail() {
    echo "pepper-build: $*" >&2
    exit 1
}
# A signature's entitlements, as XML (none: nothing). codesign failing fails
# the build, rather than passing what it couldn't read.
entitlements() {
    codesign -d --entitlements - --xml "$1" 2>/dev/null ||
        fail "codesign can't read $1's signature"
}
nl='
'
# Every program in the app, nested ones too: the app's, pepper-play's,
# pepper-agent's and Xcode's dock tile plugin today, and any plugin, service
# or helper a later build adds.
programs=$(find "$app" -type f -exec file -N -F ' :: ' -- {} + | sed -n 's/ :: *Mach-O.*$//p')
for known in "$app/Contents/MacOS/ghostty" "$helper/Contents/MacOS/pepper-play" \
    "$app/Contents/MacOS/pepper-agent"; do
    case "$nl$programs$nl" in
    *"$nl$known$nl"*) ;;
    *) fail "$known isn't among the app's programs" ;;
    esac
done
case "$identity" in
-) flags='flags=0x10002(adhoc,runtime)' signer='Signature=adhoc' ;;
*) flags='flags=0x10000(runtime)' signer="Authority=$identity" ;;
esac
while IFS= read -r code <&3; do
    signature=$(codesign -d --verbose=2 "$code" 2>&1)
    case "$signature" in
    *"$flags"*) ;;
    *) fail "$code isn't signed with the hardened runtime" ;;
    esac
    case "$nl$signature$nl" in
    *"$nl$signer$nl"*) ;;
    *) fail "$code isn't signed by $identity" ;;
    esac
    allowed=$(entitlements "$code") || exit 1
    case "$allowed" in
    *get-task-allow* | *allow-dyld-environment-variables*)
        fail "$code may be debugged or have code put in it" ;;
    esac
done 3<<EOF
$programs
EOF
allowed=$(entitlements "$app") || exit 1
case "$allowed" in
*disable-library-validation* | *allow-unsigned-executable-memory* | *allow-jit*)
    fail "$app loads libraries not its own, or makes code" ;;
esac
[ ! -e "$app/Contents/Frameworks" ] || fail "$app has frameworks (Sparkle is taken out)"
allowed=$(entitlements "$app/Contents/MacOS/pepper-agent") || exit 1
[ -z "$allowed" ] || fail "pepper-agent has entitlements"
# With a certificate, macOS must know the app by its name and certificate,
# not by this build's hash, or it forgets Pepper's permissions again.
if [ "$identity" != - ]; then
    case "$(codesign -d -r- "$app" 2>/dev/null)" in
    *cdhash*) fail "$app's designated requirement names the build" ;;
    *'identifier "io.github.erictran308.pepper" and certificate leaf = H"'*) ;;
    *) fail "$app's designated requirement isn't Pepper's identifier and certificate" ;;
    esac
fi

echo "pepper-build: $app $pepper_version, with pepper-play $version and pepper-agent $agent_version, signed by $identity" >&2
