# -*- coding: utf-8 -*-
"""从 D:/xiaozhushou_monitor/logo.png 生成安卓全套启动图标（覆盖 chaquopy-app 的 res）。

自适应图标：背景纯黑（logo 自带黑底），前景徽标缩到 62% 安全区，圆形/方圆裁剪都不切内容；
传统图标（Android 7.1-）：黑底方形 + 圆形遮罩两套。
"""
import os
from PIL import Image, ImageDraw

SRC = r"D:\xiaozhushou_monitor\logo.png"
RES = r"D:\xiaozhushou_monitor\shell\chaquopy-app\app\src\main\res"

src = Image.open(SRC).convert("RGBA")
w, h = src.size
s = min(w, h)
badge = src.crop(((w - s) // 2, (h - s) // 2, (w + s) // 2, (h + s) // 2))  # 居中方裁

LEGACY = {"mdpi": 48, "hdpi": 72, "xhdpi": 96, "xxhdpi": 144, "xxxhdpi": 192}
FOREGROUND = {"mdpi": 108, "hdpi": 162, "xhdpi": 216, "xxhdpi": 324, "xxxhdpi": 432}

def circular(img):
    m = Image.new("L", (img.width * 4, img.height * 4), 0)
    ImageDraw.Draw(m).ellipse((0, 0, m.width - 1, m.height - 1), fill=255)
    m = m.resize(img.size, Image.LANCZOS)
    out = Image.new("RGBA", img.size, (0, 0, 0, 0))
    out.paste(img, (0, 0), m)
    return out

for d, px in FOREGROUND.items():
    canvas = Image.new("RGBA", (px, px), (0, 0, 0, 0))
    b = badge.resize((round(px * 0.62),) * 2, Image.LANCZOS)
    canvas.paste(b, ((px - b.width) // 2,) * 2, b)
    canvas.save(os.path.join(RES, f"mipmap-{d}", "ic_launcher_foreground.png"))
    print("foreground", d, px)

for d, px in LEGACY.items():
    sq = Image.new("RGBA", (px, px), (0, 0, 0, 255))
    b = badge.resize((round(px * 0.86),) * 2, Image.LANCZOS)
    sq.paste(b, ((px - b.width) // 2,) * 2, b)
    sq.save(os.path.join(RES, f"mipmap-{d}", "ic_launcher.png"))
    circular(sq).save(os.path.join(RES, f"mipmap-{d}", "ic_launcher_round.png"))
    print("legacy", d, px)
print("ICONS_DONE")
