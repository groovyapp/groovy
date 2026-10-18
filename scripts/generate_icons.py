"""Regenerate the app icon's PNG sizes and Windows .ico from assets/icons/app-icon.svg.

Requires the resvg CLI (`cargo install resvg`). Run from anywhere:

    python scripts/generate_icons.py
"""

import struct
import subprocess
from pathlib import Path

ICONS = Path(__file__).resolve().parent.parent / "assets" / "icons"
SOURCE = ICONS / "app-icon.svg"

PNG_SIZES = [16, 24, 32, 48, 64, 128, 256, 512]
# Sizes Windows picks from for the taskbar, Alt+Tab, Explorer and titlebar.
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]


def render(size: int) -> Path:
    out = ICONS / f"app-icon-{size}.png"
    subprocess.run(
        ["resvg", "-w", str(size), "-h", str(size), str(SOURCE), str(out)],
        check=True,
    )
    return out


def write_ico(pngs: dict[int, bytes], out: Path) -> None:
    """Packs PNG images into an .ico (PNG entries are supported since Vista)."""
    header = struct.pack("<HHH", 0, 1, len(pngs))
    offset = len(header) + 16 * len(pngs)
    entries, images = b"", b""
    for size, data in pngs.items():
        # A dimension of 0 means 256.
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        images += data
        offset += len(data)
    out.write_bytes(header + entries + images)


def main() -> None:
    rendered = {size: render(size) for size in PNG_SIZES}
    write_ico({size: rendered[size].read_bytes() for size in ICO_SIZES}, ICONS / "app-icon.ico")
    print(f"Wrote {len(PNG_SIZES)} PNGs and app-icon.ico to {ICONS}")


if __name__ == "__main__":
    main()
