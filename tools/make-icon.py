#!/usr/bin/env python3
"""从 assets/brand.svg 生成 assets/app.ico（16/24/32/48/64/128/256 多档）。

为什么用 Chrome：图标的 16 px 档必须是**矢量原生光栅化**，把 256 px 位图缩小
会得到一坨糊的圆角块；而本机没有任何 SVG 命令行光栅器（ImageMagick 的
`convert` 是 Windows 自带的磁盘工具，不是 IM）。

依赖：Chrome/Edge（走注册表 CHROME_EXECUTABLE → 常见安装路径逐级回退）、
Python 标准库。圆角外的四个角必须是透明的，所以截图开
`--default-background-color=00000000`。

用法：python tools/make-icon.py
"""

import os
import struct
import subprocess
import sys
import tempfile

SIZES = (16, 24, 32, 48, 64, 128, 256)
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SVG_PATH = os.path.join(ROOT, "assets", "brand.svg")
ICO_PATH = os.path.join(ROOT, "assets", "app.ico")


def find_browser() -> str:
    candidates = [
        os.environ.get("CHROME_EXECUTABLE"),
        os.path.expandvars(r"%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe"),
        os.path.expandvars(r"%USERPROFILE%\scoop\apps\googlechrome\current\chrome.exe"),
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ]
    for path in candidates:
        if path and os.path.isfile(path):
            return path
    sys.exit("找不到 Chrome/Edge，无法光栅化 SVG")


def render(browser: str, svg: str, size: int, out_png: str) -> None:
    """把 SVG 以 size×size 的视口截成 PNG（视口即像素，不做二次缩放）。"""
    page = (
        "<!doctype html><meta charset=utf-8>"
        "<style>*{margin:0;padding:0}html,body{"
        f"width:{size}px;height:{size}px;overflow:hidden}}</style>{svg}"
    )
    html_path = out_png[:-4] + ".html"
    with open(html_path, "w", encoding="utf-8") as handle:
        handle.write(page)
    url = "file:///" + os.path.abspath(html_path).replace("\\", "/")
    subprocess.run(
        [
            browser,
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--force-device-scale-factor=1",
            "--default-background-color=00000000",
            f"--window-size={size},{size}",
            f"--screenshot={out_png}",
            url,
        ],
        check=True,
        capture_output=True,
    )
    os.unlink(html_path)
    with open(out_png, "rb") as handle:
        header = handle.read(24)
    width, height = struct.unpack(">II", header[16:24])
    if (width, height) != (size, size):
        sys.exit(f"{size}px 档实际得到 {width}×{height}，Chrome 视口被钳制了")


def build_ico(entries: list[tuple[int, bytes]], dest: str) -> None:
    """ICO 容器：PNG 直接作为条目负载（Vista 起合法），圆角靠 alpha 保住。"""
    count = len(entries)
    out = bytearray(struct.pack("<HHH", 0, 1, count))
    offset = 6 + 16 * count
    for size, png in entries:
        # 256 在目录项里记作 0。
        byte = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", byte, byte, 0, 0, 1, 32, len(png), offset)
        offset += len(png)
    for _, png in entries:
        out += png
    with open(dest, "wb") as handle:
        handle.write(out)


def main() -> None:
    browser = find_browser()
    with open(SVG_PATH, encoding="utf-8") as handle:
        svg = handle.read()
    entries: list[tuple[int, bytes]] = []
    with tempfile.TemporaryDirectory() as work:
        for size in SIZES:
            png = os.path.join(work, f"icon-{size}.png")
            render(browser, svg, size, png)
            with open(png, "rb") as handle:
                entries.append((size, handle.read()))
            print(f"  {size:>3}px  {len(entries[-1][1]):>6} bytes")
    build_ico(entries, ICO_PATH)
    print(f"wrote {ICO_PATH}  {os.path.getsize(ICO_PATH)} bytes  ({len(entries)} sizes)")


if __name__ == "__main__":
    main()
