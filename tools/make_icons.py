"""Generate RDM icons (PNG sizes + Windows .ico) with no third-party libraries."""
import os
import struct
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TOP = (0x5B, 0x7C, 0xFF)
BOTTOM = (0x7B, 0x4D, 0xF2)


def inside_round_rect(x, y, s, r):
    # Rounded square covering [0, s).
    cx = min(max(x, r), s - r)
    cy = min(max(y, r), s - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def arrow_coverage(x, y, s):
    """Download glyph: vertical shaft, chevron head, tray bar. Coordinates in 0..1."""
    u, v = x / s, y / s
    w = 0.085
    # Shaft.
    if abs(u - 0.5) < w / 2 and 0.2 < v < 0.6:
        return True
    # Chevron arms: lines from (0.31,0.43) and (0.69,0.43) to (0.5,0.62).
    for x0 in (0.31, 0.69):
        ax, ay, bx, by = x0, 0.43, 0.5, 0.62
        dx, dy = bx - ax, by - ay
        t = max(0.0, min(1.0, ((u - ax) * dx + (v - ay) * dy) / (dx * dx + dy * dy)))
        px, py = ax + t * dx, ay + t * dy
        if (u - px) ** 2 + (v - py) ** 2 < (w / 2) ** 2:
            return True
    # Tray: horizontal bar with rounded ends.
    if 0.24 < u < 0.76 and abs(v - 0.76) < w / 2:
        return True
    for ex in (0.24, 0.76):
        if (u - ex) ** 2 + (v - 0.76) ** 2 < (w / 2) ** 2:
            return True
    return False


def render(size, ss=4):
    rows = []
    r = size * 0.22
    for y in range(size):
        row = bytearray()
        for x in range(size):
            acc = [0, 0, 0, 0]
            for sy in range(ss):
                for sx in range(ss):
                    fx = x + (sx + 0.5) / ss
                    fy = y + (sy + 0.5) / ss
                    if not inside_round_rect(fx, fy, size, r):
                        continue
                    t = fy / size
                    col = [int(TOP[i] * (1 - t) + BOTTOM[i] * t) for i in range(3)]
                    if arrow_coverage(fx, fy, size):
                        col = [255, 255, 255]
                    acc[0] += col[0]
                    acc[1] += col[1]
                    acc[2] += col[2]
                    acc[3] += 255
            n = ss * ss
            a = acc[3] // n
            if a:
                cover = acc[3] / 255
                row += bytes([int(acc[0] / cover), int(acc[1] / cover), int(acc[2] / cover), a])
            else:
                row += b"\0\0\0\0"
        rows.append(bytes(row))
    return rows


def png(size):
    rows = render(size)
    raw = b"".join(b"\0" + r for r in rows)

    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico(pngs):
    header = struct.pack("<HHH", 0, 1, len(pngs))
    entries, data = b"", b""
    offset = 6 + 16 * len(pngs)
    for size, blob in pngs:
        s = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", s, s, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    return header + entries + data


def main():
    ext = os.path.join(ROOT, "extension", "icons")
    assets = os.path.join(ROOT, "assets")
    os.makedirs(ext, exist_ok=True)
    os.makedirs(assets, exist_ok=True)
    blobs = {}
    for s in (16, 32, 48, 64, 128, 256):
        blobs[s] = png(s)
    for s in (16, 32, 48, 128):
        open(os.path.join(ext, f"icon{s}.png"), "wb").write(blobs[s])
    open(os.path.join(assets, "icon256.png"), "wb").write(blobs[256])
    open(os.path.join(assets, "rdm.ico"), "wb").write(ico([(s, blobs[s]) for s in (16, 32, 48, 64, 128, 256)]))
    print("icons written")


if __name__ == "__main__":
    main()
