# Resource Monitor — UI foundation

A design specification for the next visual generation of the panel, widget,
overlay and tray. **This is an evolution of what is already there**, not a
replacement: the drawing vocabulary, the layout rhythm and the interaction
model all survive. What changes is the tuning — type, spacing, colour, edges —
and one genuinely new capability: a rendering primitive that can draw an
antialiased curve.

Everything here is expressible in Win32 GDI. Where a treatment needs a call the
codebase does not use yet, the call is named. Where a treatment is expensive or
risky, the cost is stated in the same sentence.

Charting is specified separately in [`charts.md`](charts.md).

---

## 1. Honest critique of the current design language

### What is already good, and must be preserved

**Vector glyphs drawn from an exact centre.** `gdi::chevron` (`src/ui/gdi.rs:300`)
and `gdi::disclosure` (`src/ui/gdi.rs:344`) reject font glyphs with a written
reason:

> *"Font glyphs like `‹` centre their cell, not their ink, so a glyph never
> lines up with adjacent text; this always does."* — `gdi.rs:297`

That instinct is exactly right and is the seed of the whole icon system in §5.

**Baseline-relative typography.** `header_ex` (`panel.rs:3090-3094`) computes a
single baseline from `text_metrics` and hangs the chevron, the title and the
right-hand action off it, using `tmInternalLeading` to find the optical centre
of the capitals. Very few Win32 apps do this. Keep it; make it the rule.

**Graceful text degradation.** `text_fit` (`gdi.rs:372`) walking a `fit_stack`
(`gdi.rs:127`) before falling back to ellipsis is how a 336 px panel survives a
user who picks "larger". Keep the mechanism; §2 only changes what is in the
stack.

**One control height.** `util::CTRL_H = 24` (`src/util.rs:6`) so a chip can
never sit shorter than the input beside it. Keep; §3 puts it on a grid.

**Derived colour from a single accent.** `gdi::shade` (`gdi.rs:89`) lets
`footer_button` (`panel.rs:2600-2611`) build a border and an interior from one
constant. Keep, and generalise it to `mix` (§4).

**Total DPI discipline.** Every geometric constant goes through `self.s(n)`.
There is not one raw pixel in the panel. Preserve absolutely.

**Double buffering and clipping.** `BackBuffer` (`gdi.rs:141`), the
`SaveDC`/`IntersectClipRect`/`RestoreDC` pattern around every scrolling list
(`panel.rs:3289-3290`), and the `WS_CLIPCHILDREN` note at `panel.rs:42-45`.
The rendering plumbing is sound; only the paint is dated.

### What reads as dated or unconsidered

**1. The sparkline is the weakest thing in the product.**
`gdi::sparkline` (`gdi.rs:389-417`) is a 1 px aliased `Polyline` in a 110×32
logical box. It has no fill, no baseline, no scale reference and no readout.
Worse, callers pass `ring.max()` as the ceiling (`panel.rs:2400`, `:2407`,
`:2414`), so the y-axis silently rescales on every tick: **a flat trace and a
spiky trace render identically**. See `charts.md`.

**2. Words and font glyphs are doing the work of icons.**
`"settings"`, `"pin"`, `"top"` as lowercase words (`panel.rs:2298-2306`);
`"×"` for close (`panel.rs:2223`) *and* for destructive kill (`panel.rs:3313`,
`:3737`, `:4389`); `"›"` in `nav_row` (`panel.rs:3151`) beside the vector
chevron used for the same meaning in `draw_settings` (`panel.rs:3839`);
`"▾"`/`"▸"` as strings (`panel.rs:2753`, `:2825`) beside the vector
`disclosure` used for the same meaning at `panel.rs:2990`; `"◆"`/`"◇"` in the
footer (`panel.rs:2554`, `:2568`) and the widget (`widget.rs:322-324`);
`"pick ▾"` (`panel.rs:4280`); `"↓"`/`"↑"` inside value strings
(`panel.rs:2412`). The file that knows glyphs do not align is surrounded by
glyphs. This is the single most visible gap against iStat Menus and Stats.

**3. Accents are theme-independent constants.**
`ACC_CPU … ACC_AUDIO` (`gdi.rs:78-84`) are `pub const`, while `THEMES`
(`gdi.rs:25`) carries only surfaces. Measured against the light theme's white
card they run **1.86:1 – 2.78:1**. Every coloured label in the light theme
fails WCAG by a wide margin. Two of them are also indistinguishable from each
other: `ACC_RAM #34D399` and `ACC_GPU #2DD4BF` are **ΔE 5.2 apart under normal
vision, 4.9 under deuteranopia** (OKLab ×100, Machado 2009 @ 1.0). The floor
for "telling two neighbours apart" is 15 and 8 respectively.

**4. Spacing is ad hoc.** `draw_main` alone uses `s(3) s(4) s(6) s(7) s(8)
s(10) s(12) s(14) s(20) s(21) s(22) s(25) s(26) s(30) s(32) s(39) s(42) s(46)
s(52) s(60) s(118)`. And `panel_height` (`panel.rs:1085`) is:

```rust
self.s(12 + 26 + 30 + 50 + 26) + metrics + drives * self.s(42) + temps + mcp + self.s(12)
```

— a hand-maintained sum of magic numbers that must agree with the paint code by
inspection. There is no scale, so every new element is fitted by eye against
its neighbours and nothing shares a rhythm.

**5. Cards have no edges.** `hover_fill` (`panel.rs:1293`) fills a rectangle
with `card` or `card_hover`. Dark `card #212328` on `bg #18191C` is a **1.20:1**
step — at that separation the row does not read as a surface, it reads as a
slightly-off patch. `CreateRoundRectRgn` and `RoundRect` are in the crate and
unused.

**6. There is no pressed state and no focus ring.** `click` acts on
`WM_LBUTTONDOWN` (`panel.rs:662`) and paints nothing between press and result.
Hover exists everywhere; press exists nowhere.

**7. Chip ink is a hardcoded near-black.** `gdi::rgb(15, 17, 20)` appears at
`panel.rs:2257`, `:2612`, `:3230`, `:4231` as the label colour on an accent
fill, regardless of theme or of that accent's luminance. Any accent dark enough
to need white ink gets black ink instead.

**8. The whole header bar is the Back button.** `header_ex` pushes
`(bar, Action::Back)` for the full-width bar (`panel.rs:3125`), so the title
text is a button whose only affordance is a hover fill. The chevron is the
control; it should look like one and own its own hit box.

**9. Four fonts, two weights.** `Fonts::new` (`gdi.rs:118-123`) builds
14/400, 14/700, 12/700, 11/400. There is no semibold, no display size for the
one number a drill-down is actually about, and no distinct treatment for the
ALL-CAPS section headings (`"HOW OFTEN TO UPDATE"`, `panel.rs:3902`), which are
drawn in the same 11/400 as body microcopy and therefore read as a whisper
rather than a heading.

**10. The drive bar carries no state.** `gdi::bar` (`gdi.rs:428`) is a two-tone
rectangle. A 97 %-full disk and a 40 %-full disk differ only in length.

**11. The widget is a hard-edged rectangle.** `SetLayeredWindowAttributes(hwnd,
0, 235, LWA_ALPHA)` (`widget.rs:196`) over square corners on a Windows 11
taskbar of rounded elements.

**12. The FPS overlay's shadow is unscaled.** `overlay.rs:127` offsets by a
literal `+2` while everything around it is `* s.scale`.

---

## 2. Type

Segoe UI throughout — correct choice, keep it. Segoe UI Semibold (weight 600)
ships with every supported Windows and is currently unused; it is the biggest
single improvement available for one line of code.

### The scale

Six steps. Sizes are **logical px, before `scale`** — `Fonts::new` already
multiplies by `scale` (`gdi.rs:116`).

| token | size | weight | tracking | role |
|---|---:|---:|---:|---|
| `display` | 26 | 600 | −10 | the one hero number per drill-down / watch view |
| `title` | 15 | 600 | 0 | view titles in `header_ex`, watched app name |
| `value` | 14 | 600 | 0 | metric-row values, list values, drive figures |
| `body` | 13 | 400 | 0 | list rows, setting labels, prose |
| `label` | 11 | 600 | +40 | metric names, chip labels, ALL-CAPS section headings |
| `micro` | 10 | 500 | +10 | axis ticks, timestamps, units, host names |

Tracking is in 1/1000 em and is applied with **`SetTextCharacterExtra(dc, n)`**
(gdi32, already linked, currently unused) — `n = round(size * tracking / 1000)`
in device px. It must be reset to 0 after the run, exactly like `SetBkMode`.
This is what makes an 11 px ALL-CAPS heading read as a heading rather than as
shouted body text, and it costs one call.

Negative tracking on `display` tightens a big number so `74.3%` does not look
loose — the standard optical correction at 26 px.

### Semantic roles, concretely

- **One `display` per view, or none.** The drill-down's current value and the
  watch view's headline metric. Never two.
- `title` replaces `fonts.bold` in `header_ex` (`panel.rs:3121`). 15/600 instead
  of 14/700: larger, lighter, calmer.
- `value` replaces `fonts.bold_sm` (12/700) in every list value column
  (`panel.rs:3312`, `:3484`, `:3800`). 14/600 is more legible and stops the
  value column looking like a footnote.
- `body` at 400 is the reading weight. Everything that is a sentence uses it.
- `label` at 600 + tracking is the *identity* step: `CPU`, `RAM`, `NETWORK`,
  chip text, `THEME`, `ALERT ME WHEN`.
- `micro` is the only step below 11 and is reserved for text that is not read,
  only glanced at: chart ticks, `14:22`, `443 https`.

### `fit_stack`

`Fonts::fit_stack` (`gdi.rs:127`) becomes three role-specific stacks, because
one stack cannot serve both a value and a title:

```rust
pub fn fit_value(&self) -> [HFONT; 3] { [self.value, self.body, self.label] }
pub fn fit_title(&self) -> [HFONT; 3] { [self.title, self.value, self.body] }
pub fn fit_body(&self)  -> [HFONT; 2] { [self.body,  self.micro] }
```

Cost: 6 `HFONT`s instead of 4. `Fonts::destroy` (`gdi.rs:134`) already handles
the set as an array; extend the array.

### Numerals

Segoe UI's figures are proportional, so a value that ticks between `9.4%` and
`10.1%` shifts horizontally every frame. GDI has no OpenType feature switch.
The fix is layout, not font: **every numeric value is right-aligned to a fixed
column**, using the existing `gdi::text_right` (`gdi.rs:218`). The metric rows
already left-align their values (`panel.rs:2462`) and visibly jitter; move them
to a right edge at `spark.left - SP3` and the jitter is gone. Free.

The unit label that follows a value is fixed-width for the life of a row —
`read`, `down`, `used`, `fps` never change — so the value's own right edge stays
at `spark.left - SP3 - label_w - SP2` and the anti-jitter guarantee holds with
the label in place.

### The value line

A metric row carries up to three pieces of text on the right: the value, its
unit, and sometimes a secondary figure. Two rules:

1. **The value is baseline-centred on the card midline in every row**, whether
   or not that row has a secondary figure.
2. **The secondary figure is bottom-anchored**, drawn as its own `text_right`
   rather than concatenated into the value string.

This exists because the obvious layout — stack value over secondary and centre
the pair — is wrong. It puts the value ~5 px above the midline in rows that
have a secondary and exactly *on* the midline in rows that don't, so scanning
the value column down the panel the numbers zigzag, and the value no longer
lines up with its own metric name. Taking the secondary out of the vertical flow
costs nothing and fixes both.

| row | value + unit | secondary | fitted size |
|---|---|---|---|
| CPU | `41.7%` | — | 14 |
| RAM | `21.6 GB used` | `of 32 GB · 67%` | 14 |
| GPU | `63.0%` | — | 14 |
| FPS | `141 fps` | `in Cyberpunk2077.exe` | 14 |
| Disk | `88 MB/s read` | `12 MB/s write` | 14 |
| Network | `12.4 MB/s` + ↓ marker | `0.9 MB/s` + ↑ marker | **12** |
| Sound | `2 playing` | — | 14 |

### Direction markers, not words

`down` and `up` are set as **glyphs, not words**. The word `down` measures
~25 px at the micro step; the marker measures 6. Network is the row where that
matters: `NETWORK` is the longest metric name in the product and a rate like
`12.4 MB/s` is the widest value, and the two share one ~144 px band. With words
they collide outright.

`read` and `write` stay as words. They are not directions, and an arrow beside a
disk read rate would imply the disk is *downloading*. Disk has ~29 px of
clearance with the words in place, so nothing forces the change.

### Fitting the value

Even with markers, Network does not clear at the value step. So the value is
**measured and stepped down**, which is what `GetTextExtentPoint32` is for:

```
budget = row_w - 2*SP4 - spark_w - SP4 - SP3      // ≈ 144 at PANEL_W 336
for size in [14, 13, 12]:
    if name_w + value_w(size) + marker_w <= budget: break
```

Measured in the mockup at 336 px: every row holds 14 except Network, which
lands at **12** with 4.7 px of clearance. The app-detail flyout's shorter
`2.1 MB/s` lands at **13**.

Cost: two extra semibold `HFONT`s (13 and 12), taking the set from six to eight.
`Fonts::destroy` (`gdi.rs:134`) already frees the set as an array.

Two levers exist if a uniform 14 everywhere is worth more than the sparkline's
width or the full metric name — **neither is taken here**:

| lever | frees | cost |
|---|---|---|
| sparkline 120 → 104 px | 16 px | 13 % of the chart, the thing the product is for |
| label `NETWORK` → `NET` | ~30 px | loses a full metric name, against the grain of naming things properly |

**A hard backstop sits under the ladder:** if even 12 will not clear, the metric
name ellipsises. The value and the name must never be able to overlap, whatever
a future string does.

It also retires the `"↓ {} · ↑ {}"` and `"R {} · W {}"` strings, which buried
the primary figure's label at the far end of a line shared with the secondary
figure — the reader had to parse the whole string to learn that the big number
was the down rate.

---

## 3. Spacing

### The scale

Base unit **4**. Eight steps, in logical px before `scale`:

| token | value | use |
|---|---:|---|
| `SP0` | 0 | flush |
| `SP1` | 2 | hairlines, icon-to-baseline nudges, mark gaps |
| `SP2` | 4 | inside a control (chip padding is `SP2` vertical) |
| `SP3` | 8 | between a label and its value; icon-to-text |
| `SP4` | 12 | **the panel gutter** (today's `pad`) and the gap inside a card |
| `SP5` | 16 | between rows of related controls |
| `SP6` | 24 | between sections |
| `SP7` | 32 | above a section heading that follows content |

Two derived rhythms, both multiples of 4:

| token | value | replaces |
|---|---:|---|
| `CTRL_H` | 24 | unchanged (`util.rs:6`) ✓ |
| `CTRL_GAP` | 8 | was 6 (`util.rs:9`) |
| `ROW_LIST` | 28 | was `s(26)` (`panel.rs:3284`, `:2350`, `:3802`) |
| `ROW_NAV` | 32 | was `nav_row_h() = 30` (`panel.rs:3135`) |
| `ROW_NAV_STRIDE` | 40 | was 36 (`panel.rs:3138`) |
| `CARD_METRIC` | 48 | was `s(46)` (`panel.rs:2453`) |
| `ROW_METRIC` | 56 | was `s(52)` (`panel.rs:2465`) |
| `HEADER_H` | 40 | unchanged (`panel.rs:3078`) ✓ |
| `HEADER_STRIDE` | 52 | was `40 + 10` (`panel.rs:3130`) |

Implementation: a `const` block in `util.rs` beside `CTRL_H`, and `self.s(SP4)`
in place of `self.s(12)`. No new machinery — `s()` already exists and every
constant already flows through it.

### Mapping onto `draw_main`

The metric row (`panel.rs:2453-2465`) as it stands and as proposed:

| element | today | proposed |
|---|---|---|
| card | `top: y, bottom: y + s(46)` | `y … y + s(CARD_METRIC=48)`, 4 px radius, 1 px `line` border |
| label x | `row.left + s(10)` | `row.left + s(SP4)` = 12 |
| label y | `y + s(6)` | `y + s(SP3)` = 8 |
| value y | `y + s(21)` | baseline-centred on the card midline, in **every** row (see §2) |
| unit label | part of the value string | `micro`/`mute`, `SP2` to the right of the value, sharing its baseline |
| secondary y | part of the value string | separate `text_right`, bottom-anchored at `y + s(CARD_METRIC) - s(SP2)` |
| chart box | `right - s(118) … right - s(8)`, `y+s(7) … y+s(39)` | `right - s(120) … right - s(SP4)`, `y + s(SP3) … y + s(40)` |
| stride | `y += s(52)` | `y += s(ROW_METRIC=56)` |

`panel_height` (`panel.rs:1085`) becomes a sum of the same named constants,
which is the point: it stops being a magic number and starts being derivable
from the tokens the paint code uses.

```rust
self.s(SP4)                              // top gutter
  + self.s(HEADER_STRIDE)                // header strip
  + self.s(CTRL_H + SP3)                 // find-app row
  + metrics * self.s(ROW_METRIC)
  + self.s(SP6)                          // DRIVES heading
  + drives * self.s(DRIVE_ROW)
  + mcp
  + self.s(SP4)                          // bottom gutter
```

### Reflow at other text sizes

`scale = dpi_scale * text_scale(text_size)` (`panel.rs:476`) and the flyout is
re-sized whenever the scale changes (`panel.rs:1659-1664`). Because the whole
scale is multiplicative, nothing here reflows differently from today — the
panel grows, the type grows, the ratios hold. Two consequences to respect:

- Values that must fit *within* a card use `text_fit` against a right edge
  derived from the same tokens, so a wider string at "larger" degrades a step
  instead of overflowing.
- The chart box shrinks proportionally, so its gridline count and tick labels
  must be gated on **device** height, not logical: below `s(64)` device px a
  chart drops its gridlines, and below `s(40)` it drops its ceiling label. See
  `charts.md` §4.

---

## 4. Colour and theme

### The structural change

Accents move out of module constants and into the `Theme` struct.

```rust
pub struct Theme {
    pub name: &'static str,
    // surfaces
    pub bg: u32, pub card: u32, pub card_hover: u32, pub card_press: u32,
    pub line: u32, pub grid: u32, pub track: u32,
    pub input_bg: u32, pub input_border: u32,
    // ink
    pub text: u32, pub dim: u32, pub mute: u32,
    // accents
    pub cpu: u32, pub ram: u32, pub gpu: u32,
    pub disk: u32, pub net: u32, pub audio: u32, pub fps: u32,
    // status
    pub danger: u32, pub warn: u32, pub good: u32,
}
```

`ACC_CPU` and friends become `gdi::t().cpu`. `accent_for(metric)`
(`panel.rs:4575`) and `metric_label` (`panel.rs:4502`) already funnel every
lookup through one place, so this is a mechanical change with one call-site
pattern. Cost: the `THEMES` array grows from 8 fields × 3 to 22 × 3 — 264 bytes
of `.rodata`. Nothing.

Two helpers replace the ad-hoc arithmetic:

```rust
/// Linear blend of two opaque colours. Because every surface underneath is a
/// known solid, a precomputed mix is pixel-identical to an alpha blend and
/// costs nothing at paint time.
pub fn mix(a: u32, b: u32, t: f32) -> u32;

/// Ink that will be legible on `fill`: near-black above ~0.45 relative
/// luminance, near-white below. Replaces the hardcoded rgb(15,17,20) at
/// panel.rs:2257, :2612, :3230, :4231.
pub fn on(fill: u32) -> u32;
```

`shade(c, f)` (`gdi.rs:89`) stays — it is `mix(c, black, f)` and reads well at
its call sites — but new code uses `mix` against the actual surface, which is
what makes a wash sit *on* the card rather than fade toward black.

### Surfaces and ink

**Dark** (default)

| token | today | proposed | rgb |
|---|---|---|---|
| `bg` | `#18191C` | `#131417` | 19, 20, 23 |
| `card` | `#212328` | `#1E2024` | 30, 32, 36 |
| `card_hover` | `#2D3037` | `#282B31` | 40, 43, 49 |
| `card_press` | — | `#191B1F` | 25, 27, 31 |
| `line` | — | `#2C2F35` | 44, 47, 53 |
| `grid` | — | `#303236` | 48, 50, 54 |
| `track` | `#32353C` | `#2E3239` | 46, 50, 57 |
| `input_bg` | `#0F1013` | `#0E0F12` | 14, 15, 18 |
| `input_border` | `#4C525C` | `#3E434C` | 62, 67, 76 |
| `text` | `#E6E8EB` | `#E8EAED` | 232, 234, 237 |
| `dim` | `#8C9198` | `#9096A0` | 144, 150, 160 |
| `mute` | — | `#6E747E` | 110, 116, 126 |

`card` on `bg` goes from **1.20:1 to 1.35:1** — enough for the card to read as
a raised plane without turning the panel into a set of grey boxes. `text` on
`card` 13.5:1; `dim` 5.5:1; `mute` 3.5:1 (labels only, never prose).

**Black** (OLED)

| token | proposed | rgb |
|---|---|---|
| `bg` | `#000000` | 0, 0, 0 |
| `card` | `#0B0C0E` | 11, 12, 14 |
| `card_hover` | `#17191D` | 23, 25, 29 |
| `card_press` | `#060708` | 6, 7, 8 |
| `line` | `#1C1F24` | 28, 31, 36 |
| `grid` | `#1F2126` | 31, 33, 38 |
| `track` | `#1E2126` | 30, 33, 38 |
| `input_bg` | `#060607` | 6, 6, 7 |
| `input_border` | `#343941` | 52, 57, 65 |
| `text` | `#EDEFF2` | 237, 239, 242 |
| `dim` | `#8C929B` | 140, 146, 155 |
| `mute` | `#6A7079` | 106, 112, 121 |

Black uses the **same accents as dark** — contrast only improves (6.8–9.4:1 on
`card`).

**Light**

| token | today | proposed | rgb |
|---|---|---|---|
| `bg` | `#F3F4F6` | `#F1F2F5` | 241, 242, 245 |
| `card` | `#FFFFFF` | `#FFFFFF` | 255, 255, 255 |
| `card_hover` | `#E5E7EB` | `#EDEFF3` | 237, 239, 243 |
| `card_press` | — | `#E4E7EC` | 228, 231, 236 |
| `line` | — | `#E3E6EB` | 227, 230, 235 |
| `grid` | — | `#EAEAEB` | 234, 234, 235 |
| `track` | `#D1D5DB` | `#DFE3E9` | 223, 227, 233 |
| `input_bg` | `#FFFFFF` | `#FFFFFF` | 255, 255, 255 |
| `input_border` | `#A0A6AF` | `#C3C9D2` | 195, 201, 210 |
| `text` | `#181C21` | `#14171C` | 20, 23, 28 |
| `dim` | `#697078` | `#5C636E` | 92, 99, 110 |
| `mute` | — | `#767D88` | 118, 125, 136 |

`dim` on white moves from 4.8:1 to **6.1:1**; `card_hover` softens from a hard
grey to a tinted one so hover stops looking like a disabled state.

### The metric accents

Two tuned sets, one per surface family. The hue *families* are unchanged —
blue, green, violet, coral, cyan-teal, amber, magenta — so the product still
looks like itself. **One assignment is swapped: GPU takes violet and Disk takes
the cyan-teal**, because the current GPU teal `#2DD4BF` and RAM green `#34D399`
are ΔE 5.2 apart and cannot be told apart by anyone.

**Dark / Black**

| metric | today | proposed | rgb | contrast on `card` |
|---|---|---|---|---:|
| CPU | `#4FA3FF` | `#4A9CF6` | 74, 156, 246 | 5.7:1 |
| RAM | `#34D399` | `#46BE71` | 70, 190, 113 | 6.9:1 |
| GPU | `#2DD4BF` | `#AC89FC` | 172, 137, 252 | 6.0:1 |
| Disk | `#A78BFA` | `#2CC3D2` | 44, 195, 210 | 7.7:1 |
| Network | `#F59E0B` | `#F1A427` | 241, 164, 39 | 7.8:1 |
| FPS | `#FF6B6B` | `#F5746D` | 245, 116, 109 | 5.9:1 |
| Sound | `#E879F9` | `#EB6EC9` | 235, 110, 201 | 5.9:1 |

**Light**

| metric | proposed | rgb | contrast on `card` |
|---|---|---|---:|
| CPU | `#1873DC` | 24, 115, 220 | 4.6:1 |
| RAM | `#1A8731` | 26, 135, 49 | 4.6:1 |
| GPU | `#8B5BD4` | 139, 91, 212 | 4.6:1 |
| Disk | `#008193` | 0, 129, 147 | 4.6:1 |
| Network | `#A76603` | 167, 102, 3 | 4.6:1 |
| FPS | `#D33E47` | 211, 62, 71 | 4.6:1 |
| Sound | `#B63991` | 182, 57, 145 | 5.3:1 |

**Status** (all themes, distinct from every metric accent — a status colour must
never impersonate a series):

| token | dark / black | light |
|---|---|---|
| `danger` | `#F2555A` | `#C2262E` |
| `warn` | `#E8A33A` | `#9A5F00` |
| `good` | `#46BE71` | `#1A8731` |

`danger` replaces the four hardcoded reds — `rgb(220, 90, 90)`
(`panel.rs:3313`, `:3737`, `:4389`, `:2855`) and `rgb(230, 100, 100)`
(`panel.rs:2238`, `:3563`).

### Validation

Run against `dataviz`'s validator (OKLab ΔE ×100; Machado–Oliveira–Fernandes
2009 CVD simulation at severity 1.0):

| pairlist | dark, surface `#18191C` | light, surface `#FFFFFF` |
|---|---|---|
| adjacent CVD ΔE (target ≥ 8) | **14.2** PASS | **12.5** PASS |
| adjacent normal ΔE (floor ≥ 15) | **22.8** PASS | **21.2** PASS |
| chroma floor ≥ 0.10 | PASS | PASS |
| contrast vs surface ≥ 3:1 | PASS (5.7 – 7.8) | PASS (4.6 – 5.3) |
| all-pairs normal ΔE | 12.1 (GPU ↔ CPU) | 13.0 (Disk ↔ CPU) |

"Adjacent" is the default metric row order — `cpu, ram, gpu, fps, disk, net,
audio` (`draw_main`, `panel.rs:2371-2435`) — and the widget's segment order
(`widget.rs:136`). Those are the pairs that ever touch.

**Two documented deviations, both deliberate:**

1. *All-pairs separation.* Seven categorical hues cannot clear the all-pairs
   floor of 15 in any ordering; the method's own guidance caps all-pairs forms
   at about three series. The mitigation is structural and permanent: **no
   accent in this app ever appears without its own text label beside it** —
   `CPU`, `RAM`, `GPU`, `Disk`, `Network`, `Sound`, `FPS` are drawn in every
   metric row (`panel.rs:2455`), every widget segment (`widget.rs:329`), every
   watch row (`panel.rs:3604`) and every settings entry (`panel.rs:4450`). Hue
   is a redundant channel here, never the sole carrier of identity. The
   improvement from ΔE 5.2 to 12.1 is nonetheless real and worth having.
2. *Lightness band.* The dark accents sit at OKLCH L 0.68 – 0.78, above the
   method's 0.48 – 0.67 dark band. That band is calibrated for *marks* at 3:1.
   These accents double as **11 px text** (the metric label), so they are held
   to WCAG text contrast of 4.5:1 instead, which they clear at 5.7 – 7.8:1. The
   stricter requirement wins.

### Rules that fall out

- An accent may sit as text on `card`, never on `bg`, in the light theme
  (4.1:1 on `bg` vs 4.6:1 on `card`).
- Text never wears an accent **except** the metric's own name. Values, times and
  prose use `text` / `dim` / `mute`. This deletes the accent-coloured value
  column at `panel.rs:3736`.
- Chart marks always wear the accent; chart *labels* never do.
- `on(fill)` chooses chip ink. Nothing hardcodes an ink colour again.

---

## 5. Iconography

No SVG loader, so every icon is geometry. All are specified on a **16-unit
grid**, origin at the icon's centre, so a call site passes a centre point and a
box size `S` and the routine works in `u = S/16`. This matches how
`gdi::chevron` and `gdi::disclosure` already work (`gdi.rs:300`, `:344`) and
extends them.

```rust
/// One icon routine per glyph, all with this shape.
pub fn icon_gear(dc: HDC, cx: i32, cy: i32, s: i32, w: i32, c: u32);
//                        centre        box   stroke  colour
```

Default box `S = s(14)`, stroke `w = max(1, s(2) / 2)` for hairline glyphs and
`s(2)` for the heavier navigation marks. Every icon is stroked with
`CreatePen(PS_SOLID, w, c)` + `Polyline`/`Polygon`/`Arc`, or filled with
`Polygon`/`Ellipse`. **Round joins need `ExtCreatePen` with `PS_GEOMETRIC |
PS_JOIN_ROUND | PS_ENDCAP_ROUND`** — available (`windows-sys` Gdi:113), and
worth it for the chevrons and the sound waves, where a mitre corner is visibly
wrong at 14 px. Note that `PS_GEOMETRIC` pens ignore `SetROP2` niceties and are
slightly slower; at ten icons per frame that is not measurable.

### Navigation and chrome

**`gear`** — settings. Replaces the word `"settings"` at `panel.rs:2298`.
6 teeth, filled `Polygon`, hub punched with `Ellipse` in the surface colour.

```
teeth = 6;  R_out = 7.4u;  R_in = 5.4u;  hub = 2.4u
pitch = 2π/6;  half = pitch * 0.21          // tooth spans 42% of its pitch
for k in 0..6:
    a = k * pitch
    push polar(R_out, a - half)
    push polar(R_out, a + half)
    push polar(R_in,  a + half + pitch*0.06)
    push polar(R_in,  a + pitch - half - pitch*0.06)
Polygon(24 points, brush = c)
Ellipse(cx-hub, cy-hub, cx+hub, cy+hub, brush = surface)
```

Six teeth, not eight: at 14 px an 8-tooth gear turns to mush.

**`close`** — two strokes, `w = s(2)`, from ±4.5u:
`(-4.5u,-4.5u)→(4.5u,4.5u)` and `(4.5u,-4.5u)→(-4.5u,4.5u)`.
Replaces the `"×"` glyph at `panel.rs:2223`. Uses `dim`, `text` on hover —
**never `danger`**, because close is not destructive.

**`kill`** — the destructive one, and it must stop being the same mark as
close. A **trash glyph**: lid `(-5u,-3.5u)→(5u,-3.5u)`; handle
`(-2u,-3.5u)→(-2u,-5u)→(2u,-5u)→(2u,-3.5u)`; body polyline
`(-4u,-3.5u)→(-3.2u,5.5u)→(3.2u,5.5u)→(4u,-3.5u)`; two ribs at
`x = ±1.4u, y = -1u…3.5u`. Colour `dim`, `danger` on hover. Replaces
`panel.rs:3313`, `:3737`, `:4389`.

**`chevron`** — keep `gdi::chevron` exactly (`gdi.rs:300`), add round caps and
retire the `"›"` glyph at `panel.rs:3151` in its favour.

**`disclosure`** — keep `gdi::disclosure` (`gdi.rs:344`) and retire the
`"▾"`/`"▸"` strings at `panel.rs:2753` and `:2825`.

**`pin`** / **`unpin`** — a pushpin: head `Ellipse` r = 3u centred at
`(0,-3u)`, shaft `Polyline (0,0)→(0,6u)` at `w = s(2)`, two shoulder strokes
`(-4u,-0.5u)→(4u,-0.5u)`. `unpin` is the same glyph rotated 35° — trivial with
`SetWorldTransform` after `SetGraphicsMode(dc, GM_ADVANCED)`, or by rotating the
points in Rust, which is cheaper and has no state to restore. Use the latter.

**`on_top`** — a window rectangle `(-6u,-4u,6u,5u)` with a 2u-tall filled title
band, plus a chevron-up above it at `(0,-6.5u)`.

**`search`** — magnifier: `Arc`/`Ellipse` ring r = 4u centred `(-1u,-1u)`,
handle `(2u,2u)→(5.5u,5.5u)` at `w = s(2)`. Goes inside the find-app frame at
`panel.rs:2311`, replacing the word `"Find app"` — which reclaims 60 logical px
of input width.

**`pause`** / **`play`** — two 2.5u × 10u filled rects at `x = ±3u`; play is a
triangle `(-3.5u,-5u), (5u,0), (-3.5u,5u)`. Replaces the words at
`panel.rs:3220`.

**`check`** — for checkboxes. Polyline `(-3.5u,0.3u)→(-1u,3u)→(4u,-3.2u)`,
`w = s(2)`, round caps, drawn in `on(accent)` over the filled box. Today the
"checked" state is a smaller filled square inside a bigger one
(`panel.rs:4477-4483`), which is a radio button's idiom, not a checkbox's.

**`grip`** — the three-bar drag handle at `panel.rs:4425-4429` is already
correct. Keep, restyle to two bars of 1u height at `y = ±2u`, width 10u.

**`dot`** — the agent status marker (`panel.rs:2924`). Replace the
`fill`-then-punch square with `Ellipse`, filled for live and stroked at
`w = s(2)` for finished. `◆`/`◇` at `panel.rs:2554`, `:2568` and
`widget.rs:322-324` become the same two dots.

### Per-metric glyphs

These give the metric rows and the widget an identity beyond colour — the
second redundant channel that makes the all-pairs deviation in §4 safe.

| metric | construction (units of `u`, centre origin) |
|---|---|
| **CPU** | square outline `(-4,-4,4,4)`, inner square outline `(-1.6,-1.6,1.6,1.6)`, plus 3 legs per side: for `i in [-2,0,2]` strokes `(i,-4)→(i,-6.5)`, `(i,4)→(i,6.5)`, `(-4,i)→(-6.5,i)`, `(4,i)→(6.5,i)` |
| **RAM** | module outline `(-6.5,-3.5,6.5,3)`, notch `(-1,3)→(-1,1.5)→(1,1.5)→(1,3)`, 5 pins `(x,3)→(x,5)` at `x ∈ {-5,-2.5,0,2.5,5}` |
| **GPU** | board outline `(-6.5,-4,6.5,4)`, fan `Ellipse` r = 2.8 at centre, 3 blades from the hub at 120° as short strokes r 1 → 2.6 |
| **Disk** | two stacked platters: `Ellipse (-6,-4.5,6,-1.5)` and `Ellipse (-6,1.5,6,4.5)`, side walls `(-6,-3)→(-6,3)` and `(6,-3)→(6,3)` |
| **Network** | a globe: `Ellipse (-6.3,-6.3,6.3,6.3)`, meridian `Ellipse (-2.85,-6.3,2.85,6.3)`, equator `(-6.3,0)→(6.3,0)`. Three calls. **Not** the opposed-arrow pair originally specified here — the arrows became this row's unit markers (§2), and one motif cannot carry both identity and direction. Still replaces the `"↓"`/`"↑"` glyphs at `panel.rs:2412` |
| **Sound** | speaker `Polygon (-5,-2)(-2,-2)(1,-5)(1,5)(-2,2)(-5,2)`, plus two `Arc`s at r = 3.5 and r = 6 spanning ±50° about the +x axis |
| **FPS** | frame `(-6.5,-4.5,6.5,4.5)` with a bolt inside: `Polygon (0.5,-3)(-2,0.3)(-0.3,0.3)(-0.8,3)(2,-0.3)(0.2,-0.3)` |
| **Connections** | two nodes `Ellipse` r = 2 at `(-4.5,-3)` and `(4.5,3)`, link `(-3,-1.6)→(3,1.6)` |
| **AI / agent** | a 6-pointed asterisk: 3 strokes through the origin at 0°, 60°, 120°, half-length 5.5 |

### Direction markers

Two more glyphs, drawn at 10 u and 9 u rather than 13, used as the unit beside a
rate rather than as an icon in their own right (see §2):

| glyph | construction (units of `u`, centre origin) |
|---|---|
| **`down`** | stem `(0,-6.6)→(0,4.4)`, head `(-3.8,0.9)→(0,5.8)→(3.8,0.9)` |
| **`up`** | the same, mirrored in y |

Two `Polyline`s each, on the pen §5 already establishes. They take `mute`, never
an accent — the accent is spent on the metric name, and a marker that competed
with it would read as a third identity channel that means nothing.

These markers are the reason **the Network metric glyph changed to a globe**.
As originally specified it was itself two opposed arrows, so that row would have
worn the arrow motif twice — once as identity, once as direction. Identity and
direction are two different channels and must not share a shape. The markers
won the arrows because direction is all they can mean; a globe says "network"
without claiming a direction.

Every one of these is 2–8 GDI calls. A whole main panel draws about a dozen
icons per frame; measured against the ~200 `FillRect`/`TextOut` calls already
in `draw_main`, the addition is noise.

### Antialiasing

`Polyline` at 1 px is aliased, and a 45° gear tooth or a fan blade looks
ragged. §7 introduces one primitive that fixes this for every icon and every
chart at once, without GDI+.

---

## 6. Surfaces

### The elevation ladder

| level | dark | black | light | use |
|---|---|---|---|---|
| L0 | `bg` | `bg` | `bg` | the window ground |
| L1 | `card` + 1 px `line` | `card` + 1 px `line` | `card` + 1 px `line` | rows, cards, chart plates |
| L2 | `card_hover` | `card_hover` | `card_hover` | hover |
| L3 | `card_press` | `card_press` | `card_press` | pressed (recedes, in all three themes) |

Elevation is carried by **border + fill**, never by a drop shadow. A shadow in
GDI means either a blurred DIB composite (expensive, per frame) or a hard offset
rectangle (looks like 2003). The 1 px `line` at 1.22:1 against `card` reads as a
lifted edge and costs one `FillRect` per side — or one `FrameRect`.

### Radius

**4 logical px** on cards, chips, input frames, chart plates and the widget
strip. **2 px** on bars and chart marks. Nothing is fully round; a pill chip at
`CTRL_H = 24` would fight the rectangular data.

GDI options, in order of preference:

1. **Corner sprites (recommended).** Because a card's fill and the surface
   behind it are both known solids, the four 4×4 corner patches can be computed
   once per `(radius, fill, behind)` pair and cached. Drawing a card is then
   `FillRect` for the body plus 4 × 16 = 64 pixel writes straight into the back
   buffer's DIB. This is exact, antialiased, and cheaper than `RoundRect`.
   Requires §7.
2. `CreateRoundRectRgn` + `FillRgn` — aliased, but at r = 4 on a dark card the
   staircase is 3 pixels and barely visible. Acceptable fallback, zero new
   machinery.
3. `RoundRect` with a pen and brush — aliased *and* the pen's geometry at r = 4
   is unpredictable across DPI. Avoid.

The corner cache is keyed on `(r, fill, behind)`; a panel has at most a dozen
live combinations, so a `Vec<(key, [u32; 4*r*r])>` of a few hundred bytes covers
every frame.

### Interactive states

```
rest      L1  card      + line
hover     L2  card_hover + line lifted one mix step toward text
press     L3  card_press + line
focus     L1  card      + 1px accent ring (the metric's accent, or cpu for chrome)
selected  accent fill, ink from on(accent)
disabled  card, ink = mute
```

**Press is new.** Implementation: `click` (`panel.rs:1315`) already runs on
`WM_LBUTTONDOWN`. Add a `pressed: Option<usize>` index into `self.hits`, set it
on button-down before dispatching, clear it on `WM_LBUTTONUP`, and have
`hover_fill` consult it. Ten lines. The action still fires on down, so nothing
about the interaction model changes — the user simply sees the press land.

**Focus ring is new and matters** because the panel puts keyboard focus in an
`EDIT` child (`panel.rs:1138`) and offers no visible focus anywhere else. A 1 px
accent ring inset 1 px from the card edge, drawn only when the panel has
keyboard focus and a hit index is "current".

### Input frames

`gdi::input_frame` (`gdi.rs:421`) draws border-then-interior. Keep the shape,
add the radius and swap the border for a **2 px accent underline on focus**
rather than a full accent border — a full ring around a 24 px input reads as an
error state. `WM_CTLCOLOREDIT` (`panel.rs:748`) already supplies the child's own
background; nothing changes there.

### The widget strip

Round the layered window to 6 px with `SetWindowRgn(hwnd,
CreateRoundRectRgn(0, 0, w+1, h+1, 12, 12), TRUE)` at create and after every
`WM_SIZING` (`widget.rs:394`). One call, and the strip stops being the only
square thing on a Windows 11 taskbar. Raise the alpha from 235 to 242
(`widget.rs:196`) — at 235 the taskbar shows through enough to muddy the accent
labels.

---

## 7. The one new primitive

Everything above — antialiased icons, antialiased chart curves, exact rounded
corners, real washes — wants sub-pixel coverage. There are three ways to get it.

### Option A — GDI+ (`gdiplus.dll`). Not recommended.

**What it buys:** `GraphicsPath`, antialiased `DrawPath`/`FillPath`, real alpha
brushes, `LinearGradientBrush`, curve fitting. Genuinely the best curve quality
available without shipping a rasteriser.

**What it costs:**

- `gdiplus.dll` is present on every supported Windows, so it is not a third-party
  dependency — but it *is* a ~1.7 MB DLL mapped into the process, plus
  `GdiplusStartup` on the UI thread. Against an 840 KB binary whose entire
  identity is "lightweight", doubling the resident footprint to draw seven
  sparklines is a bad trade.
- `Win32_Graphics_GdiPlus` in `windows-sys` is FFI declarations only, so the
  *binary* grows by a few KB — the cost is runtime, not size.
- GDI+ text does not match GDI text metrics, so a mixed pipeline needs either
  all-GDI+ text (different rasteriser, different ClearType behaviour, visible
  seam against the `EDIT` children) or careful separation.
- GDI+ path rendering is roughly an order of magnitude slower than GDI per call.
  Irrelevant at seven sparklines; not irrelevant at a 32-core small-multiples
  grid on a hover repaint.

**Flag it explicitly, as asked:** GDI+ would give the best-looking curves and I
am recommending against it, because option B gets 90 % of the quality for about
1 KB of Rust and no new DLL.

### Option B — a DIB-backed back buffer with a coverage rasteriser. Recommended.

Change one line in `BackBuffer::new` (`gdi.rs:151`): `CreateCompatibleBitmap`
becomes `CreateDIBSection` with a 32 bpp top-down `BITMAPINFO` — exactly the
pattern `tray.rs:246-253` already uses for the tray icons. The DC still works
with every GDI call it works with today, `BitBlt` presents identically, and we
now hold `bits: *mut u32` into the pixels.

That unlocks, in ~200 lines of straightforward Rust:

```rust
/// Antialiased polyline, Xiaolin Wu coverage, width in fixed-point px.
pub fn aa_polyline(bb: &BackBuffer, pts: &[(f32, f32)], w: f32, color: u32);
/// Antialiased filled polygon (scanline coverage) — chart washes, icon fills.
pub fn aa_polygon(bb: &BackBuffer, pts: &[(f32, f32)], color: u32);
/// One antialiased rounded-rect corner, cached per (r, fill, behind).
pub fn corner(bb: &BackBuffer, x: i32, y: i32, r: i32, fill: u32, behind: u32);
```

**Cost, concretely.** Coverage work is proportional to the *ink*, not the area:
a 110 px sparkline at 1.5 px wide touches ~330 pixels. Seven of them is 2 300
pixel blends per paint. The 336 × 96 hero chart with a wash is ~1 000 line
pixels plus a 32 000-pixel scanline fill — and the fill is a straight write, not
a blend, except at the boundary. Total added cost per full paint is on the order
of **40 000 pixel operations**, versus the ~100 000 the existing `FillRect`s
already do. Call it a 20–40 % increase on a paint that currently runs in well
under a millisecond, at 0.5–2 Hz plus hover.

**Risks, honestly.** Writing into the DIB bypasses GDI's clip region, so
`aa_*` must take the clip rect explicitly — the scrolling lists rely on
`IntersectClipRect` (`panel.rs:3290`). And `GdiFlush()` must be called before
touching `bits` after any GDI call, because GDI batches. Both are one-liners,
both are easy to get wrong once and then never again.

### Option C — supersampled DIB. Rejected.

Render the chart into a 4× DIB and box-downsample. Quality is excellent and the
code is trivial, but a 336 × 96 chart at 4× is 1 296 × 384 = 500 000 samples per
chart per paint. On a hover repaint that is visible. Fine for a one-off export;
wrong for an immediate-mode panel.

### What each existing surface gets

| surface | today | with option B |
|---|---|---|
| panel back buffer | `CreateCompatibleBitmap` | `CreateDIBSection`, 32 bpp top-down |
| widget | direct `BeginPaint` HDC | same change; it is a layered window and already composited |
| overlay | direct HDC, colour-keyed | **unchanged** — colour-key transparency and per-pixel alpha do not mix; the FPS number stays plain GDI text with a scaled shadow |
| tray icons | already a DIB (`tray.rs:253`) | reuse `aa_polygon` for the fill bar's rounded top |

`GdiGradientFill` and `GdiAlphaBlend` are worth knowing about and both live in
**gdi32.dll**, not msimg32 (`windows-sys` Gdi:127, :131) — so a vertical gradient
costs no new import at all. `charts.md` §2 uses `GdiGradientFill` clipped to a
`CreatePolygonRgn` for the area wash, which is a better fit than per-pixel work
for that specific job.

---

## 8. Motion

The repaint model is tick-driven — 0.5 s / 1 s / 2 s (`panel.rs:3904-3908`) —
plus a repaint on every hover change (`panel.rs:687`). There is no compositor
and no animation loop. Anything that moves must either ride an existing repaint
or bring its own timer.

**Two things animate. Nothing else does.**

### 1. Hover and press cross-fade — 90 ms

The only motion the user actually feels. Today `hover_fill` (`panel.rs:1293`)
snaps between `card` and `card_hover`; at a 15 % lightness step that snap is
what makes the panel feel like a form rather than an instrument.

Implementation, cheaply:

- `Ui` gains `fade: Option<(usize, u32, f32)>` — hit index, start tick, progress.
- On a hover change, `SetTimer(hwnd, ID_FADE, 16, null)`.
- Each `WM_TIMER` advances progress by `16.0 / 90.0`, invalidates, and
  `KillTimer`s at 1.0.
- `hover_fill` draws `mix(card, card_hover, ease_out(progress))`.

Six frames per transition, one `FillRect` each, and the timer only exists while
a transition is live. `ease_out(t) = 1 - (1-t)²`.

### 2. The live head dot — free

The newest sample on a chart carries a filled dot with a 2 px ring in the
surface colour. On the tick where a new sample arrives the dot is drawn one
pixel larger. This is not a timer — it rides the tick repaint that is happening
anyway — and it is the entire visual language for "this is live". Cost: one
`Ellipse` per chart.

### What must not animate

**Charts do not animate.** No easing of the trace, no sliding window, no
counting-up numbers. A monitor that animates its data is lying about when it
sampled — at a 2 s interval an eased trace would show values that were never
measured. The data is drawn where and when it was taken. Say so, and stop.

**View transitions: not in this generation.** A 120 ms cross-fade between views
is achievable — render the outgoing view into a second `BackBuffer`, then
`GdiAlphaBlend` the incoming one over it across four frames. It costs two full
panel composites per frame for four frames, which is affordable, but it needs
the retained outgoing buffer to survive a `change_view` (`panel.rs:1219`) that
currently resets scroll, filter text and `EDIT` child positions in the same
call. That entanglement is where it would go wrong. Deferred, deliberately.

### The one thing that looks like motion but is not

The chart ceiling. Today the y-scale jumps whenever `ring.max()` changes
(`panel.rs:2400`), which reads as the graph animating. Fixing it — a sticky,
quantised ceiling that rises instantly and decays over ten ticks — makes the
chart *stop* moving, and is the single biggest legibility gain available. It is
specified in `charts.md` §3 and it is a data change, not an animation.

---

## 9. Sequencing

Ordered by ratio of visible improvement to risk.

| # | change | files | risk |
|---:|---|---|---|
| 1 | Accents into `Theme`; the two tuned sets; `mix` / `on` | `gdi.rs`, `panel.rs` call sites | low |
| 2 | Chart ceiling policy — sticky, quantised, percent-pinned | `panel.rs`, `util.rs` | low |
| 3 | Type scale: semibold, six steps, `SetTextCharacterExtra`, right-aligned values | `gdi.rs`, `panel.rs` | low |
| 4 | Spacing tokens; `panel_height` derived from them | `util.rs`, `panel.rs` | low, wide |
| 5 | `BackBuffer` → DIB section; `aa_polyline`, `aa_polygon`, `corner` | `gdi.rs` | medium |
| 6 | The chart (`charts.md`): wash, baseline, gridlines, hover readout | `gdi.rs`, `panel.rs` | medium |
| 7 | Icon set; retire every glyph and every word-as-button | `gdi.rs`, `panel.rs` | low, wide |
| 8 | Rounded cards, borders, press state, focus ring | `gdi.rs`, `panel.rs` | low |
| 9 | Hover fade timer | `panel.rs` | low |
| 10 | Widget region + alpha; overlay shadow scaling | `widget.rs`, `overlay.rs` | low |

Steps 1–4 need no new primitive and would already make the product look
considered. Step 5 is the gate for everything premium.
