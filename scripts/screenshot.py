#!/usr/bin/env python3
"""Makes docs/screenshot.png: the demo, in a terminal window on a wallpaper.

petsitter draws one frame of its demo (`--demo-frame`, made-up sessions)
as ANSI text; this script lays it out in an HTML page (a macOS-style window
over an abstract gradient) and has a headless Chrome take the picture.

    python3 scripts/screenshot.py [--chrome PATH] [--size 150x58]

With `--wallpaper WxH PATH` it only paints the wallpaper, for the recording
made by `scripts/demo.tape`.

Needs Python 3.9+, Cargo, Chrome or Chromium, and the Fira Code font.
"""

import argparse
import html
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "docs" / "screenshot.png"

# Catppuccin Mocha: the terminal's text, background and 16 colors.
FOREGROUND = "#cdd6f4"
BACKGROUND = "#1e1e2e"
ANSI = [
    "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
    "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
]

CHROME_PATHS = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "google-chrome",
    "chromium",
    "chromium-browser",
]

SGR = re.compile(r"\x1b\[([0-9;]*)m")

# Layout of the picture, in CSS pixels (the PNG is twice as large).
FONT_SIZE = 13
LINE_HEIGHT = 17
CELL_WIDTH = 7.8  # Fira Code advances 0.6 em
MARGIN = 96
TITLE_BAR = 38


def color(params, i):
    """Reads the color starting at params[i]; returns (css, next index)."""
    code = params[i]
    if code in (38, 48):
        if params[i + 1] == 5:
            n = params[i + 2]
            return (ANSI[n] if n < 16 else None), i + 3
        r, g, b = params[i + 2 : i + 5]
        return f"#{r:02x}{g:02x}{b:02x}", i + 5
    if 30 <= code <= 37 or 40 <= code <= 47:
        return ANSI[code % 10], i + 1
    if 90 <= code <= 97 or 100 <= code <= 107:
        return ANSI[8 + code % 10], i + 1
    return None, i + 1


def parse(ansi):
    """Splits ANSI text into lines of (text, style) runs."""
    lines = []
    for raw in ansi.rstrip("\n").split("\n"):
        runs, style, pos = [], {}, 0
        for match in SGR.finditer(raw):
            if match.start() > pos:
                runs.append((raw[pos : match.start()], dict(style)))
            pos = match.end()
            params = [int(p) for p in match.group(1).split(";") if p] or [0]
            i = 0
            while i < len(params):
                code = params[i]
                if code == 0:
                    style = {}
                    i += 1
                elif code in (1, 2, 3, 7):
                    style[{1: "bold", 2: "dim", 3: "italic", 7: "reverse"}[code]] = True
                    i += 1
                elif code in (39, 49):
                    style.pop("fg" if code == 39 else "bg", None)
                    i += 1
                else:
                    css, i = color(params, i)
                    if css:
                        style["bg" if code in (48,) or 40 <= code <= 47 or 100 <= code <= 107 else "fg"] = css
        if pos < len(raw):
            runs.append((raw[pos:], dict(style)))
        lines.append(runs)
    return lines


# Braille dots, (column, row) of each bit of a cell.
BRAILLE_DOTS = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)]
# Box-drawing lines through a cell: which of up, down, left, right they reach.
BOX = {
    "│": "ud", "─": "lr", "└": "ur", "┌": "dr", "┐": "dl", "┘": "ul", "├": "udr", "┤": "udl",
    "┬": "dlr", "┴": "ulr", "┼": "udlr", "╭": "dr", "╮": "dl", "╯": "ul", "╰": "ur",
}


def glyph(char, fg):
    """A cell's content: braille and box lines drawn as shapes, so they are
    crisp and join up whatever the font; other characters as text."""
    w, h = CELL_WIDTH, LINE_HEIGHT
    code = ord(char)
    if 0x2800 < code <= 0x28FF:
        dots = "".join(
            f'<circle cx="{w * (0.3 + 0.4 * x):.2f}" cy="{h * (0.14 + 0.24 * y):.2f}" r="1.25"/>'
            for bit, (x, y) in enumerate(BRAILLE_DOTS)
            if (code - 0x2800) >> bit & 1
        )
        return f'<svg width="{w}" height="{h}" fill="{fg}">{dots}</svg>'
    if char in BOX:
        cx, cy, ends = w / 2, h / 2, BOX[char]
        points = {"u": (cx, 0), "d": (cx, h), "l": (0, cy), "r": (w, cy)}
        lines = "".join(
            f'<line x1="{cx}" y1="{cy}" x2="{points[e][0]}" y2="{points[e][1]}"/>' for e in ends
        )
        return f'<svg width="{w}" height="{h}" stroke="{fg}" stroke-width="1.1">{lines}</svg>'
    return html.escape(char)


def cell(char, style):
    fg, bg = style.get("fg", FOREGROUND), style.get("bg")
    if style.get("reverse"):
        fg, bg = bg or BACKGROUND, fg
    css = [f"color:{fg}"]
    if bg:
        css.append(f"background:{bg}")
    if style.get("bold"):
        css.append("font-weight:600")
    if style.get("dim"):
        css.append("opacity:.55")
    if style.get("italic"):
        css.append("font-style:italic")
    return f'<i style="{";".join(css)}">{glyph(char, fg)}</i>'


# The wallpaper: soft color blobs over a pastel gradient.
WALLPAPER = """
      radial-gradient(ellipse 60% 55% at 12% 18%, #f5a97f 0%, transparent 60%),
      radial-gradient(ellipse 55% 60% at 88% 12%, #c6a0f6 0%, transparent 62%),
      radial-gradient(ellipse 70% 60% at 80% 92%, #7dc4e4 0%, transparent 60%),
      radial-gradient(ellipse 60% 70% at 8% 88%, #ee99a0 0%, transparent 58%),
      linear-gradient(135deg, #8aadf4 0%, #b7bdf8 50%, #f0c6c6 100%)"""


def page(lines, cols, rows):
    width = round(cols * CELL_WIDTH) + 2 * 18
    height = rows * LINE_HEIGHT + TITLE_BAR + 2 * 12
    body = "\n".join(
        "<div>" + "".join(cell(c, style) for text, style in runs for c in text) + "</div>"
        for runs in lines
    )
    return f"""<!doctype html>
<html><head><meta charset="utf-8"><style>
  html, body {{ margin: 0; }}
  body {{
    width: {width + 2 * MARGIN}px; height: {height + 2 * MARGIN}px;
    display: flex; align-items: center; justify-content: center;
    background: {WALLPAPER};
  }}
  .window {{
    width: {width}px; height: {height}px; box-sizing: border-box;
    background: {BACKGROUND}; border-radius: 12px; overflow: hidden;
    border: 1px solid rgba(255, 255, 255, .12);
    box-shadow: 0 32px 90px rgba(17, 17, 27, .55), 0 8px 24px rgba(17, 17, 27, .35);
  }}
  .bar {{
    height: {TITLE_BAR}px; position: relative;
    display: flex; align-items: center; padding-left: 16px; gap: 8px;
  }}
  .dot {{ width: 12px; height: 12px; border-radius: 50%; }}
  .title {{
    position: absolute; inset: 0; display: flex; align-items: center; justify-content: center;
    color: #9399b2; font: 500 13px -apple-system, "Helvetica Neue", sans-serif;
  }}
  .screen {{
    padding: 0 18px 12px;
    font: {FONT_SIZE}px/{LINE_HEIGHT}px "Fira Code", "JetBrains Mono", Menlo, monospace;
    font-variant-ligatures: none; color: {FOREGROUND};
  }}
  .screen div {{ height: {LINE_HEIGHT}px; white-space: nowrap; }}
  /* Every character fills exactly one cell, whatever font draws it. */
  .screen i {{
    display: inline-block; width: {CELL_WIDTH}px; height: {LINE_HEIGHT}px;
    vertical-align: top; text-align: center; font-style: normal; overflow: visible;
  }}
  .screen svg {{ display: block; }}
</style></head><body>
<div class="window">
  <div class="bar">
    <div class="dot" style="background:#ff5f57"></div>
    <div class="dot" style="background:#febc2e"></div>
    <div class="dot" style="background:#28c840"></div>
    <div class="title">petsitter — ~/code</div>
  </div>
  <div class="screen">{body}</div>
</div>
</body></html>
""", width + 2 * MARGIN, height + 2 * MARGIN


def find_chrome(explicit):
    for candidate in [explicit] if explicit else CHROME_PATHS:
        path = candidate if os.path.isabs(candidate) else shutil.which(candidate)
        if path and os.path.exists(path):
            return path
    sys.exit("Chrome or Chromium not found; pass --chrome PATH")


def shoot(chrome, document, width, height, output, scale=2):
    """Has a headless Chrome render `document` to `output`."""
    with tempfile.TemporaryDirectory() as tmp:
        source = Path(tmp) / "page.html"
        source.write_text(document, encoding="utf-8")
        subprocess.run(
            [
                chrome, "--headless=new", "--disable-gpu", "--hide-scrollbars",
                f"--force-device-scale-factor={scale}", f"--window-size={width},{height}",
                f"--screenshot={output}", source.as_uri(),
            ],
            check=True, capture_output=True,
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--chrome", help="path to Chrome or Chromium")
    parser.add_argument("--size", default="150x58", help="terminal size, COLSxROWS")
    parser.add_argument("--wallpaper", nargs=2, metavar=("WxH", "PATH"),
                        help="only paint the wallpaper, W by H pixels, to PATH")
    args = parser.parse_args()
    chrome = find_chrome(args.chrome)

    if args.wallpaper:
        size, path = args.wallpaper
        width, height = (int(n) for n in size.split("x"))
        document = f"<!doctype html><body style='margin:0;width:{width}px;height:{height}px;background:{WALLPAPER}'>"
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        shoot(chrome, document, width, height, Path(path).resolve(), scale=1)
        print(f"wrote {path}")
        return

    cols, rows = (int(n) for n in args.size.split("x"))

    ansi = subprocess.run(
        ["cargo", "run", "--quiet", "--", "--demo-frame", args.size],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    document, width, height = page(parse(ansi), cols, rows)

    OUTPUT.parent.mkdir(exist_ok=True)
    shoot(chrome, document, width, height, OUTPUT)
    print(f"wrote {OUTPUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
