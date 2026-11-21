import random
from pathlib import Path

rnd = random.Random(7)
LIFT = 4
COLORS = ["#26a269", "#33d17a", "#8ff0a4"]
BASE_Y = 104
# Blade heights were drawn for a rim at y=100; the tray and grass were then
# stretched to fill the canvas as tall as GNOME's other square-ish icons.
HEIGHT_SCALE = 1.1

def blade(bx, width, tx, ty, bend):
    """Path for a tapered blade rooted on the tray, bowed sideways by `bend`."""
    l, r = bx - width / 2, bx + width / 2
    ty = BASE_Y - (100 - ty) * HEIGHT_SCALE
    my = (BASE_Y + ty) / 2
    return (f"M{l:.1f} {BASE_Y} Q{(l + tx) / 2 + bend:.1f} {my:.1f} {tx:.1f} {ty:.1f} "
            f"Q{(r + tx) / 2 + bend:.1f} {my:.1f} {r:.1f} {BASE_Y} Z")



FILLERS = [73, 33, 95, 44, 87, 25]
blades = []
count = 64
for i in range(count):
    bx = 24 + i * (80 / (count - 1)) + rnd.uniform(-1.5, 1.5)
    spread = (bx - 64) / 40
    width = rnd.uniform(7, 10)
    ty = rnd.uniform(10, 32) + abs(spread) * 8
    tx = bx + spread * 7 + rnd.uniform(-5, 5)
    bend = rnd.uniform(-4, 4)
    depth = rnd.random()
    color = COLORS[min(2, int(depth * 3))]
    blades.append((rnd.random(), color, blade(bx, width, tx, ty, bend)))

# Random stacking keeps all three greens visible across the mass.
# Mid-green blades at the back fill gaps that would drop below the play arrow.
for fx in FILLERS:
    blades.append((0.0, COLORS[1], blade(fx, 10, fx, 24, 0)))
blades.sort(key=lambda b: b[0])
grass = "\n  ".join(f'<path fill="{c}" d="{d}"/>' for _, c, d in blades)
SWAP = {COLORS[0]: COLORS[1], COLORS[1]: COLORS[0], COLORS[2]: COLORS[2]}
mirrored = "\n    ".join(f'<path fill="{SWAP[c]}" d="{d}"/>' for _, c, d in blades)

svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
  <!-- Mirrored copy behind, dark and mid greens swapped, raised slightly. -->
  <g transform="translate(0 -{LIFT}) translate(128 0) scale(-1 1)">
    {mirrored}
  </g>
  {grass}
  <path fill="#ffffff" stroke="#ffffff" stroke-width="12" stroke-linejoin="round" d="M54 48 L54 82 L83.44 65 Z"/>
  <path fill="#3d3846" stroke="#3d3846" stroke-width="4" stroke-linejoin="round" d="M56 52 L56 78 L78.52 65 Z"/>
  <path fill="#3d3846" d="M14 100 H114 L107 118 Q106 120 103 120 H25 Q22 120 21 118 Z"/>
  <path fill="#5e5c64" d="M14 100 H114 L108.5 114 Q107.5 116 105 116 H23 Q20.5 116 19.5 114 Z"/>
  <rect x="14" y="99" width="100" height="3" rx="1.5" fill="#9a9996"/>
</svg>
'''
open(Path(__file__).with_name("layered.svg"), "w").write(svg)
