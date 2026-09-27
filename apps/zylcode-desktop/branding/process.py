"""Generate brand assets from logo-source.jpg (run from repo root).

The source is a 1254x1254 JPEG whose "transparency" is a FAKE checkerboard.
Measured properties: neutral gray, ~10px cells, but the pattern PHASE DRIFTS
across the frame (it is not a global grid — no parity model holds), tones
~105..216 with vignette drift. So classification is signal-based, not
geometry-based:

  1. fg_seed = saturated pixels (blue/orange strokes) | very dark (outlines)
              | very bright (silver highlights)  — never floodable.
  2. background = adaptive flood-fill from the image border: a pixel joins if
     it is neutral, not fg_seed, and within 42 luminance of the neighbour it
     arrived from. JPEG cell ramps step ~20px per pixel so the flood crosses
     the drifting checker freely, but dies at logo outlines (jumps > 42).
  3. enclosed checker (letter counters, gaps between strokes) cannot be
     reached by the flood: neutral-mid components with luminance spread
     (p90-p10 >= 45) and area >= 400 are bimodal checker, not flat chrome.
  4. stray seed islands (checker overshoots) drop out via area filter (<120).

Everything is composited onto the #141414 chip (the app/menu/README color).

Outputs (all RGB on BG=(20,20,20)):
  branding/icon-source.png   1024x1024 monogram tile (input for `tauri icon`)
  branding/wordmark.png      trimmed ZYLCODE wordmark (header chip)
  branding/lockup.png        horizontal emblem+wordmark (About/Settings h-10)
  public/branding/emblem.png        256x256 monogram square
  public/branding/wordmark.png      header wordmark
  public/branding/lockup.png        horizontal lockup
  public/branding/logo-hero.png     1254x1254 vertical lockup (hero artwork)
  public/favicon.png                128x128 monogram
  docs/branding/zylcode-logo.png    720x720 vertical lockup (README)

After this script, regenerate the Tauri icon set from icon-source.png:
  cd apps/zylcode-desktop && ./node_modules/.bin/tauri icon branding/icon-source.png -o src-tauri/icons
  (then resize branding/icon-source.png to 64x64 -> icons/64x64.png, which
   `tauri icon` does not emit but the repo ships.)
"""
from PIL import Image, ImageFilter
import numpy as np
from scipy import ndimage

SRC = "apps/zylcode-desktop/branding/logo-source.jpg"
BG = (20, 20, 20)  # exact chip color (menus, About/Settings, README card)
MIN_FG_AREA = 120      # stray checker-overshoot islands drop below this
MIN_ENCLOSED = 400     # enclosed checker regions are at least this big
FLOOD_TOL = 42         # neighbour-adaptive luminance tolerance

im = Image.open(SRC).convert("RGB")
a = np.asarray(im).astype(float)
h, w = a.shape[:2]
lum = a.mean(axis=2)
mx, mn = a.max(axis=2), a.min(axis=2)
sat = np.where(mx > 0, (mx - mn) / np.maximum(mx, 1e-6), 0.0)

# --- seed mask: never floodable ------------------------------------------------
fg_seed = (sat > 0.07) | (lum < 92) | (lum > 218)

# --- flood fill from the border (BFS, neighbour-adaptive tolerance) ------------
neutral = (sat <= 0.07)
free = neutral & (~fg_seed)
bg = np.zeros((h, w), bool)

# seed queue: all border pixels that are free
queue = []
for x in range(w):
    for y in (0, h - 1):
        if free[y, x] and not bg[y, x]:
            bg[y, x] = True; queue.append((y, x))
for y in range(h):
    for x in (0, w - 1):
        if free[y, x] and not bg[y, x]:
            bg[y, x] = True; queue.append((y, x))

# coarse-chunk BFS: process queue in blocks for speed
arr_lum = lum
qi = 0
while qi < len(queue):
    y, x = queue[qi]; qi += 1
    base = arr_lum[y, x]
    for dy, dx in ((1, 0), (-1, 0), (0, 1), (0, -1)):
        ny, nx = y + dy, x + dx
        if 0 <= ny < h and 0 <= nx < w and free[ny, nx] and not bg[ny, nx]:
            if abs(arr_lum[ny, nx] - base) <= FLOOD_TOL:
                bg[ny, nx] = True
                queue.append((ny, nx))
print(f"flood: {bg.mean()*100:.1f}% of frame reached from border")

# --- enclosed checker (letter counters, stroke gaps) ---------------------------
enc_candidates = (~bg) & (~fg_seed)
lab, n = ndimage.label(enc_candidates, structure=np.ones((3, 3)))
enc = np.zeros((h, w), bool)
objs = ndimage.find_objects(lab)
stats = ndimage.sum(np.ones_like(lab), lab, index=np.arange(1, n + 1))
keyed_enc = 0
for i, sl in enumerate(objs, start=1):
    if sl is None:
        continue
    area = int(stats[i - 1])
    if area < MIN_ENCLOSED:
        continue
    comp = lum[sl][lab[sl] == i]
    p10, p90 = np.percentile(comp, [10, 90])
    if (p90 - p10) >= 45:          # bimodal light/dark oscillation = checker
        enc[lab == i] = True
        keyed_enc += 1
print(f"enclosed checker components keyed: {keyed_enc}")

bg_mask = bg | enc

# --- foreground cleanup --------------------------------------------------------
fg_mask = ~bg_mask
lab_f, n_f = ndimage.label(fg_mask, structure=np.ones((3, 3)))
if n_f:
    sizes = ndimage.sum(np.ones_like(lab_f), lab_f, index=np.arange(1, n_f + 1))
    keep = np.zeros(n_f + 1, bool)
    keep[1:] = sizes >= MIN_FG_AREA
    fg_mask = keep[lab_f]
# solidify strokes (JPEG speckle holes), then feather the mask
fg_mask = ndimage.binary_closing(fg_mask, structure=np.ones((5, 5)))

alpha = Image.fromarray((fg_mask * 255).astype(np.uint8)).filter(ImageFilter.GaussianBlur(1.0))
alpha_np = np.asarray(alpha).astype(float) / 255.0
chip = np.zeros_like(a)
chip[:, :] = BG
comp = (a * alpha_np[:, :, None] + chip * (1 - alpha_np[:, :, None])).astype(np.uint8)
out = Image.fromarray(comp, "RGB")

ys, xs = np.where(fg_mask)
y0, y1, x0, x1 = ys.min(), ys.max() + 1, xs.min(), xs.max() + 1
ring_fg = fg_mask.copy(); ring_fg[40:-40, 40:-40] = False
print(f"content bbox: x {x0}..{x1} y {y0}..{y1}; fg-in-border-ring: {ring_fg.mean()*100:.3f}% (should be ~0)")

dens = fg_mask.mean(axis=1)
# The monogram/wordmark valley still carries glow pixels (never reaches zero),
# so split at the density minimum in the 70-85% depth band instead of
# hunting for fully-empty row runs.
lo = y0 + int((y1 - y0) * 0.70)
hi = y0 + int((y1 - y0) * 0.85)
split = lo + int(np.argmin(dens[lo:hi]))
gap = (split, split) if dens[split] < 0.12 else None
print(f"split row: {split} (dens={dens[split]:.3f}); gap rows: {gap}")

mono_box = (x0, y0, x1, gap[0]) if gap else (x0, y0, x1, y1)
word_box = (x0, gap[1], x1, y1) if gap else None


def trim(img):
    arr = np.asarray(img).astype(int)
    d = np.abs(arr - np.array(BG)).sum(axis=2) > 18
    yy, xx = np.where(d)
    return img.crop((xx.min(), yy.min(), xx.max() + 1, yy.max() + 1))


def fit(img, canvas, frac):
    """Center `img` (scaled to frac of canvas, contained) on a BG canvas."""
    cw, ch = canvas
    side_w, side_h = int(cw * frac), int(ch * frac)
    scale = min(side_w / img.width, side_h / img.height)
    r = img.resize((max(1, round(img.width * scale)), max(1, round(img.height * scale))), Image.LANCZOS)
    canv = Image.new("RGB", canvas, BG)
    canv.paste(r, ((cw - r.width) // 2, (ch - r.height) // 2))
    return canv


mono = trim(out.crop(mono_box))
word = trim(out.crop(word_box)) if word_box else None
full = trim(out)
if word is None:
    word = mono  # no split found — full lockup stands in for the wordmark


def hlockup(cw=1155, ch=434):
    """Horizontal lockup: emblem left, wordmark right (About/Settings use h-10)."""
    mh = int(ch * 0.80)
    m = mono.resize((round(mono.width * mh / mono.height), mh), Image.LANCZOS)
    wh = int(ch * 0.34)
    wd_ = word.resize((round(word.width * wh / word.height), wh), Image.LANCZOS)
    gapx = int(cw * 0.05)
    gw, gh = m.width + gapx + wd_.width, max(m.height, wd_.height)
    scale = min(cw * 0.94 / gw, ch * 0.92 / gh)
    m = m.resize((max(1, round(m.width * scale)), max(1, round(m.height * scale))), Image.LANCZOS)
    wd_ = wd_.resize((max(1, round(wd_.width * scale)), max(1, round(wd_.height * scale))), Image.LANCZOS)
    gap2 = max(8, round(gapx * scale))
    canv = Image.new("RGB", (cw, ch), BG)
    total = m.width + gap2 + wd_.width
    x = (cw - total) // 2
    canv.paste(m, (x, (ch - m.height) // 2))
    canv.paste(wd_, (x + m.width + gap2, (ch - wd_.height) // 2))
    return canv


def save(img, path):
    img.save(path)
    arr = np.asarray(img).astype(int)
    corners = np.concatenate([arr[:6, :6].reshape(-1, 3), arr[:6, -6:].reshape(-1, 3),
                              arr[-6:, :6].reshape(-1, 3), arr[-6:, -6:].reshape(-1, 3)])
    print(f"  saved {path} {img.size} corner_std={corners.std():.1f}")


B = "apps/zylcode-desktop/branding/"
P = "apps/zylcode-desktop/public/"
save(fit(mono, (1024, 1024), 0.86), B + "icon-source.png")
save(word, B + "wordmark.png")
save(hlockup(), B + "lockup.png")
save(fit(mono, (256, 256), 0.90), P + "branding/emblem.png")
save(word, P + "branding/wordmark.png")
save(hlockup(), P + "branding/lockup.png")
save(fit(full, (1254, 1254), 0.92), P + "branding/logo-hero.png")
save(fit(mono, (128, 128), 0.90), P + "favicon.png")
save(fit(full, (720, 720), 0.90), "docs/branding/zylcode-logo.png")
print(f"checker removed: {(~fg_mask).mean()*100:.1f}% of frame keyed to chip")
print("done")
