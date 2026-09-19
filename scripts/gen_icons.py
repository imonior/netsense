#!/usr/bin/env python3
"""Generate the NetSense icon set from the artwork master (pure stdlib, no third-party deps).

Input
    app-icon.png   square (>= 1024 px), 8-bit RGB/RGBA, non-interlaced PNG.
                   This is the framed artwork master: the art is already positioned
                   the way it should fill the icon. Replace it when the art changes.
                   `npx tauri icon app-icon.png` would produce an equivalent set.

Output (src-tauri/icons/)
    32x32.png, 128x128.png, 128x128@2x.png (256), icon.png (1024)
    icon.ico    16/24/32/48/64/128 as BMP frames + 256 as a PNG frame
    icon.icns   ic07..ic14 (the same chunk set the Rust `icns` crate emits)

Rounding is applied at the target size (radius = ROUND_RATIO * size) so the corners
stay crisp at every scale. Everything below — PNG decode/encode, area resampling,
the rounded-corner mask and the ICO/ICNS containers — is implemented here.
Expect roughly half a minute for a 1024 px master.
"""
import argparse
import math
import os
import struct
import sys
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_SRC = os.path.join(ROOT, "app-icon.png")
DEFAULT_OUT = os.path.join(ROOT, "src-tauri", "icons")

ROUND_RATIO = 0.18  # corner radius as a fraction of the icon size

PNG_TARGETS = (("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256))
MASTER_OUT = ("icon.png", 1024)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
ICO_PNG_FROM = 256  # this size and above are stored as PNG frames inside the .ico
ICNS_CHUNKS = (
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic13", 256),
    (b"ic14", 512),
)


# --------------------------------------------------------------------------- PNG


def _paeth(a, b, c):
    p = a + b - c
    pa = abs(p - a)
    pb = abs(p - b)
    pc = abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def read_png(path):
    """Decode an 8-bit RGB/RGBA non-interlaced PNG into (w, h, RGBA bytes)."""
    with open(path, "rb") as fh:
        data = fh.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("%s: not a PNG" % path)

    pos = 8
    idat = []
    w = h = depth = ctype = None
    while pos + 8 <= len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        tag = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if tag == b"IHDR":
            w, h, depth, ctype, _comp, _filt, interlace = struct.unpack(">IIBBBBB", body)
            if depth != 8:
                raise ValueError("%s: only 8-bit PNG is supported" % path)
            if interlace:
                raise ValueError("%s: interlaced PNG is not supported" % path)
        elif tag == b"IDAT":
            idat.append(body)
        elif tag == b"IEND":
            break

    if ctype not in (2, 6):
        raise ValueError("%s: only RGB/RGBA PNG is supported" % path)
    nch = 3 if ctype == 2 else 4
    raw = zlib.decompress(b"".join(idat))
    stride = w * nch
    out = bytearray(w * h * nch)
    prev = bytearray(stride)

    p = 0
    for y in range(h):
        ft = raw[p]
        p += 1
        line = bytearray(raw[p:p + stride])
        p += stride
        if ft == 1:
            for i in range(nch, stride):
                line[i] = (line[i] + line[i - nch]) & 0xFF
        elif ft == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ft == 3:
            for i in range(stride):
                a = line[i - nch] if i >= nch else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif ft == 4:
            for i in range(stride):
                a = line[i - nch] if i >= nch else 0
                c = prev[i - nch] if i >= nch else 0
                line[i] = (line[i] + _paeth(a, prev[i], c)) & 0xFF
        elif ft != 0:
            raise ValueError("%s: unknown filter %d" % (path, ft))
        out[y * stride:(y + 1) * stride] = line
        prev = line

    if nch == 4:
        return w, h, bytes(out)

    rgba = bytearray(w * h * 4)
    rgba[0::4] = out[0::3]
    rgba[1::4] = out[1::3]
    rgba[2::4] = out[2::3]
    rgba[3::4] = b"\xff" * (w * h)
    return w, h, bytes(rgba)


# Sum of absolute signed values — the standard PNG filter heuristic.
_SIGNED = bytes(min(i, 256 - i) for i in range(256))


def _png_chunk(tag, payload):
    body = tag + payload
    return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)


def _filter_line(line, prev, kind):
    n = len(line)
    if kind == 0:
        return line
    if kind == 1:
        return bytes((line[i] - (line[i - 4] if i >= 4 else 0)) & 0xFF for i in range(n))
    return bytes((line[i] - prev[i]) & 0xFF for i in range(n))


def _png_bytes(size, rgba):
    """Encode RGBA as a PNG, choosing the cheapest of the None/Sub/Up filters per row."""
    stride = size * 4
    raw = bytearray()
    prev = bytes(stride)
    for y in range(size):
        line = bytes(rgba[y * stride:(y + 1) * stride])
        best = None
        for kind in (0, 1, 2):
            cand = _filter_line(line, prev, kind)
            score = sum(cand.translate(_SIGNED))
            if best is None or score < best[0]:
                best = (score, kind, cand)
        raw.append(best[1])
        raw += best[2]
        prev = line

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n"
            + _png_chunk(b"IHDR", ihdr)
            + _png_chunk(b"IDAT", zlib.compress(bytes(raw), 9))
            + _png_chunk(b"IEND", b""))


def write_png(path, size, rgba):
    blob = _png_bytes(size, rgba)
    with open(path, "wb") as fh:
        fh.write(blob)
    return len(blob)


# ---------------------------------------------------------------------- geometry


def _weights(src_len, dst_len):
    """Box-filter contributions: for each destination index, (first_src, [(offset, cov)])."""
    scale = src_len / dst_len
    table = []
    for i in range(dst_len):
        start = i * scale
        end = start + scale
        lo = int(math.floor(start))
        hi = min(int(math.ceil(end)), src_len)
        acc = []
        for j in range(lo, hi):
            cov = min(end, j + 1) - max(start, j)
            if cov > 1e-9:
                acc.append((j - lo, cov))
        table.append((lo, acc))
    return table


def resize(rgba, sw, sh, dw, dh):
    """Area-average resample (separable). The source must be opaque."""
    if (sw, sh) == (dw, dh):
        return rgba
    xw = _weights(sw, dw)
    yw = _weights(sh, dh)
    sw4 = sw * 4
    res = bytearray(dw * dh * 4)
    for y in range(dh):
        j0, col = yw[y]
        total = sum(c for _, c in col)
        tmp = [0.0] * sw4
        for off, cov in col:
            base = (j0 + off) * sw4
            row = rgba[base:base + sw4]
            w = cov / total
            tmp = [t + v * w for t, v in zip(tmp, row)]

        o = y * dw * 4
        for x in range(dw):
            i0, row = xw[x]
            total_x = sum(c for _, c in row)
            acc = [0.0, 0.0, 0.0, 0.0]
            for off, cov in row:
                b = (i0 + off) * 4
                w = cov / total_x
                acc[0] += tmp[b] * w
                acc[1] += tmp[b + 1] * w
                acc[2] += tmp[b + 2] * w
                acc[3] += tmp[b + 3] * w
            for c in range(4):
                v = int(acc[c] + 0.5)
                res[o + x * 4 + c] = 255 if v > 255 else (0 if v < 0 else v)
    return bytes(res)


def round_corners(rgba, size, radius):
    """Multiply alpha by a rounded-rectangle coverage (1 px analytic soft edge)."""
    out = bytearray(rgba)
    half = size / 2.0
    for y in range(size):
        dy = abs(y + 0.5 - half) - (half - radius)
        if dy < 0.0:
            dy = 0.0
        row = y * size * 4
        for x in range(size):
            dx = abs(x + 0.5 - half) - (half - radius)
            if dx < 0.0:
                dx = 0.0
            cov = 0.5 - (math.hypot(dx, dy) - radius)
            if cov >= 1.0:
                continue
            i = row + x * 4 + 3
            if cov <= 0.0:
                out[i] = 0
            else:
                out[i] = int(out[i] * cov + 0.5)
    return bytes(out)


# ------------------------------------------------------------------- containers


def ico_bmp_frame(size, rgba):
    """BITMAPINFOHEADER + bottom-up BGRA + 1bpp AND mask (bit set = transparent)."""
    mask_row = ((size + 31) // 32) * 4
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0,
                         size * size * 4 + mask_row * size, 0, 0, 0, 0)

    body = bytearray()
    for y in range(size - 1, -1, -1):
        base = y * size * 4
        row = bytearray(size * 4)
        row[0::4] = rgba[base + 2:base + size * 4:4]
        row[1::4] = rgba[base + 1:base + size * 4:4]
        row[2::4] = rgba[base:base + size * 4:4]
        row[3::4] = rgba[base + 3:base + size * 4:4]
        body += row

    mask = bytearray(mask_row * size)
    for y in range(size):
        base = y * size * 4
        off = y * mask_row
        for x in range(size):
            if rgba[base + x * 4 + 3] < 128:
                mask[off + (x >> 3)] |= 0x80 >> (x & 7)
    for y in range(size - 1, -1, -1):
        body += mask[y * mask_row:(y + 1) * mask_row]

    return header + bytes(body)


def write_ico(path, frames):
    """frames: list of (size, payload bytes); ascending size order."""
    header = struct.pack("<HHH", 0, 1, len(frames))
    offset = 6 + 16 * len(frames)
    entries = bytearray()
    for size, payload in frames:
        d = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", d, d, 0, 0, 1, 32, len(payload), offset)
        offset += len(payload)
    with open(path, "wb") as fh:
        fh.write(header + bytes(entries) + b"".join(p for _, p in frames))


def write_icns(path, chunks):
    """chunks: list of (4-byte type, PNG payload)."""
    body = bytearray()
    for tag, payload in chunks:
        body += tag + struct.pack(">I", len(payload) + 8) + payload
    with open(path, "wb") as fh:
        fh.write(b"icns" + struct.pack(">I", len(body) + 8) + bytes(body))


# ------------------------------------------------------------------------- main


def main(argv=None):
    ap = argparse.ArgumentParser(description="Generate the NetSense icon set.")
    ap.add_argument("--source", default=DEFAULT_SRC, help="artwork master (default: app-icon.png)")
    ap.add_argument("--out", default=DEFAULT_OUT, help="output directory (default: src-tauri/icons)")
    ap.add_argument("--radius", type=float, default=ROUND_RATIO,
                    help="corner radius as a fraction of the icon size (default: 0.18; 0 = square)")
    args = ap.parse_args(argv)

    if not os.path.isfile(args.source):
        ap.error("artwork master not found: %s" % args.source)

    w, h, rgba = read_png(args.source)
    if w != h:
        ap.error("artwork master must be square, got %dx%d" % (w, h))
    if rgba[3::4].count(255) != w * h:
        ap.error("artwork master must be fully opaque")

    os.makedirs(args.out, exist_ok=True)

    cache = {}

    def render(size):
        """Resize + rounded mask at the requested size (each size rendered once)."""
        if size not in cache:
            px = resize(rgba, w, h, size, size)
            radius = args.radius * size
            cache[size] = round_corners(px, size, radius) if radius > 0 else px
        return cache[size]

    report = []

    def note(name, detail, nbytes):
        report.append((name, detail, nbytes))

    for name, size in PNG_TARGETS:
        note(name, "%dx%d" % (size, size),
             write_png(os.path.join(args.out, name), size, render(size)))

    master_name, master_size = MASTER_OUT
    note(master_name, "%dx%d" % (master_size, master_size),
         write_png(os.path.join(args.out, master_name), master_size, render(master_size)))

    frames = []
    for size in sorted(ICO_SIZES):
        px = render(size)
        payload = _png_bytes(size, px) if size >= ICO_PNG_FROM else ico_bmp_frame(size, px)
        frames.append((size, payload))
    write_ico(os.path.join(args.out, "icon.ico"), frames)
    note("icon.ico", "%d frames" % len(frames), os.path.getsize(os.path.join(args.out, "icon.ico")))

    chunks = [(tag, _png_bytes(size, render(size))) for tag, size in ICNS_CHUNKS]
    write_icns(os.path.join(args.out, "icon.icns"), chunks)
    note("icon.icns", "%d chunks" % len(chunks), os.path.getsize(os.path.join(args.out, "icon.icns")))

    print("NetSense icons <- %s (%dx%d, corner radius %.0f%%)"
          % (os.path.basename(args.source), w, h, args.radius * 100))
    for name, detail, nbytes in report:
        print("  %-16s %-12s %9d bytes" % (name, detail, nbytes))
    return 0


if __name__ == "__main__":
    sys.exit(main())
