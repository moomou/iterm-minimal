#!/usr/bin/env python3
"""Process a raw square icon PNG into a transparent macOS HIG 1024x1024 PNG and Retina .icns.

Pipeline:
  1. Flood-fill exterior black background from the 4 image corners stopping at the
     metallic bezel ring (preserving dark pixels inside the terminal body).
  2. Fit the exact rounded-rectangle outer contour and inset slightly (~2px) into the
     solid metallic bezel to eliminate black-edge anti-aliasing halos/ringing.
  3. Normalize the bezel tile to Apple's macOS HIG icon grid: 824x824 centered on a
     1024x1024 transparent canvas (100px margin on all 4 sides) with analytically
     anti-aliased rounded corners and subtle macOS drop shadow.
  4. Pack all standard macOS 1x + Retina @2x PNG representations (16..1024px) into a
     spec-compliant Apple .icns container.
"""

from __future__ import annotations

import argparse
import io
import struct
import sys
from collections import deque
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter

CANVAS = 1024
TILE_SIZE = 824
TILE_MARGIN = (CANVAS - TILE_SIZE) // 2  # 100px on all 4 sides (Apple macOS HIG)
CORNER_RADIUS_RATIO = 0.205  # Matches source icon bezel (r_norm=0.410 -> r/width=0.205)

# Apple .icns OSTypes (PNG payloads supported on macOS 10.7+; LSMinimumSystemVersion=13.0)
ICNS_SLOTS: list[tuple[bytes, int]] = [
    (b"icp4", 16),    # 16x16 @1x
    (b"ic11", 32),    # 16x16 @2x
    (b"icp5", 32),    # 32x32 @1x
    (b"ic12", 64),    # 32x32 @2x
    (b"icp6", 64),    # 64x64 @1x
    (b"ic07", 128),   # 128x128 @1x
    (b"ic13", 256),   # 128x128 @2x
    (b"ic08", 256),   # 256x256 @1x
    (b"ic14", 512),   # 256x256 @2x
    (b"ic09", 512),   # 512x512 @1x
    (b"ic10", 1024),  # 512x512 @2x
]


def detect_bezel_bbox(arr: np.ndarray, thresh: int = 40) -> tuple[int, int, int, int]:
    """Find outer bounding box of the watertight bezel via exterior corner flood-fill."""
    h, w = arr.shape[:2]
    gray = arr[:, :, :3].max(axis=2)
    ext = np.zeros((h, w), dtype=bool)
    q: deque[tuple[int, int]] = deque([(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)])
    for x, y in q:
        ext[y, x] = True
    while q:
        x, y = q.popleft()
        for nx, ny in ((x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)):
            if 0 <= nx < w and 0 <= ny < h and not ext[ny, nx] and gray[ny, nx] <= thresh:
                ext[ny, nx] = True
                q.append((nx, ny))
    inside = ~ext
    ys, xs = np.where(inside)
    if len(xs) == 0:
        raise RuntimeError("Could not locate icon bezel in source image")
    return int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max())


def rounded_rect_sdf(h: int, w: int, cx: float, cy: float, half_w: float, half_h: float, radius: float) -> np.ndarray:
    """Signed distance field for a centered rounded rectangle (negative inside, positive outside)."""
    yy, xx = np.mgrid[0:h, 0:w].astype(np.float64)
    dx = np.abs(xx + 0.5 - cx) - (half_w - radius)
    dy = np.abs(yy + 0.5 - cy) - (half_h - radius)
    outside = np.hypot(np.maximum(dx, 0.0), np.maximum(dy, 0.0))
    inside = np.minimum(np.maximum(dx, dy), 0.0)
    return outside + inside - radius


def process_icon(src_path: Path, inset_px: float = 2.5, add_shadow: bool = True) -> Image.Image:
    im = Image.open(src_path).convert("RGBA")
    arr = np.array(im)

    x0, y0, x1, y1 = detect_bezel_bbox(arr, thresh=40)
    # Inset slightly inside the solid metallic bezel to remove black-blended outer halo/ringing.
    cx = (x0 + x1 + 1) / 2.0
    cy = (y0 + y1 + 1) / 2.0
    crop_x0 = x0 + inset_px
    crop_y0 = y0 + inset_px
    crop_x1 = (x1 + 1) - inset_px
    crop_y1 = (y1 + 1) - inset_px

    # Resample the bezel region onto the exact Apple HIG 824x824 tile using subpixel Lanczos crop.
    tile_rgb = im.convert("RGB").resize(
        (TILE_SIZE, TILE_SIZE),
        resample=Image.Resampling.LANCZOS,
        box=(crop_x0, crop_y0, crop_x1, crop_y1),
    )

    # Analytically anti-aliased rounded-rectangle alpha mask (exact 1px smooth transition).
    # Subtract 0.5px from half-size so the outer AA ramp stays strictly inside the solid metallic rim.
    radius = TILE_SIZE * CORNER_RADIUS_RATIO
    sdf = rounded_rect_sdf(
        TILE_SIZE,
        TILE_SIZE,
        cx=TILE_SIZE / 2.0,
        cy=TILE_SIZE / 2.0,
        half_w=TILE_SIZE / 2.0 - 0.5,
        half_h=TILE_SIZE / 2.0 - 0.5,
        radius=radius,
    )
    alpha_f = np.clip(0.5 - sdf, 0.0, 1.0)
    alpha_u8 = np.rint(alpha_f * 255.0).astype(np.uint8)

    tile_rgba = tile_rgb.convert("RGBA")
    tile_rgba.putalpha(Image.fromarray(alpha_u8, mode="L"))

    canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))

    if add_shadow:
        # Subtle Apple macOS HIG drop shadow (offset y +12px, blur 12px, ~28% opacity).
        shadow_alpha = Image.new("L", (CANVAS, CANVAS), 0)
        shadow_alpha.paste(Image.fromarray(alpha_u8, mode="L"), (TILE_MARGIN, TILE_MARGIN + 12))
        shadow_alpha = shadow_alpha.filter(ImageFilter.GaussianBlur(radius=12.0))
        shadow_arr = np.rint(np.array(shadow_alpha, dtype=np.float64) * 0.28).astype(np.uint8)
        shadow_layer = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
        shadow_layer.putalpha(Image.fromarray(shadow_arr, mode="L"))
        canvas = Image.alpha_composite(canvas, shadow_layer)

    fg_layer = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    fg_layer.paste(tile_rgba, (TILE_MARGIN, TILE_MARGIN))
    canvas = Image.alpha_composite(canvas, fg_layer)
    return canvas


def build_icns(master_1024: Image.Image) -> bytes:
    """Encode a 1024x1024 RGBA Pillow image into a multi-resolution Retina Apple .icns blob."""
    if master_1024.size != (1024, 1024) or master_1024.mode != "RGBA":
        raise ValueError("Master icon must be 1024x1024 RGBA")

    png_cache: dict[int, bytes] = {}
    chunks: list[bytes] = []

    for ostype, size in ICNS_SLOTS:
        if size not in png_cache:
            resized = (
                master_1024
                if size == 1024
                else master_1024.resize((size, size), resample=Image.Resampling.LANCZOS)
            )
            buf = io.BytesIO()
            resized.save(buf, format="PNG", optimize=True)
            png_cache[size] = buf.getvalue()
        payload = png_cache[size]
        chunks.append(ostype + struct.pack(">I", 8 + len(payload)) + payload)

    body = b"".join(chunks)
    return b"icns" + struct.pack(">I", 8 + len(body)) + body


def main() -> int:
    parser = argparse.ArgumentParser(description="Build transparent macOS AppIcon.png and AppIcon.icns")
    parser.add_argument("input", type=Path, help="Source icon PNG")
    parser.add_argument("--out-png", type=Path, required=True, help="Output 1024x1024 transparent PNG")
    parser.add_argument("--out-icns", type=Path, required=True, help="Output macOS .icns file")
    parser.add_argument("--no-shadow", action="store_true", help="Omit macOS HIG drop shadow")
    args = parser.parse_args()

    master = process_icon(args.input, add_shadow=not args.no_shadow)
    args.out_png.parent.mkdir(parents=True, exist_ok=True)
    args.out_icns.parent.mkdir(parents=True, exist_ok=True)

    master.save(args.out_png, format="PNG", optimize=True)
    icns_bytes = build_icns(master)
    args.out_icns.write_bytes(icns_bytes)

    print(f"Wrote {args.out_png} ({args.out_png.stat().st_size:,} bytes, 1024x1024 RGBA)")
    print(f"Wrote {args.out_icns} ({len(icns_bytes):,} bytes, {len(ICNS_SLOTS)} Retina/1x slots)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
