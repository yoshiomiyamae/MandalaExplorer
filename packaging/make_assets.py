"""Generates every image asset the MSIX manifest asks for.

The icon is a grid of tiles arranged as a mandala, which is what the app puts
on screen. It is drawn here rather than rasterised from the SVG beside it so
that packaging needs nothing installed but Python and Pillow -- the same reason
it can run in CI.

    python packaging/make_assets.py [output_dir]

Everything is laid out on the 256px master grid and scaled, so a 16px app-list
icon and a 1240px tile are the same drawing.
"""

import sys
from pathlib import Path

from PIL import Image, ImageDraw

# Master canvas the geometry below is expressed in.
MASTER = 256

# Colours, as (top-left, bottom-right) of a diagonal gradient.
COOL = ((78, 168, 222), (58, 110, 165))
DEEP = ((124, 92, 191), (76, 58, 140))
WARM = ((240, 166, 60), (226, 98, 46))

# (x, y, w, h, radius, colours). Corners smallest, centre largest, so that as
# the icon shrinks the outer ring fades to texture and the centre still reads.
TILES = [
    # Outer corners.
    (24, 24, 40, 40, 8, COOL),
    (192, 24, 40, 40, 8, COOL),
    (24, 192, 40, 40, 8, COOL),
    (192, 192, 40, 40, 8, COOL),
    # Cardinal edges.
    (104, 16, 48, 48, 9, DEEP),
    (104, 192, 48, 48, 9, DEEP),
    (16, 104, 48, 48, 9, DEEP),
    (192, 104, 48, 48, 9, DEEP),
    # Inner ring.
    (72, 72, 44, 44, 9, COOL),
    (140, 72, 44, 44, 9, COOL),
    (72, 140, 44, 44, 9, COOL),
    (140, 140, 44, 44, 9, COOL),
    # Centre.
    (98, 98, 60, 60, 12, WARM),
]

# Drawn at this multiple of the target size and scaled down, since Pillow has
# no anti-aliased shape drawing of its own.
SUPERSAMPLE = 4


def gradient_tile(size, colours):
    """A rounded tile filled with a diagonal gradient."""
    (w, h) = size
    (r0, g0, b0), (r1, g1, b1) = colours
    tile = Image.new("RGBA", (w, h))
    pixels = tile.load()
    span = max(w + h - 2, 1)
    for y in range(h):
        for x in range(w):
            # Diagonal position, so the gradient runs corner to corner.
            t = (x + y) / span
            pixels[x, y] = (
                round(r0 + (r1 - r0) * t),
                round(g0 + (g1 - g0) * t),
                round(b0 + (b1 - b0) * t),
                255,
            )
    return tile


def draw_icon(px, padding_ratio=0.0):
    """Draws the icon at `px` square, with optional padding around the grid.

    Tile assets want the art inset from the edges; app-list icons want it to
    fill the square.
    """
    scale = px * SUPERSAMPLE / MASTER
    canvas = Image.new("RGBA", (px * SUPERSAMPLE, px * SUPERSAMPLE), (0, 0, 0, 0))

    inset = padding_ratio * px * SUPERSAMPLE
    usable = px * SUPERSAMPLE - inset * 2

    for x, y, w, h, radius, colours in TILES:
        tx = round(inset + x / MASTER * usable)
        ty = round(inset + y / MASTER * usable)
        tw = max(round(w / MASTER * usable), 1)
        th = max(round(h / MASTER * usable), 1)

        tile = gradient_tile((tw, th), colours)
        # A rounded-rectangle alpha mask, since the fill is a gradient.
        mask = Image.new("L", (tw, th), 0)
        ImageDraw.Draw(mask).rounded_rectangle(
            (0, 0, tw - 1, th - 1),
            radius=max(round(radius * scale * (usable / (px * SUPERSAMPLE))), 1),
            fill=255,
        )
        canvas.paste(tile, (tx, ty), mask)

    return canvas.resize((px, px), Image.LANCZOS)


def draw_wide(width, height):
    """The wide tile: the square icon centred on a transparent field."""
    canvas = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    side = round(height * 0.72)
    icon = draw_icon(side)
    canvas.paste(icon, ((width - side) // 2, (height - side) // 2), icon)
    return canvas


def draw_splash(width, height):
    """The splash screen: the icon, larger, on the same transparent field."""
    canvas = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    side = round(min(width, height) * 0.55)
    icon = draw_icon(side)
    canvas.paste(icon, ((width - side) // 2, (height - side) // 2), icon)
    return canvas


# Manifest entries with a square base size, and the scales to emit.
SQUARE_ASSETS = {
    "Square44x44Logo": 44,
    "Square71x71Logo": 71,
    "Square150x150Logo": 150,
    "Square310x310Logo": 310,
    "StoreLogo": 50,
}
SCALES = [100, 125, 150, 200, 400]

# Sizes Windows asks for by exact pixel count rather than by scale factor.
TARGET_SIZES = [16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256]


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "packaging/Assets")
    out.mkdir(parents=True, exist_ok=True)
    written = 0

    for name, base in SQUARE_ASSETS.items():
        for scale in SCALES:
            px = round(base * scale / 100)
            # Tiles inset their art; the app-list icon fills its square.
            padding = 0.0 if name == "Square44x44Logo" else 0.08
            draw_icon(px, padding).save(out / f"{name}.scale-{scale}.png")
            written += 1

    # The app-list icon, unplated so Windows does not draw a backplate behind
    # it, in every size the shell asks for. Same art for both themes: it is
    # legible on either, having no near-white or near-black in it.
    for size in TARGET_SIZES:
        icon = draw_icon(size)
        for suffix in ("", "_altform-unplated", "_altform-lightunplated"):
            icon.save(out / f"Square44x44Logo.targetsize-{size}{suffix}.png")
            written += 1

    for scale in SCALES:
        draw_wide(round(310 * scale / 100), round(150 * scale / 100)).save(
            out / f"Wide310x150Logo.scale-{scale}.png"
        )
        draw_splash(round(620 * scale / 100), round(300 * scale / 100)).save(
            out / f"SplashScreen.scale-{scale}.png"
        )
        written += 2

    # The .ico the unpackaged build uses, so both routes look the same.
    ico_sizes = [(s, s) for s in (16, 24, 32, 48, 64, 128, 256)]
    draw_icon(256).save(out / "mandala.ico", sizes=ico_sizes)
    written += 1

    print(f"wrote {written} files to {out}")


if __name__ == "__main__":
    main()
