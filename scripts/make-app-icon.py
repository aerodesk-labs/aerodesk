#!/usr/bin/env python3
"""生成 AeroDesk.app 占位图标（app-assets/icon-1024.png）。

零第三方依赖（标准库 zlib/struct 手写 PNG）：深蓝底 + 白色右箭头
（远程桌面"投送/方向"语义）。CI/本地均可复现；再由 make-app-icon.sh
用 sips + iconutil 合成 .icns。
用法: scripts/make-app-icon.py [输出路径]
"""
import struct, sys, zlib

def in_arrow(x: int, y: int) -> bool:
    # 箭头：水平条 + 右侧三角（顶点在左，向右张开），中心在 (512, 512)
    # 水平条：y 512±90，x 256..704
    if 422 <= y <= 602 and 256 <= x <= 704:
        return True
    # 三角形：顶点 (256,512)，底边 x=768，y 512±170
    if 342 <= y <= 682 and 256 <= x <= 768:
        # 归一化：t = (x-256)/(768-256)，半宽 = 170*t
        t = (x - 256) / 512.0
        half = 170.0 * t
        return abs(y - 512) <= half
    return False

def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "app-assets/icon-1024.png"
    w = h = 1024
    bg = (0x00, 0x71, 0xFF)   # 品牌蓝（与 Slint UI 主按钮色 #0071ff 一致）
    fg = (0xFF, 0xFF, 0xFF)   # 白色箭头
    rows = []
    for y in range(h):
        row = bytearray()
        for x in range(w):
            row += bytes(fg if in_arrow(x, y) else bg) + b"\xff"
        # PNG 每条扫描线必须以 1 字节 filter（0=None）开头，否则标准解码器会把
        # 首像素当作 filter 导致整行通道错位（生成彩虹马赛克）。
        rows.append(b"\x00" + bytes(row))
    raw = b"".join(rows)

    def chunk(tag: bytes, data: bytes) -> bytes:
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    with open(out, "wb") as f:
        f.write(png)
    print(f"== 完成: {out} ({w}x{h})")

if __name__ == "__main__":
    main()
