#!/usr/bin/env python3
"""Render the architecture diagrams as light/dark SVG pairs.

One source per diagram, two files out, so a GitHub `<picture>` element can pick
the reader's theme without the two variants drifting apart.
"""
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "docs/images/diagrams"

THEMES = {
    "light": dict(bg="#ffffff", surface="#f6f8fa", privileged="#fff8f0",
                  border="#d0d7de", strong="#1f2328", muted="#656d76",
                  accent="#1a7f37", warn="#bc4c00", wire="#8c959f"),
    "dark": dict(bg="#0d1117", surface="#161b22", privileged="#1c1610",
                 border="#30363d", strong="#e6edf3", muted="#8b949e",
                 accent="#3fb950", warn="#db6d28", wire="#6e7681"),
}

FONT = ("ui-monospace,SFMono-Regular,Menlo,Consolas,"
        "'Liberation Mono',monospace")
SANS = ("-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif")


def head(w, h, t):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}"
     viewBox="0 0 {w} {h}" role="img">
<rect width="{w}" height="{h}" fill="{t['bg']}"/>
<defs>
  <marker id="a" viewBox="0 0 10 10" refX="9" refY="5"
          markerWidth="7" markerHeight="7" orient="auto-start-reverse">
    <path d="M0,0 L10,5 L0,10 z" fill="{t['wire']}"/>
  </marker>
</defs>
'''


def box(x, y, w, h, t, fill=None, stroke=None, dash=None):
    fill = fill or t["surface"]
    stroke = stroke or t["border"]
    d = f' stroke-dasharray="{dash}"' if dash else ""
    return (f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="8" '
            f'fill="{fill}" stroke="{stroke}" stroke-width="1.5"{d}/>\n')


def text(x, y, s, t, size=13, color=None, anchor="start", font=FONT, weight="400"):
    color = color or t["strong"]
    s = s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    return (f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" '
            f'font-weight="{weight}" fill="{color}" text-anchor="{anchor}">{s}</text>\n')


def arrow(x1, y1, x2, y2, t):
    return (f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" '
            f'stroke="{t["wire"]}" stroke-width="1.5" marker-end="url(#a)"/>\n')


def privilege_split(t):
    W, H = 860, 470
    s = head(W, H, t)
    s += text(W / 2, 30, "Ownership is split at the login boundary", t,
              size=15, anchor="middle", font=SANS, weight="600")

    s += box(60, 52, 740, 74, t)
    s += text(80, 80, "plugins", t, size=14, weight="600")
    s += text(80, 102, "sandboxed WebAssembly components · one process each", t,
              size=12, color=t["muted"])
    s += text(780, 80, "no OS access", t, size=12, color=t["accent"], anchor="end")

    s += arrow(430, 126, 430, 168, t)
    s += text(444, 152, "private Wayland · surfaces, DMA-BUF", t, size=11,
              color=t["muted"])

    s += box(60, 170, 740, 108, t)
    s += text(80, 198, "touchbar-sessiond", t, size=14, weight="600")
    s += text(780, 198, "your user", t, size=12, color=t["accent"], anchor="end")
    s += text(80, 222, "profiles · layout · themes · GPU composition · gestures", t,
              size=12, color=t["muted"])
    s += text(80, 246, "no DRM master · no raw input · no uinput", t,
              size=12, color=t["muted"])
    s += text(80, 268, "the canvas follows the attached panel", t, size=11,
              color=t["muted"])

    s += arrow(330, 278, 330, 330, t)
    s += text(344, 300, "final frames", t, size=11, color=t["muted"])
    s += arrow(600, 330, 600, 278, t)
    s += text(614, 300, "normalized touch + Fn", t, size=11, color=t["muted"])

    s += box(60, 332, 740, 82, t, fill=t["privileged"], stroke=t["warn"])
    s += text(80, 360, "touchbard", t, size=14, weight="600")
    s += text(780, 360, "root", t, size=12, color=t["warn"], anchor="end")
    s += text(80, 384, "DRM · evdev · backlight · a uinput keyboard "
                       "limited to fixed system keys", t, size=12, color=t["muted"])
    s += text(80, 404, "keeps a media/Fn fallback when no session is attached", t,
              size=11, color=t["muted"])

    s += arrow(430, 414, 430, 442, t)
    s += text(W / 2, 460, "Touch Bar", t, size=12, color=t["muted"], anchor="middle")
    return s + "</svg>\n"


def sandbox(t):
    W, H = 860, 400
    s = head(W, H, t)
    s += text(W / 2, 30, "What a third-party plugin can reach", t, size=15,
              anchor="middle", font=SANS, weight="600")

    s += box(40, 56, 200, 150, t, stroke=t["accent"])
    s += text(60, 84, "component.wasm", t, size=13, weight="600")
    s += text(60, 108, "no filesystem", t, size=11, color=t["muted"])
    s += text(60, 128, "no network", t, size=11, color=t["muted"])
    s += text(60, 148, "no D-Bus", t, size=11, color=t["muted"])
    s += text(60, 168, "no environment", t, size=11, color=t["muted"])
    s += text(60, 190, "no raw handles", t, size=11, color=t["muted"])

    s += arrow(240, 118, 314, 118, t)
    s += text(277, 106, "WIT", t, size=11, color=t["muted"], anchor="middle")

    s += box(316, 56, 190, 150, t)
    s += text(336, 84, "plugin-host", t, size=13, weight="600")
    s += text(336, 108, "validates the UI tree", t, size=11, color=t["muted"])
    s += text(336, 128, "owns text, layout,", t, size=11, color=t["muted"])
    s += text(336, 146, "hit testing, GPU", t, size=11, color=t["muted"])
    s += text(336, 172, "resource budgets", t, size=11, color=t["muted"])

    s += arrow(506, 118, 580, 118, t)

    s += box(582, 56, 238, 150, t, stroke=t["warn"])
    s += text(602, 84, "supervisor", t, size=13, weight="600")
    s += text(602, 108, "matches the exact grant", t, size=11, color=t["muted"])
    s += text(602, 128, "before any work is queued", t, size=11, color=t["muted"])
    s += text(602, 154, "namespaces · seccomp", t, size=11, color=t["muted"])
    s += text(602, 174, "Landlock · cgroup budgets", t, size=11, color=t["muted"])
    s += text(602, 196, "append-only audit log", t, size=11, color=t["accent"])

    s += box(40, 236, 780, 108, t, dash="5 4")
    s += text(60, 262, "13 scoped capabilities, each denied until granted", t,
              size=12, weight="600")
    caps = [
        "filesystem.read · filesystem.write · http.request · dbus.call · dbus.subscribe",
        "command.run · clipboard.read · clipboard.write · secret.read · uri.open",
        "notification.send · local.connect · context.read",
    ]
    for i, line in enumerate(caps):
        s += text(60, 286 + i * 19, line, t, size=11, color=t["muted"])

    s += text(W / 2, 372, "A grant names the exact path, origin, bus member, or "
                          "command template — never a whole subsystem.", t,
              size=11.5, color=t["muted"], anchor="middle", font=SANS)
    return s + "</svg>\n"


def pipeline(t):
    W, H = 860, 250
    s = head(W, H, t)
    s += text(W / 2, 30, "One composed frame", t, size=15, anchor="middle",
              font=SANS, weight="600")
    stages = [
        ("plugin GLES", "each plugin renders\ninto its own buffer"),
        ("DMA-BUF", "handed over,\nnever copied"),
        ("composition", "one GPU pass,\nretained layers"),
        ("swapchain", "presenter-owned,\natomic flip"),
    ]
    x, w, gap = 40, 182, 24
    for i, (title, body) in enumerate(stages):
        bx = x + i * (w + gap)
        s += box(bx, 60, w, 96, t)
        s += text(bx + 16, 88, title, t, size=13, weight="600")
        for j, line in enumerate(body.split("\n")):
            s += text(bx + 16, 112 + j * 18, line, t, size=11, color=t["muted"])
        if i < len(stages) - 1:
            s += arrow(bx + w, 108, bx + w + gap - 4, 108, t)
    s += text(W / 2, 196, "A late plugin never blocks the strip: its last "
                          "completed buffer stays on screen.", t,
              size=12, color=t["muted"], anchor="middle", font=SANS)
    s += text(W / 2, 222, "Plugins render at 60 FPS. Physical presentation is "
                          "currently ~29.9 Hz, a limit of the ADP kernel driver.", t,
              size=11.5, color=t["muted"], anchor="middle", font=SANS)
    return s + "</svg>\n"


OUT.mkdir(parents=True, exist_ok=True)
for name, fn in (("privilege-split", privilege_split),
                 ("sandbox", sandbox),
                 ("frame-pipeline", pipeline)):
    for theme, palette in THEMES.items():
        path = OUT / f"{name}-{theme}.svg"
        path.write_text(fn(palette))
        print("wrote", path.relative_to(OUT.parent.parent.parent))
