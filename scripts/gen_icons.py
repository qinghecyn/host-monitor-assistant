"""生成应用图标：深蓝底 + 白色监控折线，输出 PNG(32/128/256) 和 ICO。

纯标准库实现（zlib + struct），无需 Pillow。
用法: python scripts/gen_icons.py
"""
import struct
import zlib
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "shell" / "tauri" / "src-tauri" / "icons"


def png_chunk(tag, data):
    c = struct.pack(">I", len(data)) + tag + data
    return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)


def make_png(size):
    """画一个圆角深色方块 + 上升折线 + 空心圆点。"""
    px = [[(0, 0, 0, 0)] * size for _ in range(size)]
    r = max(2, size // 6)  # 圆角半径
    bg_a, bg_b = (23, 32, 48), (13, 17, 23)
    for y in range(size):
        for x in range(size):
            # 圆角判断
            for cx, cy in ((r, r), (size - 1 - r, r), (r, size - 1 - r), (size - 1 - r, size - 1 - r)):
                if (x < r or x > size - 1 - r) and (y < r or y > size - 1 - r):
                    if (x - cx) ** 2 + (y - cy) ** 2 > r * r:
                        continue
            t = (x + y) / (2 * size)
            col = tuple(int(a + (b - a) * t) for a, b in zip(bg_a, bg_b)) + (255,)
            px[y][x] = col

    def put(x, y, col, w=1.0):
        if 0 <= x < size and 0 <= y < size:
            base = px[y][x]
            px[y][x] = tuple(int(base[i] * (1 - w) + col[i] * w) for i in range(3)) + (255,)

    # 折线：从左下到右上的三段波形
    pts = [(0.16, 0.62), (0.32, 0.40), (0.46, 0.52), (0.62, 0.24), (0.84, 0.34)]
    line_w = max(1.5, size / 26)
    for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
        steps = int(size * 1.5)
        for i in range(steps + 1):
            t = i / steps
            fx, fy = x0 + (x1 - x0) * t, y0 + (y1 - y0) * t
            cx, cy = fx * size, fy * size
            for dy in range(int(line_w) + 2):
                for dx in range(int(line_w) + 2):
                    d = ((dx - line_w / 2) ** 2 + (dy - line_w / 2) ** 2) ** 0.5
                    if d <= line_w / 2 + 0.5:
                        w = min(1.0, line_w / 2 + 0.5 - d)
                        put(int(cx) + dx, int(cy) + dy, (61, 174, 233), w)
                        if dx or dy:
                            put(int(cx) - dx, int(cy) - dy, (61, 174, 233), w)
    # 数据点
    for fx, fy in pts:
        rr = max(2.0, size / 16)
        cx, cy = int(fx * size), int(fy * size)
        for dy in range(-int(rr) - 1, int(rr) + 2):
            for dx in range(-int(rr) - 1, int(rr) + 2):
                d = (dx * dx + dy * dy) ** 0.5
                if rr - 1 <= d <= rr:
                    put(cx + dx, cy + dy, (230, 237, 243), 0.9)

    raw = b"".join(b"\x00" + b"".join(bytes(p) for p in row) for row in px)
    return b"".join([
        b"\x89PNG\r\n\x1a\n",
        png_chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)),
        png_chunk(b"IDAT", zlib.compress(raw, 9)),
        png_chunk(b"IEND", b""),
    ])


def make_ico(pngs):
    """把多张 PNG 打包成 ICO（PNG-in-ICO，Vista+ 支持）。"""
    count = len(pngs)
    header = struct.pack("<HHH", 0, 1, count)
    offset = 6 + 16 * count
    entries = b""
    body = b""
    for size, data in pngs:
        entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32,
                               len(data), offset)
        body += data
        offset += len(data)
    return header + entries + body


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    sizes = [32, 128, 256]
    blobs = {}
    for s in sizes:
        blobs[s] = make_png(s)
        (OUT / ("%d.png" % (128 if s == 128 else s))).write_bytes(blobs[s])
    (OUT / "icon.png").write_bytes(blobs[256])
    (OUT / "icon.ico").write_bytes(make_ico([(s, blobs[s]) for s in sizes]))
    print("icons written to", OUT)


if __name__ == "__main__":
    main()
