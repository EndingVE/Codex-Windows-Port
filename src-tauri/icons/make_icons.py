"""Generate the tray/app icon raster assets for the CodexBar Windows skeleton.

No third-party deps: writes PNG (zlib + struct) and an ICO that embeds PNG frames.
The image is the same donut-gauge motif the Rust tray renderer draws, so the tray
icon and the window/taskbar icon look identical.
"""

import os
import struct
import zlib

OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)))


def blend(dst, src):
    """Straight-alpha source-over onto a premultiplied-free RGBA tuple."""
    sr, sg, sb, sa = src
    dr, dg, db, da = dst
    if sa == 0:
        return dst
    a = sa / 255.0
    return (
        int(sr * a + dr * (1 - a)),
        int(sg * a + dg * (1 - a)),
        int(sb * a + db * (1 - a)),
        min(255, int(sa + da * (1 - a))),
    )


def render(size, ring_ratio=0.36, thickness_ratio=0.20, used_fraction=0.42):
    """Donut gauge: coloured arc = used fraction, dark track = the rest."""
    cx = cy = (size - 1) / 2.0
    outer = size * 0.5 - size * 0.06
    inner = outer - max(1.5, size * thickness_ratio * 0.5)
    track = (58, 63, 74, 255)
    accent = (34, 197, 94, 255) if used_fraction < 0.7 else (245, 158, 11, 255)
    if used_fraction >= 0.9:
        accent = (239, 68, 68, 255)

    px = [[(0, 0, 0, 0) for _ in range(size)] for _ in range(size)]
    start = -90.0  # 12 o'clock
    sweep = 360.0 * used_fraction

    for y in range(size):
        for x in range(size):
            dx, dy = x - cx, y - cy
            dist = (dx * dx + dy * dy) ** 0.5
            if dist > outer:
                continue
            if dist < inner:
                # Solid hub so the mark stays legible at 16 px.
                px[y][x] = blend(px[y][x], (30, 33, 40, 255))
                continue
            import math

            ang = (math.degrees(math.atan2(dy, dx)) - start) % 360.0
            if ang <= sweep:
                px[y][x] = blend(px[y][x], accent)
            else:
                px[y][x] = blend(px[y][x], track)

    # Small accent dot at 12 o'clock to hint "top of window".
    r = max(1, size // 16)
    for y in range(size):
        for x in range(size):
            if (x - cx) ** 2 + (y - cy) ** 2 <= r * r:
                px[y][x] = blend(px[y][x], (255, 255, 255, 255))
    return px


def to_rgba_rows(px):
    size = len(px)
    raw = bytearray()
    for y in range(size):
        raw.append(0)  # filter: none
        for x in range(size):
            raw.extend(px[y][x])
    return bytes(raw), size


def png_bytes(px):
    raw, size = to_rgba_rows(px)

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico_bytes(frames):
    """ICO container with PNG-compressed frames (Vista+)."""
    count = len(frames)
    header = struct.pack("<HHH", 0, 1, count)
    offset = 6 + 16 * count
    entries, blobs = b"", b""
    for size, data in frames:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        blobs += data
        offset += len(data)
    return header + entries + blobs


def main():
    targets = [
        ("32x32.png", 32, 0.42),
        ("128x128.png", 128, 0.42),
        ("128x128@2x.png", 256, 0.42),
        ("icon.png", 512, 0.42),
        ("Square30x30Logo.png", 30, 0.62),
        ("Square44x44Logo.png", 44, 0.62),
        ("Square71x71Logo.png", 71, 0.62),
        ("Square89x89Logo.png", 89, 0.62),
        ("Square107x107Logo.png", 107, 0.62),
        ("Square142x142Logo.png", 142, 0.62),
        ("Square150x150Logo.png", 150, 0.62),
        ("Square284x284Logo.png", 284, 0.62),
        ("Square310x310Logo.png", 310, 0.62),
        ("StoreLogo.png", 50, 0.62),
    ]
    for name, size, frac in targets:
        with open(os.path.join(OUT_DIR, name), "wb") as fh:
            fh.write(png_bytes(render(size, used_fraction=frac)))

    frames = [(16, png_bytes(render(16, used_fraction=0.55))),
              (32, png_bytes(render(32, used_fraction=0.42))),
              (48, png_bytes(render(48, used_fraction=0.42))),
              (256, png_bytes(render(256, used_fraction=0.42)))]
    with open(os.path.join(OUT_DIR, "icon.ico"), "wb") as fh:
        fh.write(ico_bytes(frames))

    print("wrote icons to", OUT_DIR)


if __name__ == "__main__":
    main()
