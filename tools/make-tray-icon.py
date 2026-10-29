# 托盘图标生成：三态各一张 32x32 PNG
# 用法: python tools/make-tray-icon.py
from PIL import Image, ImageDraw

SIZE = 32


def dot(color, path):
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.ellipse((4, 4, SIZE - 4, SIZE - 4), fill=color + (255,))
    d.ellipse((11, 11, SIZE - 11, SIZE - 11), fill=(255, 255, 255, 235))
    img.save(path)


dot((154, 164, 178), "apps/shell/assets/tray-idle.png")
dot((11, 107, 203), "apps/shell/assets/tray-listen.png")
dot((26, 26, 26), "apps/shell/assets/tray-speak.png")
print("icons written")
