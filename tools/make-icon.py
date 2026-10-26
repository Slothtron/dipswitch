#!/usr/bin/env python3
"""从 assets/brand.svg 生成 assets/app.ico（16/24/32/48/64/128/256 多档）。

这是**维护者的显式步骤，不是构建的一部分**：`build.rs` 只消费生成好的 `app.ico`，
不生成它。否则构建就要依赖 Python 加一个 SVG 光栅器，与「离线构建」的验收项冲突。
改了 assets/brand.svg 之后必须重跑本脚本，否则 exe 里还是旧图标而构建不会报错。

光栅器选择（不写死任何机器专属路径）：
1. `DIPSWITCH_RASTERIZER` —— 显式指定可执行文件，CI 与非标准安装用；
2. `rsvg-convert` —— 规范做法，`-w/-h` 直接按目标像素原生光栅化；
3. 浏览器无头截图 —— 只有 Chrome/Edge 系可用，按 size×size 视口截图等效于原生光栅化。
   把 256 px 位图缩小到 16 px 会得到一坨糊的圆角块，所以小尺寸必须原生出图。

注意：Windows 自带的 `convert.exe` 是 FAT 转 NTFS 的磁盘工具，不是 ImageMagick，
不要把它当光栅器用（本脚本因此不尝试 `convert`）。

用法：
    python tools/make-icon.py                      # 重新生成 assets/app.ico
    python tools/make-icon.py --verify <ico> <exe> # 断言每一档都进了 exe
"""

import os
import shutil
import struct
import subprocess
import sys
import tempfile

SIZES = (16, 24, 32, 48, 64, 128, 256)
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SVG_PATH = os.path.join(ROOT, "assets", "brand.svg")
ICO_PATH = os.path.join(ROOT, "assets", "app.ico")

BROWSER_NAMES = (
    "chrome",
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "microsoft-edge",
    "msedge",
)

# 平台标准安装位置（不是某台机器的私有布局），用环境变量展开，跨用户可移植。
BROWSER_FALLBACKS = (
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    r"%PROGRAMFILES(X86)%/Microsoft/Edge/Application/msedge.exe",
    r"%PROGRAMFILES%/Microsoft/Edge/Application/msedge.exe",
    r"%PROGRAMFILES%/Google/Chrome/Application/chrome.exe",
    r"%PROGRAMFILES(X86)%/Google/Chrome/Application/chrome.exe",
)


class Rasterizer:
    """一个 SVG → PNG 的后端。"""

    def __init__(self, exe: str, kind: str):
        self.exe = exe
        self.kind = kind

    def render(self, svg_file: str, size: int, out_png: str) -> None:
        if self.kind == "rsvg-convert":
            subprocess.run(
                [self.exe, "-w", str(size), "-h", str(size), "-o", out_png, svg_file],
                check=True,
                capture_output=True,
            )
            return
        self._render_via_browser(svg_file, size, out_png)

    def _render_via_browser(self, svg_file: str, size: int, out_png: str) -> None:
        """把 SVG 塞进一个 size×size 的视口截图：视口即像素，不做二次缩放。"""
        with open(svg_file, encoding="utf-8") as handle:
            body = handle.read()
        page = (
            "<!doctype html><meta charset=utf-8>"
            "<style>*{margin:0;padding:0}html,body{"
            f"width:{size}px;height:{size}px;overflow:hidden}}</style>{body}"
        )
        html_path = os.path.join(tempfile.gettempdir(), f"dipswitch-{size}.html")
        with open(html_path, "w", encoding="utf-8") as handle:
            handle.write(page)
        try:
            subprocess.run(
                [
                    self.exe,
                    "--headless=new",
                    "--disable-gpu",
                    "--hide-scrollbars",
                    "--force-device-scale-factor=1",
                    "--default-background-color=00000000",
                    f"--window-size={size},{size}",
                    f"--screenshot={out_png}",
                    "file:///" + os.path.abspath(html_path).replace("\\", "/"),
                ],
                check=True,
                capture_output=True,
            )
        finally:
            os.unlink(html_path)

    def verify(self, out_png: str, size: int) -> None:
        """断言产物正好是 size×size：视口被钳制或后端偷换缩放时立刻暴露。"""
        with open(out_png, "rb") as handle:
            header = handle.read(24)
        if header[:8] != b"\x89PNG\r\n\x1a\n":
            sys.exit(f"{out_png} 不是 PNG")
        width, height = struct.unpack(">II", header[16:24])
        if (width, height) != (size, size):
            sys.exit(f"{size}px 档实际得到 {width}×{height}（后端 {self.kind}）")


def find_rasterizer() -> Rasterizer:
    explicit = os.environ.get("DIPSWITCH_RASTERIZER")
    if explicit:
        exe = os.path.expanduser(explicit)
        if not os.path.isfile(exe):
            sys.exit(f"DIPSWITCH_RASTERIZER 指向的文件不存在：{exe}")
        kind = "rsvg-convert" if "rsvg-convert" in os.path.basename(exe) else "browser"
        return Rasterizer(exe, kind)

    if rsvg := shutil.which("rsvg-convert"):
        return Rasterizer(rsvg, "rsvg-convert")
    for name in BROWSER_NAMES:
        if found := shutil.which(name):
            return Rasterizer(found, "browser")
    for fallback in BROWSER_FALLBACKS:
        path = os.path.expandvars(os.path.expanduser(fallback))
        if os.path.isfile(path):
            return Rasterizer(path, "browser")
    sys.exit(
        "找不到可用的 SVG 光栅器。装一个 librsvg（rsvg-convert）或 Chrome/Edge，"
        "或用 DIPSWITCH_RASTERIZER 指定可执行文件路径。"
    )


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


def read_ico_entries(path: str) -> list[tuple[int, bytes]]:
    """解析 .ico 的目录项，返回 (边长, PNG 负载)。"""
    with open(path, "rb") as handle:
        blob = handle.read()
    count = struct.unpack("<H", blob[4:6])[0]
    entries = []
    for index in range(count):
        entry = blob[6 + 16 * index : 6 + 16 * (index + 1)]
        width = entry[0] or 256
        size, offset = struct.unpack("<II", entry[8:16])
        entries.append((width, blob[offset : offset + size]))
    return entries


def verify_exe(ico_path: str, exe_path: str) -> int:
    """确认 .ico 的每一档都原样进了 exe。

    rc.exe 把 PNG 型 ICO 条目的负载原样存进 RT_ICON，所以直接搜字节就是最硬的证据。
    不要用 `ExtractAssociatedIcon` 判断成败：它只挑「最合适」的一枚，遇到 PNG 压缩
    条目会返回 null，会把好的产物误判成坏的。
    """
    entries = read_ico_entries(ico_path)
    with open(exe_path, "rb") as handle:
        exe = handle.read()
    missing = [size for size, png in entries if exe.find(png) < 0]
    for size, _ in entries:
        mark = "absent" if size in missing else "embedded"
        print(f"  {size:>3}px  {mark}")
    print(f"{exe_path}: {len(entries) - len(missing)}/{len(entries)} icon entries embedded")
    if b"dpiAware" in exe:
        print("  manifest present (gpui embeds RT_MANIFEST; our .rc must not)")
    return 1 if missing else 0


def main(argv: list[str]) -> int:
    if len(argv) > 1 and argv[1] == "--verify":
        if len(argv) != 4:
            sys.exit("用法：python tools/make-icon.py --verify <app.ico> <app.exe>")
        return verify_exe(argv[2], argv[3])

    rasterizer = find_rasterizer()
    print(f"rasterizer: {rasterizer.kind}  ({rasterizer.exe})")
    entries: list[tuple[int, bytes]] = []
    with tempfile.TemporaryDirectory() as work:
        for size in SIZES:
            png = os.path.join(work, f"icon-{size}.png")
            rasterizer.render(SVG_PATH, size, png)
            rasterizer.verify(png, size)
            with open(png, "rb") as handle:
                blob = handle.read()
            entries.append((size, blob))
            print(f"  {size:>3}px  {len(blob):>6} bytes")
    build_ico(entries, ICO_PATH)
    print(f"wrote {ICO_PATH}  {os.path.getsize(ICO_PATH)} bytes  ({len(entries)} sizes)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
