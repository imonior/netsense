#!/usr/bin/env python3
"""生成 NetSense 图标集（纯 stdlib，无第三方依赖）。
产出：icons/32x32.png, 128x128.png, 128x128@2x.png, icon.png, icon.ico, icon.icns
"""
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(__file__), "..", "src-tauri", "icons")
os.makedirs(OUT, exist_ok=True)

# 颜色：深底 + 蓝色 N
BG = (30, 38, 50)
ACCENT = (47, 129, 247)
WHITE = (235, 240, 248)


def make_rgba(size, draw):
    w = h = size
    buf = bytearray(w * h * 4)
    for y in range(h):
        for x in range(w):
            r, g, b = BG
            buf[(y * w + x) * 4:(y * w + x) * 4 + 3] = bytes((r, g, b, 255))
    draw(w, h, buf)
    return bytes(buf)


def setpx(buf, w, x, y, color):
    if 0 <= x < w and 0 <= y < w:
        i = (y * w + x) * 4
        buf[i:i + 3] = bytes(color)


def thick_line(buf, w, x0, y0, x1, y1, t, color):
    # 简单加粗线段（Bresenham + 半径填充）
    dx = abs(x1 - x0)
    dy = abs(y1 - y0)
    sx = 1 if x0 < x1 else -1
    sy = 1 if y0 < y1 else -1
    err = dx - dy
    cx, cy = x0, y0
    rad = max(1, t // 2)
    while True:
        for oy in range(-rad, rad + 1):
            for ox in range(-rad, rad + 1):
                setpx(buf, w, cx + ox, cy + oy, color)
        if cx == x1 and cy == y1:
            break
        e2 = 2 * err
        if e2 > -dy:
            err -= dy
            cx += sx
        if e2 < dx:
            err += dx
            cy += sy


def draw_n(buf, w, h, color):
    t = max(2, w // 12)
    lx0, lx1 = int(w * 0.20), int(w * 0.34)
    rx0, rx1 = int(w * 0.66), int(w * 0.80)
    ty, by = int(h * 0.25), int(h * 0.75)
    for y in range(ty, by):
        for x in range(lx0, lx1):
            setpx(buf, w, x, y, color)
        for x in range(rx0, rx1):
            setpx(buf, w, x, y, color)
    thick_line(buf, w, lx0, by - 1, rx1, ty, t, color)


def png_bytes(size, rgba):
    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    # 每行前缀 filter=0
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)
        raw += rgba[y * stride:(y + 1) * stride]
    idat = zlib.compress(bytes(raw), 9)
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")


def write_png(path, size):
    rgba = make_rgba(size, lambda w, h, b: draw_n(b, w, h, WHITE))
    with open(path, "wb") as f:
        f.write(png_bytes(size, rgba))


def write_ico(path, size=256):
    rgba = make_rgba(size, lambda w, h, b: draw_n(b, w, h, WHITE))
    png = png_bytes(size, rgba)
    with open(path, "wb") as f:
        f.write(struct.pack("<HHH", 0, 1, 1))
        f.write(struct.pack("<BBBBHHII", size & 0xFF, size & 0xFF, 0, 0, 1, 32,
                            len(png), 6 + 16))
        f.write(png)


def write_icns(path):
    pngs = {
        b"ic07": png_bytes(128, make_rgba(128, lambda w, h, b: draw_n(b, w, h, WHITE))),
        b"ic08": png_bytes(256, make_rgba(256, lambda w, h, b: draw_n(b, w, h, WHITE))),
        b"ic09": png_bytes(512, make_rgba(512, lambda w, h, b: draw_n(b, w, h, WHITE))),
    }
    body = b""
    for typ, data in pngs.items():
        body += typ + struct.pack(">I", len(data) + 8) + data
    with open(path, "wb") as f:
        f.write(b"icns" + struct.pack(">I", len(body) + 8) + body)


def main():
    write_png(os.path.join(OUT, "32x32.png"), 32)
    write_png(os.path.join(OUT, "128x128.png"), 128)
    write_png(os.path.join(OUT, "128x128@2x.png"), 256)
    write_png(os.path.join(OUT, "icon.png"), 256)
    write_ico(os.path.join(OUT, "icon.ico"), 256)
    write_icns(os.path.join(OUT, "icon.icns"))
    print("icons generated at", os.path.abspath(OUT))


if __name__ == "__main__":
    main()
