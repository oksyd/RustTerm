#!/usr/bin/env python3
"""Export logo variants from the application SVG and outlined wordmark.

Run with python3 tools/branding/export-logo.py. Requires python3-gi, python3-cairo,
python3-gi-cairo and gir1.2-rsvg-2.0; no display server or GPU is used.
"""

import argparse
from copy import deepcopy
from pathlib import Path
import struct
import xml.etree.ElementTree as ET

import cairo
import gi

gi.require_version("Rsvg", "2.0")
from gi.repository import Rsvg


ROOT = Path(__file__).resolve().parents[2]
BRAND = ROOT / "assets/brand"
SVG = "http://www.w3.org/2000/svg"
MARK_VIEWBOX = "72 72 368 368"
ET.register_namespace("", SVG)


def write_svg(path, root):
    ET.indent(root, space="  ")
    path.write_text(ET.tostring(root, encoding="unicode") + "\n")


def render_png(source, destination, width, height):
    surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, width, height)
    viewport = Rsvg.Rectangle()
    viewport.width, viewport.height = width, height
    Rsvg.Handle.new_from_file(str(source)).render_document(cairo.Context(surface), viewport)
    surface.write_to_png(str(destination))


def write_ico(destination, sizes):
    """Bundle existing RGBA PNGs without resampling; ICO supports up to 256 px."""
    images = [(size, (BRAND / f"png/rustterm-{size}.png").read_bytes())
              for size in sizes if size <= 256]
    header = struct.pack('<HHH', 0, 1, len(images))
    directory = bytearray()
    offset = len(header) + 16 * len(images)
    for size, data in images:
        directory.extend(struct.pack('<BBBBHHII', size % 256, size % 256,
                                     0, 0, 1, 32, len(data), offset))
        offset += len(data)
    destination.write_bytes(header + directory + b''.join(data for _, data in images))


def symbol(name, source):
    root = ET.parse(source).getroot()
    node = ET.Element("symbol", {"id": name, "viewBox": root.get("viewBox")})
    for child in root:
        if child.tag in (f"{{{SVG}}}title", f"{{{SVG}}}desc"):
            continue
        child = deepcopy(child)
        for element in child.iter():
            element.attrib.pop("id", None)
        node.append(child)
    return ET.tostring(node, encoding="unicode")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--preview-dir', type=Path, default=ROOT / 'dist/brand-preview')
    options = parser.parse_args()
    master = ROOT / "assets/io.rustterm.RustTerm.svg"
    mark = ET.parse(master).find(".//*[@id='mark']")
    wordmark = ET.parse(BRAND / "wordmark.svg").getroot()
    wordmark.find(f"{{{SVG}}}svg").set("viewBox", MARK_VIEWBOX)
    wordmark.find(".//*[@id='mark']").set("d", mark.get("d"))
    wordmark.find(".//*[@id='mark']").set("fill", "#ad5336")
    write_svg(BRAND / "wordmark.svg", wordmark)
    wordmark.find(".//*[@id='mark']").set("fill", mark.get("fill"))
    wordmark.find(".//*[@id='wordmark']").set("fill", "#ececf3")
    write_svg(BRAND / "wordmark-dark.svg", wordmark)

    for destination, color, size in (
        (BRAND / "mark.svg", "#ad5336", 320),
        (ROOT / "assets/icons/logo.svg", "#000000", 16),
    ):
        root = ET.Element(f"{{{SVG}}}svg", {
            "width": str(size), "height": str(size), "viewBox": MARK_VIEWBOX,
        })
        glyph = deepcopy(mark)
        glyph.attrib.pop("id")
        glyph.set("fill", color)
        root.append(glyph)
        write_svg(destination, root)

    (BRAND / "png").mkdir(exist_ok=True)
    sizes = (16, 24, 32, 48, 64, 128, 256, 512)
    for size in sizes:
        render_png(master, BRAND / f"png/rustterm-{size}.png", size, size)
    write_ico(BRAND / "rustterm.ico", sizes)

    symbols = "\n".join(symbol(name, path) for name, path in (
        ("app", master), ("logo", BRAND / "wordmark.svg"),
        ("logo-dark", BRAND / "wordmark-dark.svg"), ("mono", ROOT / "assets/icons/logo.svg"),
    ))
    # The monochrome symbol can be recolored for both background samples.
    symbols = symbols.replace('fill="#000000"', 'fill="currentColor"')
    samples = "\n".join(
        f'<use xlink:href="#app" x="{132 + i * 100 - size / 2}" y="{786 - size / 2}" '
        f'width="{size}" height="{size}"/>'
        f'<text x="{132 + i * 100}" y="890" class="size">{size}</text>'
        for i, size in enumerate(sizes[:6])
    )
    samples += "\n" + "\n".join(
        f'<use xlink:href="#mono" x="{132 + i * 100 - size / 2}" y="{851 - size / 2}" '
        f'width="{size}" height="{size}" color="#ad5336"/>'
        for i, size in enumerate(sizes[:4])
    )
    preview = f'''<svg xmlns="{SVG}" xmlns:xlink="http://www.w3.org/1999/xlink" width="1440" height="960" viewBox="0 0 1440 960" role="img" aria-labelledby="title description">
  <title id="title">RustTerm identity</title>
  <desc id="description">A faceted iron-oxide silhouette shaped around a terminal prompt and open cursor. Light and dark wordmarks, application icon, monochrome marks and actual-size icon samples.</desc>
  <defs>
    <style>
      text {{ font-family: 'Noto Sans', sans-serif; fill: #25262e; }}
      .label {{ font-size: 12px; letter-spacing: 1.5px; fill: #717581; }}
      .note {{ font-size: 15px; fill: #717581; }}
      .size {{ font-size: 12px; fill: #717581; text-anchor: middle; }}
    </style>
    {symbols}
  </defs>
  <rect width="1440" height="960" fill="#f5f2ef"/>
  <text x="48" y="50" class="label">RUSTTERM / VISUAL IDENTITY</text>
  <text x="1392" y="50" class="label" text-anchor="end">OXIDE + TERMINAL</text>
  <path d="M48 72H1392" stroke="#dddfe7"/>

  <rect x="48" y="104" width="856" height="304" rx="20" fill="#ffffff"/>
  <text x="80" y="146" class="label">PRIMARY WORDMARK</text>
  <use xlink:href="#logo" x="136" y="198" width="680" height="136"/>

  <rect x="48" y="432" width="856" height="208" rx="20" fill="#202124"/>
  <text x="80" y="474" class="label" style="fill:#a0a3b0">DARK SURFACES</text>
  <use xlink:href="#logo-dark" x="236" y="500" width="480" height="96"/>

  <rect x="928" y="104" width="464" height="536" rx="20" fill="#eee0d7"/>
  <text x="960" y="146" class="label">APPLICATION ICON</text>
  <use xlink:href="#app" x="976" y="186" width="368" height="368"/>
  <text x="1160" y="596" class="size">OXIDE / FACETED TERMINAL</text>

  <rect x="48" y="664" width="856" height="248" rx="20" fill="#ffffff"/>
  <text x="80" y="706" class="label">ACTUAL PIXEL SIZES</text>
  {samples}
  <text x="780" y="790" class="note">px</text>

  <rect x="928" y="664" width="464" height="248" rx="20" fill="#ffffff"/>
  <text x="960" y="706" class="label">ONE COLOR / SAME SILHOUETTE</text>
  <use xlink:href="#mono" x="994" y="744" width="120" height="120" color="#25262e"/>
  <rect x="1180" y="732" width="144" height="144" rx="20" fill="#25262e"/>
  <use xlink:href="#mono" x="1192" y="744" width="120" height="120" color="#ececf3"/>
</svg>
'''
    preview_dir = options.preview_dir
    (preview_dir / "assets").mkdir(parents=True, exist_ok=True)
    source = preview_dir / "logo.svg"
    source.write_text(preview)
    render_png(source, preview_dir / "assets/logo-preview.png", 1440, 960)
    print("Exported SVG variants, 8 PNG icon sizes, the Windows ICO and the logo preview.")


if __name__ == "__main__":
    main()
