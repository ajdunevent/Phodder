"""Flattens gen.py's layered grass into one shape per colour and writes the app icon.

Run gen.py first. Needs shapely.
"""
import re
import runpy
from pathlib import Path

from shapely.geometry import Polygon, box
from shapely.ops import unary_union

HERE = Path(__file__).parent
g = runpy.run_path(str(HERE / "gen.py"))
COLORS, SWAP, LIFT = g["COLORS"], g["SWAP"], g["LIFT"]
blades = g["blades"]  # back to front: (key, colour, path)

def quad(p0, c, p1, n=32):
    return [((1 - t) ** 2 * p0[0] + 2 * (1 - t) * t * c[0] + t * t * p1[0],
             (1 - t) ** 2 * p0[1] + 2 * (1 - t) * t * c[1] + t * t * p1[1])
            for t in (i / n for i in range(n + 1))]

def polygon(d, mirror=False):
    n = list(map(float, re.findall(r"-?\d+\.?\d*", d)))
    start, c1, tip, c2, end = (n[0], n[1]), (n[2], n[3]), (n[4], n[5]), (n[6], n[7]), (n[8], n[9])
    pts = quad(start, c1, tip) + quad(tip, c2, end)[1:]
    if mirror:
        pts = [(128 - x, y - LIFT) for x, y in pts]
    return Polygon(pts).buffer(0)

# The mirrored copy is painted first, behind the original.
layers = [(SWAP[c], polygon(d, mirror=True)) for _, c, d in blades]
layers += [(c, polygon(d)) for _, c, d in blades]

# Everything below the tray rim is hidden by the tray.
visible_area = box(0, 0, 128, g["BASE_Y"] - 4)
covered = None
pieces = {c: [] for c in COLORS}
for colour, poly in reversed(layers):
    part = poly.intersection(visible_area)
    if covered is not None:
        part = part.difference(covered)
    if not part.is_empty:
        pieces[colour].append(part)
    covered = poly if covered is None else covered.union(poly)

regions = {c: unary_union(p).simplify(0.02) for c, p in pieces.items()}
silhouette = unary_union([poly for _, poly in layers]).intersection(box(0, 0, 128, g["BASE_Y"])).simplify(0.02)

# The largest colour is painted as the whole silhouette underneath, so antialiased
# edges between regions never let the background show through as a seam.
under = max(COLORS, key=lambda c: regions[c].area)

def to_d(geom):
    polys = getattr(geom, "geoms", [geom])
    out = []
    for p in polys:
        for ring in [p.exterior, *p.interiors]:
            xs = list(ring.coords)[:-1]
            out.append("M" + " ".join(f"{x:.2f} {y:.2f}" for x, y in xs) + "Z")
    return "".join(out)

paths = [f'  <path fill="{under}" d="{to_d(silhouette)}"/>']
paths += [f'  <path fill="{c}" d="{to_d(regions[c])}"/>' for c in COLORS if c != under]

src = (HERE / "layered.svg").read_text()
start = src.index("  <!-- Mirrored copy")
end = src.index('  <path fill="#ffffff"')
open(HERE.parent / "hicolor/scalable/apps/net.dunevent.Phodder.svg", "w").write(src[:start] + "\n".join(paths) + "\n" + src[end:])
print({c: len(getattr(regions[c], "geoms", [regions[c]])) for c in COLORS}, "under:", under)
