# Resource Monitor — charting

The graphs are the product. Right now they are one function:

```rust
/// Line sparkline of `ring` inside `r`, values scaled to `max` (min 1.0).
pub fn sparkline(dc: HDC, r: &RECT, ring: &Ring, max: f32, color: u32)
```

— `src/ui/gdi.rs:389-417`. A 1 px aliased `Polyline`, no fill, no baseline, no
scale, no labels, no readout. This document replaces it.

Read [`ui-foundation.md`](ui-foundation.md) first: §4 defines the accents and
the `mix` helper, §7 defines the DIB-backed back buffer and `aa_polyline` /
`aa_polygon`, which everything here assumes.

---

## 1. What is actually broken today

**The y-axis is not stable.** `draw_main` passes `self.hist_disk.max()` and
`self.hist_net.max()` as the ceiling (`panel.rs:2407`, `:2414`); `draw_process`
passes `self.watch_rings[n].max()` (`panel.rs:3595`, `:3598`, `:3599`). The
ceiling therefore changes on almost every tick, and the trace re-scales under
itself. Consequences:

- A disk doing a steady 4 MB/s and a disk doing a steady 400 MB/s draw the
  **same picture**.
- A single spike rescales the previous 59 samples down to a flat line, so the
  spike erases its own context.
- The eye reads the resulting vertical drift as motion, which is worse than
  useless on an instrument.

**There is no zero.** With no baseline drawn, "idle" and "no data" and "the
window rescaled" are visually identical.

**There is no window.** The ring is 60 samples (`panel.rs:505-510`) and the
interval is user-selectable — 0.5 s, 1 s or 2 s (`panel.rs:3904-3908`). So the
graph silently covers 30 s, 60 s or 120 s and never says which.

**There is no readout.** The panel already tracks `hover_pos` and repaints on
every move (`panel.rs:668-690`). The machinery for "what was the value 14
seconds ago" is sitting there, unused.

**It is 1 px and aliased.** At 32 logical px tall with 60 samples across 110 px,
almost every segment is a diagonal, and every diagonal is a staircase.

---

## 2. The three chart sizes

| name | size (logical) | where | anatomy |
|---|---|---|---|
| **Row** | 120 × 32 | metric rows in `draw_main` (`panel.rs:2456`) and `draw_process` (`panel.rs:3605`) | trace, wash, baseline, head dot |
| **Hero** | full width × 96 | top of every drill-down and the watch view | + gridlines, ceiling label, window label, hover crosshair, peak marker, range chips |
| **Micro** | 56 × 14 | per-core cells, widget segments (future) | trace + baseline only |

All three share one routine and one options struct:

```rust
pub struct Chart<'a> {
    pub rect: RECT,
    pub series: &'a [Series<'a>],   // 1 or 2
    pub ceiling: f32,
    pub accent: u32,
    pub unit: Unit,                 // Pct | Rate | Bytes | Fps
    pub grid: bool,
    pub labels: bool,
    pub hover_x: Option<i32>,
    pub mirrored: bool,             // series[1] drawn below the midline
}
pub struct Series<'a> { pub ring: &'a Ring, pub label: &'a str }
```

`Row` is `Chart { grid: false, labels: false, .. }`. There is one drawing path,
so a fix to the hero chart is a fix to the sparkline.

---

## 3. The scale — the single most important fix

### Ceiling policy

```rust
/// Nearest 1 / 2 / 5 × 10ⁿ at or above v.
fn nice(v: f32) -> f32 {
    if v <= 0.0 { return 1.0 }
    let p = 10f32.powf(v.log10().floor());
    let m = v / p;
    p * if m <= 1.0 { 1.0 } else if m <= 2.0 { 2.0 } else if m <= 5.0 { 5.0 } else { 10.0 }
}
```

Per metric, per tick:

| metric | ceiling |
|---|---|
| CPU, GPU, RAM %, Sound | **pinned to 100.** Never rescales, ever. |
| FPS | `nice(max(60, window_max))` — so 60 / 120 / 144 Hz displays hold a stable frame |
| Disk, Network | sticky nice ceiling (below) |

Sticky, for the rate metrics — one extra `f32` per ring:

```rust
self.ceil_raw = window_max.max(self.ceil_raw * 0.93);   // rises instantly, decays ~7%/tick
let ceiling  = nice(self.ceil_raw);
```

The trace holds still while traffic varies, and the ceiling steps by a clean
factor of 2 or 2.5 when the order of magnitude genuinely changes — roughly once
every thirty ticks at worst, not every tick. Because `nice` quantises, a decay
of 7 % per tick produces a ceiling change only when it crosses a step boundary.

### Ceiling label

Top-right of the plot, `micro` in `dim`, formatted by `Unit`:
`100%` · `50 MB/s` · `144 fps` · `16 GB`. On a `Row` chart the ceiling label is
omitted — the row's value text carries the number, and the fixed 100 % ceiling
means the percent rows need no label at all.

### Window label

Bottom-left of the hero plot, `micro` in `mute`: `60s` — derived, not hardcoded:

```rust
let secs = ring.capacity() as u32 * self.cfg.interval_ms / 1000;
```

This is the first time the panel will have said what its own graphs cover.

---

## 4. Line, wash and baseline

### Geometry

Sample *i* of *n*, right-aligned so the newest sits at the right edge — keep the
existing mapping from `gdi.rs:399-406`, but in `f32`:

```
x(i) = plot.left + (start_slot + i) * (plot.w - 1) / (cap - 1)
y(v) = plot.bottom - 1 - (plot.h - 2) * clamp(v / ceiling, 0, 1)
```

### Draw order

1. **Plate.** `card` fill, 4 px radius, 1 px `line` border. On a `Row` chart the
   metric card *is* the plate — no second surface.
2. **Un-sampled shading.** If fewer than `cap` samples are held, fill
   `plot.left … x(0)` with `mix(card, bg, 0.5)`. The window visibly fills.
3. **Gridlines** (hero only, §5).
4. **Wash.** Build `HRGN` = `CreatePolygonRgn(trace ++ [(x_last, base), (x_first,
   base)], WINDING)`, `SelectClipRgn`, then one **`GdiGradientFill`**
   (`GRADIENT_FILL_RECT_V`, gdi32 — no msimg32 import needed) over the whole plot
   rect from `mix(accent, card, 0.26)` at the top to `mix(accent, card, 0.03)` at
   the baseline, then `SelectClipRgn(dc, null)` and `DeleteObject(rgn)`.

   Two calls, one region. And because the gradient runs in *screen* y rather
   than per-column, a busy chart is luminous and a quiet one is nearly bare —
   which is the correct reading, and free.

   Worked example, dark theme, CPU `#4A9CF6` on `card #1E2024`:
   top `#2A435F`, bottom `#20252C`. Light theme, `#1873DC` on white:
   top `#D1E3F8` (at 0.20 — light surfaces need a lighter wash), bottom
   `#F1F7FD`.
5. **Baseline.** 1 px `line`, full plot width, **always drawn**, even at zero.
   This is what makes "idle" legible.
6. **Trace.** `aa_polyline` at **1.5 logical px**, round joins. Antialiased, or
   the whole exercise is pointless (§7 of `ui-foundation.md`).
7. **Head dot.** Filled circle r = 2.5 logical px at the newest sample, with a
   2 px ring in `card` so it stays legible where it sits on the trace. One pixel
   larger on the frame a new sample arrives (`ui-foundation.md` §8).

### Zero and near-zero

A rate of 0 draws at `y = plot.bottom - 1`, i.e. **on** the baseline, not below
the plot. The trace and the baseline coincide and the wash has zero height. That
is the correct picture of "nothing is happening" and it is unambiguous, because
the baseline is always there for comparison.

---

## 5. Gridlines, axis and labelling

Gate on **device** height, so a user at "larger" text or 200 % DPI gets more
detail, not a crowded box:

| plot height (device px) | gridlines |
|---|---|
| < 40 | none — baseline only |
| 40 – 79 | 50 % |
| ≥ 80 | 25 %, 50 %, 75 % |

Gridlines are **hairline, solid, one step off the surface** — `grid`
(`#303236` dark / `#EAEAEB` light), never dashed. `PS_DOT` in GDI is 1 px-only
and renders as noise; there is no reason to reach for it.

**No vertical gridlines.** The x-axis is time and time is continuous; a 336 px
panel has no room to label vertical rules, and unlabelled ones are decoration.

**No y-axis tick labels.** One ceiling label top-right is the whole y-axis. A
left-hand tick column would cost 32 logical px of a 312 px plot for information
that is redundant with a fixed 100 % ceiling and with the hover readout.

**One peak marker per hero chart.** A 3 px tick in `dim` at the window maximum's
`(x, y)`, with `micro dim` text `peak 82%` placed to whichever side has room.
Suppressed when the peak is the newest sample (the head dot already says so) or
when the window is flat (§6). This is the only direct label on the chart, and it
works *because* it is the only one.

---

## 6. Hover readout

The hero chart's header line, above the plot:

```
CPU                                    41.7%   now
──────────────────────────────────────────────────
                                     ╭──╮
              ╭─╮        ╭───╮      ╭╯  ╰──╮
   ╭──────────╯ ╰────────╯   ╰──────╯      ╰────●
   ──────────────────────────────────────────────
   60s                                       100%
```

On hover it becomes the sample under the cursor:

```
CPU                                    78.2%   23s ago
```

Mechanics — all of it rides existing machinery:

- `hover_pos` is already tracked and already forces a repaint on change
  (`panel.rs:681-689`). Nothing new is needed to drive it.
- Snap the cursor x to the nearest sample slot: `i = round((hx - plot.left) *
  (cap - 1) / (plot.w - 1))`, clamped, then skip if that slot holds no sample.
- Draw a 1 px vertical rule in `grid` at `x(i)`, from plot top to baseline,
  **behind the trace** (before step 6 above).
- Draw a ring dot at `(x(i), y(v))` — r = 3, 2 px ring in `card`, fill `accent`.
- Right-align the value in `value` weight and the relative age in `micro dim`,
  in the header line. Relative, not absolute: `23s ago` answers the question a
  monitor is asked; `14:22:07` does not.

`Row` charts get no crosshair — but hovering a metric row already highlights it,
and clicking opens the hero. That is the right progressive disclosure.

---

## 7. Spiky versus flat

Three mechanisms, in order of how much they do:

1. **The fixed / sticky ceiling (§3)** does most of the work. A flat 4 % CPU
   trace now draws near the floor of a 0–100 plot instead of filling the box.
2. **The always-present baseline (§4)** gives the eye a fixed reference, so a
   low flat trace reads as low rather than as "the middle of something".
3. **The flat case is named.** When
   `window_max - window_min < ceiling * 0.02` and the chart is a hero, suppress
   the peak marker and the gridline labels and print nothing extra. The picture
   — a straight line just above a baseline, under a 100 % ceiling — is already
   complete. Do not add a "flat" badge; the chart is not broken, the machine is
   idle.

For a genuinely spiky series the wash is what carries the shape: a single 90 %
spike over a 3 % floor draws a narrow luminous spire out of a dark strip. That
reads instantly, and it is the exact case the current rescaling destroys.

---

## 8. Multi-series

**Never two different metrics on one plot.** No dual y-axis, ever — two scales
in one frame is the single most common charting mistake and this app has an
obvious temptation to do it (CPU and GPU on one graph). Two measures means two
charts.

### Network — download versus upload

One quantity, two directions, so: **mirrored about the midline**, and **each
half on its own sticky ceiling**.

> **Revised after implementation.** This section originally specified one shared
> ceiling, `nice(max(rx_max, tx_max))`, so that the two halves stayed comparable
> by eye. Measured against real traffic that is the wrong trade: disk read runs
> about seven times its write and download about twelve times its upload, and a
> row chart gives each half fifteen pixels. The secondary drew one or two pixels
> off the midline and was reported, correctly, as looking flat at zero. Each half
> now scales to its own ceiling and **the hero plot labels both**, which puts the
> asymmetry on the screen rather than hiding it inside a scale nobody can read.
> Comparability was a real property and it has been given up deliberately; the
> two ceiling labels are what replaces it.

```
        Down                                 12.4 MB/s
        ╭───╮      ╭──╮
   ─────╯   ╰──────╯  ╰───────────────●            ← down, accent
   ═══════════════════════════════════════          ← midline, `line`
   ────╮  ╭─────────╮   ╭───────────────●          ← up, mix(accent, dim, 0.45)
        Up                                 0.9 MB/s
```

- Down uses `net` at full strength; up uses **its own hue**, `net_tx`
  (blue-green), and disk write uses `disk_w` (gold).

  > **Revised after implementation.** This originally read "up uses
  > `mix(net, dim, 0.45)` — a desaturated sibling of the same hue, because
  > direction is not identity". The reasoning still holds in the abstract and it
  > lost to practice: a desaturated trace at a twelfth of the primary's height
  > reads as a rendering artefact, not as a second series. Gold against cyan and
  > blue-green against orange are complementary pairs, separable at a glance, and
  > both sit in gaps the seven metric accents leave — so neither can be misread
  > as another metric inside a row that already names its own. All four clear
  > 4.5:1 on `card` in both themes.
- Two permanent direct labels, `Down` and `Up`, in `micro mute` at the left of
  each half. **Never colour alone.**
- Each half gets its own wash, gradient running away from the midline.
- Replaces the current `"↓ {} · ↑ {}"` value string (`panel.rs:2412`), which
  puts two numbers where a shape belongs.

### Disk — read versus write

Identical treatment, labels `Read` / `Write`, accent `disk`. Replaces
`"R {} · W {}"` (`panel.rs:2405`).

### CPU cores

The existing bar grid (`panel.rs:3240-3257`) is the right form for 336 px and up
to 64 cores; a small-multiples grid of sparklines is unreadable below ~90 px per
cell and 32 cells would be 190 px of vertical space. **Keep the grid, restyle
it**, and add the one thing it lacks — history:

- Bars 8 logical px tall (was `s(8)` ✓), 4 px radius, **2 px surface gap**
  between neighbours (currently `s(6)`, which reads as a gap between *groups*).
- Track in `track`, fill in `cpu`.
- **A 1 px peak tick** in `mix(cpu, text, 0.4)` at each core's maximum over the
  last 60 samples. One `u8` ring of 60 per core: 32 cores = **1.9 KB**. That
  single pixel per core turns a snapshot into a window, and it is the whole
  reason the grid is worth keeping.
- Hovering a bar shows `core 12 · 74% · peak 98%` in the section header.
- Columns: 2 up to 8 cores, 4 up to 32, 8 above — unchanged from
  `panel.rs:3244`.

---

## 9. Empty, collecting and unavailable

Four distinct states, four distinct pictures. Today three of them look the same.

| state | picture |
|---|---|
| **Collecting** (`ring.len() < 3`) | plate + baseline + `collecting…` in `micro mute`, centred. The un-sampled shading (§4 step 2) covers the whole plot. |
| **Partially filled** (`3 ≤ len < cap`) | normal chart; the un-sampled region on the left stays shaded, so the window is visibly filling. |
| **Idle** (`window_max == 0`) | normal chart: trace flat on the baseline, no wash, head dot present. The head dot is what distinguishes idle from dead. |
| **Unavailable** (`!gpu_ok`, `!etw_ok`) | **no plate at all.** An empty framed box implies data that failed to arrive. Draw the row's prose instead — the panel already does this well at `panel.rs:3259-3268` and `panel.rs:2367`. |

The `—` value already used for unavailable GPU and FPS (`panel.rs:2360`,
`:2367`) stays; the chart beside it simply is not drawn.

---

## 10. The longer window

The ring holds 60 samples. At 1 s that is one minute, which is right for "what
is happening now" and useless for "what happened while I was building".

**Add one decimated ring per metric.** For each metric, accumulate
`(min, max, sum, n)` over each wall-clock minute; on rollover push a
`(min, max, mean)` triple into a second 60-slot ring. That is a **60-minute
window for 720 bytes per metric** — 5 KB across all seven — versus the 84 KB a
naive 3 600-sample ring would need.

The hero chart gains two `micro` chips at its top-right:

```
                                              [ 1m ] [ 1h ]
```

reusing `Ui::chip` (`panel.rs:2245`) at `micro` size. `1m` draws the raw ring
exactly as specified above. `1h` draws a **min–max band with a mean line**:

- `aa_polygon` over `[max points…] ++ [min points reversed]` filled with
  `mix(accent, card, 0.22)` — the envelope of everything that happened.
- `aa_polyline` at 1.5 px in `accent` over the means.
- Same baseline, same gridlines, same ceiling policy (computed over the minute
  maxima, so a 1 h view is not scaled by a 1 s spike it cannot show).
- Window label reads `60m`; the hover readout reads
  `14:03 · avg 34% · peak 91%`.

Two polylines and one polygon. This is the treatment that makes the panel worth
opening after the fact, and it is the one feature here that iStat Menus has and
this app does not.

Persistence is explicitly **out of scope** — the 1 h window lives in memory and
resets with the process, like everything else in `Ui`.

---

## 10b. Per-core, and the question it cannot answer

Each core cell carries a 60-sample ring and a 1 px peak tick, and hovering one
names it: `core 5 · 42% now · 91% peak in 60s`. The grid stays otherwise
unlabelled — at four to eight columns there is no room for sixteen numbers, and
unlabelled rules would be decoration.

**Per-core usage *per process* is not offered, and that is a decision.** Windows
exposes process *affinity* cheaply and exactly — which cores a process is
allowed on, shown in the process view — but not which core it is currently
running on. The only accurate source for that is ETW context-switch tracing,
which needs a second kernel session, a thread-to-process map maintained from
thread lifetime events, and thousands of events a second on a busy machine. For
a tool whose whole claim is that it is cheap to run, that is the wrong trade. If
it is ever built it should be an opt-in mode with an explicit cost, not a
default.

## 11. Colour rules for charts

Translated from the `dataviz` method to this renderer:

- **Marks wear the accent. Text does not.** Values, ages, ceilings, window
  labels and axis text use `text` / `dim` / `mute`. The one exception is the
  metric's own name — it is the identity key and it sits beside its own graph.
  This deletes the accent-coloured value column at `panel.rs:3736`.
- **The wash is a wash**: 26 % → 3 % of the accent mixed over the card in dark,
  20 % → 6 % in light. Never a saturated block.
- **Gridlines are one step off surface, hairline, solid.** Never dashed, never
  coloured.
- **Direction gets its own hue, and keeps its label.** Down/up and read/write
  are two hues — `net`/`net_tx` and `disk`/`disk_w` — *plus* the permanent direct
  labels. The labels are not redundant: they are what stops the second hue being
  read as a different metric, and they are what survives colourblindness. This
  reverses the original rule, which asked for two steps of one hue; see §8 for
  what real traffic did to it.
- **Status never becomes a series colour.** `danger` on a chart means a
  threshold was crossed, nothing else. Reserved.
- **A single series needs no legend** — the row's own label is the legend. Only
  the mirrored charts carry direct labels, and they carry them always.

---

## 12. Cost

Per full panel paint, dark theme, 336 × 620 logical at 100 % DPI.

| element | count | GDI calls | pixel writes |
|---|---:|---:|---:|
| Row chart: plate + wash + baseline + trace + dot | 7 | ~9 each | ~1 400 (fill) + ~330 (blend) each |
| Hero chart: + 3 gridlines, 2 labels, peak marker | 1 | ~20 | ~30 000 (fill) + ~1 000 (blend) |
| Core grid, 16 cores | 1 | 32 | ~2 000 |
| Hover crosshair + readout | 1 | 5 | ~200 |

Coverage-blended (antialiased) pixels are the only expensive ones and they are
proportional to *ink*, not to area: about **3 500 per paint** on the busiest
screen. The panel already issues on the order of 200 `FillRect`/`TextOut` calls
per paint; this adds roughly 60 calls and a low-tens-of-thousands of pixel
writes. At 0.5 Hz plus hover, on any machine that can run Windows 11, this is
not measurable.

The one thing to watch is **hover repaints on the core grid at 64 cores** —
every mouse move repaints 64 bars and 64 peak ticks. If that ever shows up,
the fix is to invalidate only the chart's rect on a hover-position change
rather than the whole client area (`panel.rs:688` currently passes `null`), not
to make the chart cheaper.

---

## 13. What this replaces

| today | becomes |
|---|---|
| `gdi::sparkline` (`gdi.rs:389`) | `gdi::chart` with `Chart { grid: false, labels: false }` |
| `ring.max()` as ceiling (`panel.rs:2400`, `:2407`, `:2414`, `:3595-3599`) | the ceiling policy in §3 |
| `"R {} · W {}"` (`panel.rs:2405`) | mirrored disk chart |
| `"↓ {} · ↑ {}"` (`panel.rs:2412`) | mirrored network chart |
| per-core `gdi::bar` grid (`panel.rs:3253`) | same grid, rounded, gapped, with peak ticks |
| `gdi::bar` for drives (`gdi.rs:428`) | same signature, 2 px radius, `warn` above 85 % and `danger` above 95 % |
| nothing | hover readout, window label, peak marker, 1 h range |
