#!/usr/bin/env python3
"""Render the DMG window background from the site's hero photograph.

Writes `crates/pilus/assets/dmg/background.png` (1x) and `background@2x.png`,
sized to the Finder window in `scripts/dmg-settings.py`. The output is
committed so packaging needs no image tooling; rerun this only when the photo
or the layout constants change (requires Pillow).
"""
import math
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

# Keep in sync with scripts/dmg-settings.py.
WINDOW = (660, 400)
APP_X, DROP_X, ICON_Y = 180, 480, 170
ARC_DIP = 30  # how far the arrow's arc sags below its ends, in points

root = Path(__file__).resolve().parent.parent
src = root / "site/assets/hero-factory.webp"
out = root / "crates/pilus/assets/dmg"


def render(scale: int) -> Image.Image:
    w, h = WINDOW[0] * scale, WINDOW[1] * scale
    photo = Image.open(src).convert("RGB")
    # Cover-fit, anchored low so the horizon sits under the icon row and the
    # sky stays clear behind the window title and the drag hint.
    ratio = max(w / photo.width, h / photo.height)
    photo = photo.resize((round(photo.width * ratio), round(photo.height * ratio)), Image.LANCZOS)
    left = (photo.width - w) // 2
    top = round((photo.height - h) * 0.80)
    canvas = photo.crop((left, top, left + w, top + h)).convert("RGBA")

    # Wash the photo toward white behind the icons, as the site does under its
    # hero, so icon labels stay readable on the haze.
    wash = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(wash)
    band_top, band_bot = int(h * 0.18), int(h * 0.80)
    for y in range(band_top, band_bot):
        t = (y - band_top) / (band_bot - band_top)
        a = int(120 * (1 - abs(2 * t - 1) ** 2))
        d.line([(0, y), (w, y)], fill=(255, 255, 255, a))
    canvas = Image.alpha_composite(canvas, wash)

    # Arrow from the app toward the Applications drop target: a U-shaped arc
    # (bottom to top) with round caps and an open chevron head turned to the curve's tangent. Drawn
    # 4x oversampled because PIL strokes have square ends; discs along the
    # path give the round caps and joins.
    ss = 4
    arrow = Image.new("RGBA", (w * ss, h * ss), (0, 0, 0, 0))
    a = ImageDraw.Draw(arrow)
    ink = (36, 44, 58, 215)
    r = 2.25 * scale * ss

    def disc(x, y):
        a.ellipse([x * ss - r, y * ss - r, x * ss + r, y * ss + r], fill=ink)

    def stroke(points):
        for (x0, y0), (x1, y1) in zip(points, points[1:]):
            steps = max(1, int(max(abs(x1 - x0), abs(y1 - y0)) * ss))
            for i in range(steps + 1):
                t = i / steps
                disc(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)

    # Quadratic Bezier; the control point sits at twice the arc's dip.
    dip = ARC_DIP * scale
    p0 = ((APP_X + 76) * scale, (ICON_Y + 16) * scale)
    p2 = ((DROP_X - 76) * scale, (ICON_Y + 16) * scale)
    p1 = ((p0[0] + p2[0]) / 2, p0[1] + 2 * dip)
    n = 120
    path = [
        (
            (1 - t) ** 2 * p0[0] + 2 * (1 - t) * t * p1[0] + t**2 * p2[0],
            (1 - t) ** 2 * p0[1] + 2 * (1 - t) * t * p1[1] + t**2 * p2[1],
        )
        for t in (i / n for i in range(n + 1))
    ]
    stroke(path)

    # Head: two wings swept back from the tip, ~40 degrees off the tangent.
    tx, ty = p2[0] - p1[0], p2[1] - p1[1]
    norm = math.hypot(tx, ty)
    tx, ty = tx / norm, ty / norm
    wing = 16 * scale
    for sign in (1, -1):
        ang = math.radians(40) * sign
        bx = -(tx * math.cos(ang) - ty * math.sin(ang))
        by = -(tx * math.sin(ang) + ty * math.cos(ang))
        stroke([(p2[0] + bx * wing, p2[1] + by * wing), p2])
    arrow = arrow.resize((w, h), Image.LANCZOS)
    shadow = arrow.filter(ImageFilter.GaussianBlur(3 * scale))
    shadow = Image.composite(Image.new("RGBA", (w, h), (255, 255, 255, 140)), Image.new("RGBA", (w, h), (0, 0, 0, 0)), shadow.split()[3])
    canvas = Image.alpha_composite(canvas, shadow)
    canvas = Image.alpha_composite(canvas, arrow)
    return canvas.convert("RGB")


out.mkdir(parents=True, exist_ok=True)
render(1).save(out / "background.png", dpi=(72, 72))
render(2).save(out / "background@2x.png", dpi=(144, 144))
print(f"wrote {out}/background.png and background@2x.png")
