#!/usr/bin/env python3
"""Generate assets/app.ico — dark rounded tile with three metric bars
(CPU blue, RAM green, NET orange). Pure stdlib, BMP-encoded ICO entries."""

import struct
import os
import zlib

# Small sizes as BMP, 256 as PNG (ICO supports PNG entries; keeps size sane).
SIZES = [16, 24, 32, 48]
PNG_SIZE = 256

BG = (31, 34, 41)        # dark tile
BARS = [
    ((79, 163, 255), 0.55),   # blue, 55% height
    ((52, 211, 153), 0.85),   # green, 85%
    ((245, 158, 11), 0.42),   # orange, 42%
]


def render(sz):
    """Return BGRA top-down pixel rows."""
    px = [[(0, 0, 0, 0)] * sz for _ in range(sz)]
    r = max(2, sz // 6)  # corner radius
    corners = [(r, r), (sz - 1 - r, r), (r, sz - 1 - r), (sz - 1 - r, sz - 1 - r)]
    for y in range(sz):
        for x in range(sz):
            inside = True
            if (x < r and y < r) or (x >= sz - r and y < r) or (x < r and y >= sz - r) or (
                x >= sz - r and y >= sz - r
            ):
                cx, cy = min(corners, key=lambda c: (c[0] - x) ** 2 + (c[1] - y) ** 2)
                inside = (x - cx) ** 2 + (y - cy) ** 2 <= r * r
            if inside:
                px[y][x] = (*BG, 255)

    # Three bars rising from a common baseline.
    pad = max(2, sz * 3 // 16)
    gap = max(1, sz // 16)
    bar_w = (sz - 2 * pad - 2 * gap) // 3
    base = sz - pad
    for i, (color, frac) in enumerate(BARS):
        x0 = pad + i * (bar_w + gap)
        h = max(2, int((sz - 2 * pad) * frac))
        for y in range(base - h, base):
            for x in range(x0, min(x0 + bar_w, sz)):
                if 0 <= y < sz:
                    px[y][x] = (*color, 255)
    return px


def bmp_entry(px, sz):
    header = struct.pack(
        "<IiiHHIIiiII", 40, sz, sz * 2, 1, 32, 0, 0, 0, 0, 0, 0
    )
    body = bytearray()
    for y in range(sz - 1, -1, -1):  # bottom-up
        for x in range(sz):
            r, g, b, a = px[y][x]
            body += bytes((b, g, r, a))
    # 1bpp AND mask, all zero, rows padded to 32 bits
    row_bytes = ((sz + 31) // 32) * 4
    mask = bytes(row_bytes * sz)
    return header + bytes(body) + mask


def png_entry(px, sz):
    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    raw = bytearray()
    for y in range(sz):
        raw.append(0)  # no filter
        for x in range(sz):
            r, g, b, a = px[y][x]
            raw += bytes((r, g, b, a))
    ihdr = struct.pack(">IIBBBBB", sz, sz, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def write_ico(path, entries):
    out = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    dir_bytes = b""
    payload = b""
    for sz, data in entries:
        w = h = 0 if sz == 256 else sz
        dir_bytes += struct.pack("<BBBBHHII", w, h, 0, 0, 1, 32, len(data), offset)
        payload += data
        offset += len(data)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(out + dir_bytes + payload)
    print(f"wrote {path} ({6 + len(dir_bytes) + len(payload)} bytes)")


def main():
    assets = os.path.join(os.path.dirname(__file__), "..", "assets")
    entries = []
    for sz in SIZES:
        data = bmp_entry(render(sz), sz)
        entries.append((sz, data))
    # Classic BMP-only variant first (NSIS rejects PNG entries).
    write_ico(os.path.join(assets, "app-classic.ico"), list(entries))
    entries.append((PNG_SIZE, png_entry(render(PNG_SIZE), PNG_SIZE)))
    write_ico(os.path.join(assets, "app.ico"), entries)


if __name__ == "__main__":
    main()
