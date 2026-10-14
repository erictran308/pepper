#!/usr/bin/env python3
"""Makes images/Pepper.icon, Pepper's app icon, from images/Ghostty.icon:
Ghostty's bezel and blue screen, with a prompt and a play symbol ("> ▶", a
terminal that plays) drawn in the ghost's style in place of the ghost.

Needs Pillow. Run from anywhere: images/make-pepper-icon.py. Then rebuild
(the Xcode project compiles Pepper.icon) and remake the flat copies in
macos/Assets.xcassets/AppIconImage.imageset (1024, 512 and 256 px) from the
built app with images/render-app-icon.swift.
"""
import json
import math
import shutil
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

IMAGES = Path(__file__).resolve().parent
SRC, DST = IMAGES / "Ghostty.icon", IMAGES / "Pepper.icon"
# The ghost's glow, Ghostty's blue.
GLOW = (0, 122, 255)
# The glyph's size in points, drawn K times larger and scaled down.
W, H, K = 520, 560, 4


def closed(mask, radius):
    """Rounds a mask's corners: shrink, then grow."""
    size = radius * 2 + 1
    return mask.filter(ImageFilter.MinFilter(size)).filter(ImageFilter.MaxFilter(size))


def triangle(cx, cy, r):
    return [
        (cx + r * math.cos(math.radians(a)), cy + r * math.sin(math.radians(a)))
        for a in (0, 120, 240)
    ]


def glyph_mask():
    """A chevron, then a play triangle where the cursor would be."""
    big = (W * K, H * K)
    mask = Image.new("L", big, 0)
    draw = ImageDraw.Draw(mask)
    width = int(H * K * 0.085)
    x0, y0, x1, y1 = W * K * 0.08, H * K * 0.37, W * K * 0.30, H * K * 0.5
    points = [(x0, y0), (x1, y1), (x0, H * K - y0)]
    draw.line(points, fill=255, width=width, joint="curve")
    for x, y in points:
        draw.ellipse([x - width / 2, y - width / 2, x + width / 2, y + width / 2], fill=255)
    play = Image.new("L", big, 0)
    ImageDraw.Draw(play).polygon(triangle(W * K * 0.68, H * K * 0.5, H * K * 0.21), fill=255)
    return ImageChops.lighter(mask, closed(play, 30))


def glyph():
    """The glyph as the ghost is drawn: a white edge, a white-to-light-blue
    fill, a soft blue glow."""
    big = (W * K, H * K)
    outer = glyph_mask()
    inner = outer.filter(ImageFilter.MinFilter(7 * K * 2 + 1))
    fill = Image.new("RGBA", big)
    draw = ImageDraw.Draw(fill)
    top = (255, 255, 255)
    bottom = tuple(min(255, int(c * 0.55 + 255 * 0.45)) for c in GLOW)
    for y in range(big[1]):
        t = y / big[1]
        color = tuple(int(top[i] * (1 - t) + bottom[i] * t) for i in range(3))
        draw.line([(0, y), (big[0], y)], fill=color + (255,))
    out = Image.new("RGBA", big, (0, 0, 0, 0))
    out.paste(Image.new("RGBA", big, (255, 255, 255, 255)), (0, 0), outer)
    out.paste(fill, (0, 0), inner)
    glow = Image.new("RGBA", big, GLOW + (0,))
    glow.putalpha(outer.filter(ImageFilter.GaussianBlur(28 * K)).point(lambda a: int(a * 0.9)))
    return Image.alpha_composite(glow, out).resize((W, H), Image.LANCZOS)


def main():
    shutil.rmtree(DST, ignore_errors=True)
    shutil.copytree(SRC, DST)
    (DST / "Assets/Ghostty.png").unlink()
    glyph().save(DST / "Assets/Pepper.png")

    doc = json.loads((SRC / "icon.json").read_text())
    for group in doc["groups"]:
        for layer in group["layers"]:
            if layer.get("image-name") == "Ghostty.png":
                layer["image-name"] = "Pepper.png"
                layer["name"] = layer["name"].replace("Ghostty", "Pepper")
                # Centred, where the ghost sat at the top left.
                layer["position"] = {"scale": 1, "translation-in-points": [0, 0]}
    (DST / "icon.json").write_text(json.dumps(doc, indent=2) + "\n")
    print(f"made {DST}")


if __name__ == "__main__":
    main()
