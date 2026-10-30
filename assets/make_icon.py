"""Clipper icon: GuideBook's flat circle (r=120 on 256) in Clipper red.
Writes clipper.svg and a multi-size clipper.ico next to this script."""
from pathlib import Path
from PIL import Image, ImageDraw

RED = "#C9361B"
here = Path(__file__).parent
(here / "clipper.svg").write_text(
    f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256"><circle cx="128" cy="128" r="120" fill="{RED}"/></svg>'
)
big = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
ImageDraw.Draw(big).ellipse((32, 32, 992, 992), fill=RED)  # r=120/256, supersampled
sizes = [256, 128, 64, 48, 32, 24, 16]
big.resize((256, 256), Image.LANCZOS).save(here / "clipper.ico", sizes=[(s, s) for s in sizes])
print("wrote clipper.svg, clipper.ico")
