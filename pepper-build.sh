#!/bin/sh
# Builds Pepper with Zig 0.15.2 against Apple's 26.4+ SDKs.
#
# Those SDKs' .tbd stubs list only arm64e-macos, and Zig 0.15.2 links only
# against stubs naming arm64-macos, so every libc symbol comes out undefined.
# Zig 0.16 fixes it (ziglang/zig#31673), but Ghostty 1.3.x needs 0.15.2.
# Xcode 27's math.h also expects INFINITY and NAN from a float.h newer than
# Zig's (Ghostty fixed it upstream in 1c861e3c4, after moving to Zig 0.16).
#
# So this keeps a clone of the SDK whose stubs also list arm64-macos and whose
# math.h supplies the two macros, and puts an xcrun first on PATH that gives
# its path to Zig. xcodebuild keeps using the real SDK.
#
# Then it builds pepper-play (pepper-play/, Rust) and puts it in the app as
# Contents/Helpers/Pepper.app, and pepper-agent (pepper-agent/, Rust) as
# Contents/MacOS/pepper-agent, signs them all ad hoc with the hardened runtime
# and entitlements of their own (pepper-agent: none), and checks what the
# signatures say.
#
# Usage: ./pepper-build.sh [zig build options]   (default: -Doptimize=ReleaseFast)
set -eu
cd "$(dirname "$0")"
path_before=$PATH

# Pepper's version: Ghostty's, then Pepper's own count on it. The build
# number counts Pepper's releases.
pepper_version=1.3.1-pepper.2
pepper_build=2

zig_want=0.15.2
zig_have=$(zig version 2>/dev/null || true)
if [ "$zig_have" != "$zig_want" ]; then
    echo "pepper-build: needs zig $zig_want, found '${zig_have:-none}'" >&2
    exit 1
fi

export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
sdk=$(/usr/bin/xcrun --sdk macosx --show-sdk-path)
sdk_ver=$(/usr/bin/xcrun --sdk macosx --show-sdk-version)
sdk_build=$(/usr/bin/xcrun --sdk macosx --show-sdk-build-version)
cache="${XDG_CACHE_HOME:-$HOME/.cache}/pepper"
view="$cache/sdk/MacOSX$sdk_ver-$sdk_build.sdk"
# This user's alone: what's in it goes into every build.
mkdir -p "$cache"
chmod 700 "$cache"

# Made again unless it was made from this very SDK.
if [ "$(cat "$view/.pepper-sdk" 2>/dev/null || true)" != "$sdk" ]; then
    echo "pepper-build: making SDK view $view" >&2
    mkdir -p "$cache/sdk"
    tmp="$view.tmp"
    rm -rf "$tmp"
    # -c clones on APFS: only the files changed below take space.
    cp -cR "$sdk/." "$tmp"
    # Adds arm64-macos to each [ ... ] target list that has arm64e-macos only.
    rg -l -uu --null 'arm64e-macos' -g '*.tbd' "$tmp" |
        xargs -0 perl -0777 -i -pe '
            s{\[([^\]]*)\]}{
                my $l = $1;
                ($l =~ /\barm64e-macos\b/ && $l !~ /\barm64-macos\b/)
                    ? "[" . ($l =~ s/\barm64e-macos\b/arm64-macos, arm64e-macos/r) . "]"
                    : "[$l]"
            }ge'
    cat >>"$tmp/usr/include/math.h" <<'EOF'

/* pepper: Zig 0.15.2's float.h lacks __need_infinity_nan (see pepper-build.sh). */
#ifndef INFINITY
#define INFINITY (__builtin_inff())
#endif
#ifndef NAN
#define NAN (__builtin_nanf(""))
#endif
EOF
    echo "$sdk" >"$tmp/.pepper-sdk"
    rm -rf "$view"
    mv "$tmp" "$view"
fi

mkdir -p "$cache/bin"
# Written under a name of its own, then moved over the old one: a link left
# at the shim's path is replaced, not written through.
shim="$cache/bin/.xcrun.$$"
rm -f "$shim"
cat >"$shim" <<'EOF'
#!/bin/sh
# pepper-build.sh's xcrun: Zig asks exactly this to find the SDK.
if [ "$#" -eq 3 ] && [ "$1" = --sdk ] && [ "$2" = macosx ] &&
    [ "$3" = --show-sdk-path ] && [ -n "${PEPPER_SDK:-}" ]; then
    printf '%s\n' "$PEPPER_SDK"
    exit 0
fi
exec /usr/bin/xcrun "$@"
EOF
chmod +x "$shim"
mv -f "$shim" "$cache/bin/xcrun"

[ "$#" -gt 0 ] || set -- -Doptimize=ReleaseFast
# The build copies the app over the one already in zig-out, keeping files
# the new build no longer has; so the old one goes first.
app=yes
case " $* " in
*emit-macos-app=false*) app=no ;;
*) rm -rf zig-out/Pepper.app ;;
esac
PEPPER_SDK="$view" PATH="$cache/bin:$PATH" zig build "$@"

# Built without the app (-Demit-macos-app=false): nothing to put it in.
[ "$app" = yes ] || exit 0
app=zig-out/Pepper.app
[ -d "$app" ] || exit 0

echo "pepper-build: building pepper-play" >&2
PATH="$path_before" cargo build --release --locked --manifest-path pepper-play/Cargo.toml
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
PATH="$path_before" cargo build --release --locked --manifest-path pepper-agent/Cargo.toml
# On every shell's PATH, next to pepper-play.
cp pepper-agent/target/release/pepper-agent "$app/Contents/MacOS/pepper-agent"
agent_version=$("$app/Contents/MacOS/pepper-agent" --version | cut -d' ' -f2)

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
codesign --force --sign - --options runtime \
    --entitlements pepper-play/pepper-play.entitlements "$helper"
# pepper-agent loads nothing and makes no code: no entitlements at all.
codesign --force --sign - --options runtime "$app/Contents/MacOS/pepper-agent"
codesign --force --sign - --options runtime \
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
while IFS= read -r code <&3; do
    case "$(codesign -d --verbose=2 "$code" 2>&1)" in
    *"flags=0x10002(adhoc,runtime)"*) ;;
    *) fail "$code isn't signed with the hardened runtime" ;;
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

echo "pepper-build: $app $pepper_version, with pepper-play $version and pepper-agent $agent_version" >&2
