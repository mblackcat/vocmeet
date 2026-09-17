#!/usr/bin/env python3
"""生成 VocMeet 的菜单栏托盘图标。

沿用应用的视觉母题：一组竖条波形（见 src/styles.css 顶部的设计宣言）。
- tray-idle.png      纯 alpha 单色，作 macOS template 图标，自动适配深浅色菜单栏
- tray-recording.png 砖红实心（--live #8E3A26），录制中高亮

输出 44×44（22pt @2x）。仓库里没有 Pillow，所以直接手写 PNG。
"""
import struct
import zlib
from pathlib import Path

SIZE = 44
# 五根竖条的高度（像素），中间高两侧低，读起来像一段波形
BAR_HEIGHTS = [16, 28, 40, 26, 14]
BAR_W = 4
GAP = 3
LIVE = (0x8E, 0x3A, 0x26)


def draw(rgb):
    """返回 SIZE×SIZE 的 RGBA 像素缓冲。rgb=None 表示画纯黑（template 用）。"""
    r, g, b = rgb if rgb else (0, 0, 0)
    px = [[(0, 0, 0, 0)] * SIZE for _ in range(SIZE)]

    total_w = len(BAR_HEIGHTS) * BAR_W + (len(BAR_HEIGHTS) - 1) * GAP
    x0 = (SIZE - total_w) // 2

    for i, h in enumerate(BAR_HEIGHTS):
        left = x0 + i * (BAR_W + GAP)
        top = (SIZE - h) // 2
        for y in range(top, top + h):
            for x in range(left, left + BAR_W):
                px[y][x] = (r, g, b, 255)
    return px


def write_png(path, px):
    raw = b"".join(
        b"\x00" + b"".join(struct.pack("BBBB", *p) for p in row) for row in px
    )

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    Path(path).write_bytes(png)
    print(f"  {path}  {len(png)} bytes")


if __name__ == "__main__":
    out = Path(__file__).resolve().parent.parent / "src-tauri" / "icons"
    print("生成托盘图标：")
    write_png(out / "tray-idle.png", draw(None))
    write_png(out / "tray-recording.png", draw(LIVE))
