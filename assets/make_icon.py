"""Draws the playr icon: playr.svg, playr.png (256 px) and playr.ico (16-256 px).

The SVG and the raster images are drawn from the same geometry below. The rasters are
drawn at 8x and scaled down, which is how Pillow gets anti-aliased edges.

Run from anywhere: python assets/make_icon.py
"""
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
SIZE = 256
RADIUS = 56  # rounded-square corner
TOP, BOTTOM = (0x7C, 0x3A, 0xED), (0xF9, 0x73, 0x16)  # violet -> orange, top-left to bottom-right
# Play triangle, nudged right of centre so it looks centred (its mass sits left of its box).
TRIANGLE = [(76, 72), (76, 184), (168, 128)]
TRIANGLE_ROUND = 14
# Sound arcs right of the triangle: (radius, stroke width, opacity 0-255).
ARCS = [(46, 10, 170), (70, 10, 110)]
ARC_CENTRE = (146, 128)
ARC_SPAN = 38  # degrees either side of horizontal


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def raster(size):
    s = 8 * size / SIZE  # supersampling scale
    big = round(SIZE * s)
    # The gradient is smooth, so it is computed at 256 px and scaled up rather than per big pixel.
    grad = Image.new("RGBA", (SIZE, SIZE))
    px = grad.load()
    for y in range(SIZE):
        for x in range(SIZE):
            px[x, y] = lerp(TOP, BOTTOM, (x + y) / (2 * SIZE - 2)) + (255,)
    grad = grad.resize((big, big), Image.BILINEAR)
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, big - 1, big - 1], radius=RADIUS * s, fill=255)
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    img.paste(grad, (0, 0), mask)

    over = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(over)
    cx, cy = ARC_CENTRE
    for r, w, a in ARCS:
        box = [(cx - r) * s, (cy - r) * s, (cx + r) * s, (cy + r) * s]
        d.arc(box, -ARC_SPAN, ARC_SPAN, fill=(255, 255, 255, a), width=round(w * s))
    pts = [(x * s, y * s) for x, y in TRIANGLE]
    d.polygon(pts, fill=(255, 255, 255, 255))
    # Round the triangle's corners by stroking its outline with a thick round-joined line.
    d.line(pts + [pts[0]], fill=(255, 255, 255, 255), width=round(TRIANGLE_ROUND * s), joint="curve")
    for x, y in pts:
        r = TRIANGLE_ROUND * s / 2
        d.ellipse([x - r, y - r, x + r, y + r], fill=(255, 255, 255, 255))
    img = Image.alpha_composite(img, over)
    return img.resize((size, size), Image.LANCZOS)


def svg():
    import math

    cx, cy = ARC_CENTRE
    arcs = []
    for r, w, a in ARCS:
        t = math.radians(ARC_SPAN)
        x0, y0 = cx + r * math.cos(t), cy - r * math.sin(t)
        x1, y1 = cx + r * math.cos(t), cy + r * math.sin(t)
        arcs.append(
            f'  <path d="M{x0:.2f} {y0:.2f} A{r} {r} 0 0 1 {x1:.2f} {y1:.2f}" fill="none" '
            f'stroke="#fff" stroke-opacity="{a / 255:.2f}" stroke-width="{w}"/>'
        )
    tri = " ".join(f"{x},{y}" for x, y in TRIANGLE)
    hexc = lambda c: "#%02x%02x%02x" % c
    return "\n".join(
        [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}">',
            "  <defs>",
            '    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">',
            f'      <stop offset="0" stop-color="{hexc(TOP)}"/>',
            f'      <stop offset="1" stop-color="{hexc(BOTTOM)}"/>',
            "    </linearGradient>",
            "  </defs>",
            f'  <rect width="{SIZE}" height="{SIZE}" rx="{RADIUS}" fill="url(#bg)"/>',
            *arcs,
            f'  <polygon points="{tri}" fill="#fff" stroke="#fff" stroke-width="{TRIANGLE_ROUND}" stroke-linejoin="round"/>',
            "</svg>",
            "",
        ]
    )


def main():
    (HERE / "playr.svg").write_bytes(svg().encode("utf-8"))
    raster(SIZE).save(HERE / "playr.png", optimize=True)
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    # Each ICO size is drawn on its own rather than scaled from 256, so small sizes stay sharp.
    frames = [raster(n) for n in sizes]
    frames[-1].save(HERE / "playr.ico", sizes=[(n, n) for n in sizes], append_images=frames[:-1])


if __name__ == "__main__":
    main()
