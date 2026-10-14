# Makes the inputs in crates/nuiter-core/testdata/reference: a picture, a screenshot, a noise patch, and what Pillow's
# bicubic resize makes of them (extract::bicubic must give the same; cargo test -p nuiter-core checks it).
# Usage: python scripts/make-reference.py
import json
import math
import random
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

out = Path(__file__).resolve().parent.parent / "crates" / "nuiter-core" / "testdata" / "reference"
(out / "resize").mkdir(parents=True, exist_ok=True)
rng = random.Random(7)


def target_size(width, height, budget):
    """ImagePipeline.TargetSize: fill the budget with 48 px blocks, keep the aspect ratio."""
    patch, pooling = 16, 3
    side = patch * pooling
    factor = math.sqrt(budget * pooling * pooling * patch * patch / (height * width))
    return int(factor * width // side) * side, int(factor * height // side) * side


def landscape():
    w, h = 1600, 1200
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            t = y / h
            px[x, y] = (int(255 * (0.95 - 0.3 * t)), int(255 * (0.55 - 0.2 * t)), int(255 * (0.35 + 0.3 * t)))
    d = ImageDraw.Draw(img)
    d.ellipse((1050, 300, 1350, 600), fill=(255, 210, 90))
    for i, (base, colour) in enumerate([(760, (70, 110, 60)), (880, (45, 85, 40)), (1000, (30, 60, 30))]):
        pts = [(0, h)] + [(x, base + 60 * math.sin(x / (180 + 70 * i) + i) + rng.uniform(-8, 8)) for x in range(0, w + 40, 40)] + [(w, h)]
        d.polygon(pts, fill=colour)
    for _ in range(400):
        x, y = rng.randrange(w), rng.randrange(900, h)
        d.line((x, y, x + rng.randint(-3, 3), y - rng.randint(8, 30)), fill=(20, 50 + rng.randint(0, 60), 25), width=2)
    return img.filter(ImageFilter.GaussianBlur(0.6))


def invoice():
    img = Image.new("RGB", (1280, 800), (243, 243, 243))
    d = ImageDraw.Draw(img)
    big, mid, small = (ImageFont.truetype("arial.ttf", s) for s in (40, 28, 22))
    d.rectangle((0, 0, 1280, 48), fill=(32, 32, 32))
    d.text((20, 10), "Invoice INV-2026-0042 - Mail", font=small, fill=(230, 230, 230))
    d.text((80, 110), "Invoice INV-2026-0042", font=big, fill=(20, 20, 20))
    d.text((80, 180), "Northwind Plumbing Ltd.", font=mid, fill=(60, 60, 60))
    rows = [("Boiler service", "$420.00"), ("Replacement valve", "$180.00"), ("Labour, 4 hours", "$640.00")]
    for i, (item, price) in enumerate(rows):
        y = 280 + i * 60
        d.line((80, y - 10, 1200, y - 10), fill=(210, 210, 210), width=2)
        d.text((80, y), item, font=mid, fill=(30, 30, 30))
        d.text((1020, y), price, font=mid, fill=(30, 30, 30))
    d.text((80, 500), "Total due: $1,240.00", font=big, fill=(160, 20, 20))
    d.text((80, 570), "Payment due by 30 November 2026", font=mid, fill=(60, 60, 60))
    return img


def tiny():
    img = Image.new("RGB", (37, 23))
    px = img.load()
    for y in range(23):
        for x in range(37):
            px[x, y] = (rng.randrange(256), rng.randrange(256), rng.randrange(256))
    return img


pictures = {"photo.png": landscape(), "screen.png": invoice(), "noise.png": tiny()}
for name, img in pictures.items():
    img.save(out / name)

sizes = [
    ("photo.png", target_size(1600, 1200, 140)),
    ("photo.png", target_size(1600, 1200, 70)),
    ("screen.png", target_size(1280, 800, 280)),
    ("screen.png", (1280, 480)),  # only one axis changes
    ("noise.png", (96, 48)),  # enlarging
    ("noise.png", (5, 3)),
]
cases = []
for name, (w, h) in sizes:
    expected = f"resize/{Path(name).stem}-{w}x{h}.png"
    pictures[name].resize((w, h), Image.BICUBIC).save(out / expected)
    cases.append({"input": name, "width": w, "height": h, "expected": expected})
(out / "resize" / "cases.json").write_text(json.dumps(cases, indent=2))
print("\n".join(f"{c['input']} -> {c['width']}x{c['height']}" for c in cases))
