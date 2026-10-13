#!/bin/sh
# Builds pepper with Zig 0.15.2 against Apple's 26.4+ SDKs.
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
# Usage: ./pepper-build.sh [zig build options]   (default: -Doptimize=ReleaseFast)
set -eu

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

if [ ! -f "$view/.pepper-sdk" ]; then
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
cat >"$cache/bin/xcrun" <<'EOF'
#!/bin/sh
# pepper-build.sh's xcrun: Zig asks exactly this to find the SDK.
if [ "$#" -eq 3 ] && [ "$1" = --sdk ] && [ "$2" = macosx ] &&
    [ "$3" = --show-sdk-path ] && [ -n "${PEPPER_SDK:-}" ]; then
    printf '%s\n' "$PEPPER_SDK"
    exit 0
fi
exec /usr/bin/xcrun "$@"
EOF
chmod +x "$cache/bin/xcrun"

export PEPPER_SDK="$view"
export PATH="$cache/bin:$PATH"
[ "$#" -gt 0 ] || set -- -Doptimize=ReleaseFast
exec zig build "$@"
