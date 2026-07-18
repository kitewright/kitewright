#!/usr/bin/env python3
"""Render a terminal-style demo animation for Kitewright into PNG frames.
Pairs with ffmpeg (palettegen/paletteuse) to build a crisp looping GIF."""
import os, shutil
from PIL import Image, ImageDraw, ImageFont

OUT = "/tmp/kite-demo-frames"
W, H = 1240, 700
PAD = 30
BAR = 46                      # title-bar height
FS = 26                       # font size
LH = 38                       # line height
FONT = "/System/Library/Fonts/Menlo.ttc"

# Catppuccin-Mocha-ish palette
BG      = (30, 30, 46)
BARBG   = (24, 24, 37)
FG      = (205, 214, 244)
GREEN   = (166, 227, 161)     # prompt
BLUE    = (137, 180, 250)     # accent / urls
GRAY    = (127, 132, 156)     # comments
YELLOW  = (249, 226, 175)     # highlight numbers
MAUVE   = (203, 166, 247)
TEAL    = (148, 226, 213)
RED     = (243, 139, 168)
DOTS    = [(237, 106, 94), (245, 191, 79), (98, 197, 84)]

font  = ImageFont.truetype(FONT, FS, index=0)
fontb = ImageFont.truetype(FONT, FS, index=1)  # bold
CW = font.getbbox("M")[2]    # monospace char width

os.path.isdir(OUT) and shutil.rmtree(OUT)
os.makedirs(OUT)

# A "line" is a list of (text, color, bold) segments.
def seg(t, c=FG, b=False): return (t, c, b)

# The script: ("type", segments-for-typed-cmd) | ("out", [line,...]) | ("hold", frames) | ("clear",)
PROMPT = seg("$ ", GREEN, True)
SCRIPT = [
    ("out", [[seg("# Kitewright — browser automation for AI agents, one 7 MB binary", GRAY)]]),
    ("hold", 10),
    ("type", [PROMPT, seg("kite --version")]),
    ("out", [[seg("kite 0.1.0", TEAL)], []]),
    ("hold", 10),
    ("type", [PROMPT, seg("KITE_HEADLESS=1 kite &"), seg("   # start the MCP server", GRAY)]),
    ("out", [[seg("kitewright listening on ", FG), seg("http://127.0.0.1:8090/mcp", BLUE),
              seg("   (75 ms)", GRAY)], []]),
    ("hold", 14),
    ("type", [PROMPT, seg("ps -o rss= -p $!"), seg("   # idle footprint", GRAY)]),
    ("out", [[seg("kite idle RSS: ", FG), seg("8.0 MB", YELLOW, True),
              seg("   vs 100+ MB for Node + Playwright", GRAY)], []]),
    ("hold", 22),
    ("type", [PROMPT, seg("curl -s .../mcp"), seg("   # a real MCP endpoint", GRAY)]),
    ("out", [[seg("GET /mcp -> HTTP 406 ", FG), seg("(MCP handshake required)", GRAY)], []]),
    ("hold", 14),
    ("out", [[seg("# 23 tools: ", GRAY), seg("navigate", MAUVE), seg(" · ", GRAY),
              seg("screenshot", MAUVE), seg(" · ", GRAY), seg("extract", MAUVE), seg(" · ", GRAY),
              seg("fill_form", MAUVE), seg(" · ", GRAY), seg("pdf", MAUVE)]]),
    ("hold", 24),
    ("clear",),
    ("out", [[seg("# add it to any MCP client in one line:", GRAY)]]),
    ("hold", 6),
    ("type", [PROMPT, seg("claude mcp add kitewright -- npx -y @kitewright/mcp")]),
    ("out", [[seg("✓ added", GREEN, True)]]),
    ("hold", 46),
]

frames = []
buf = []            # committed lines (each a list of segments)

def draw(lines, typing=None, cursor=True):
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)
    # title bar
    d.rectangle([0, 0, W, BAR], fill=BARBG)
    for i, c in enumerate(DOTS):
        cx = 26 + i * 26
        d.ellipse([cx, BAR//2 - 7, cx + 14, BAR//2 + 7], fill=c)
    tt = "kitewright — zsh"
    d.text((W//2 - len(tt)*CW//2, BAR//2 - FS//2), tt, font=font, fill=GRAY)
    # body
    y = BAR + PAD
    alllines = lines + ([typing] if typing is not None else [])
    for ln in alllines:
        x = PAD
        for (t, c, b) in ln:
            d.text((x, y), t, font=(fontb if b else font), fill=c)
            x += len(t) * CW
        y += LH
    # cursor block at end of typing line
    if typing is not None and cursor:
        x = PAD + sum(len(t) for (t, _, _) in typing) * CW
        yy = BAR + PAD + (len(lines)) * LH
        d.rectangle([x, yy + 4, x + CW - 3, yy + FS + 4], fill=FG)
    return img

def emit(img):
    fn = os.path.join(OUT, f"f{len(frames):04d}.png")
    img.save(fn); frames.append(fn)

for step in SCRIPT:
    kind = step[0]
    if kind == "clear":
        buf = []
        emit(draw(buf))
    elif kind == "hold":
        for _ in range(step[1]):
            emit(draw(buf))
    elif kind == "out":
        for ln in step[1]:
            buf.append([seg(*s) if not isinstance(s, tuple) else s for s in ln] if False else ln)
        emit(draw(buf))
    elif kind == "type":
        target = step[1]
        # segments: first is prompt (instant), rest typed char by char (flattened)
        prompt_seg = [target[0]]
        typed_text = "".join(t for (t, _, _) in target[1:])
        typed_colors = []
        for (t, c, b) in target[1:]:
            typed_colors += [(c, b)] * len(t)
        n = 0
        while n <= len(typed_text):
            # build typing line = prompt + typed-so-far (colored per char, grouped)
            line = list(prompt_seg)
            if n > 0:
                # group consecutive same-color chars
                cur = typed_text[0:n]
                gi = 0
                while gi < n:
                    c, b = typed_colors[gi]
                    gj = gi
                    while gj < n and typed_colors[gj] == (c, b):
                        gj += 1
                    line.append(seg(cur[gi:gj], c, b))
                    gi = gj
            emit(draw(buf, typing=line))
            n += 3   # 3 chars per frame
        # commit the fully typed command as a normal line
        buf.append(list(prompt_seg) + list(target[1:]))
        emit(draw(buf, cursor=False))

print(f"rendered {len(frames)} frames to {OUT}")
