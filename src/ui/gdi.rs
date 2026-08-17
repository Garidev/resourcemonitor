//! Minimal GDI helpers: colors, fonts, double buffer, primitives, sparklines.

use windows_sys::Win32::Foundation::{POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;

use crate::util::Ring;

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// The seven metric accents. These live on the theme rather than as module
/// constants because one set cannot serve three grounds: the old accents
/// measured 1.86:1 to 2.78:1 against the light theme's white card, and
/// `ACC_RAM`/`ACC_GPU` were only ΔE 5.2 apart, which is not a distinguishable
/// pair for anyone.
pub struct Accents {
    pub cpu: u32,
    pub ram: u32,
    pub gpu: u32,
    pub disk: u32,
    pub net: u32,
    pub fps: u32,
    pub audio: u32,
    /// The second direction of the two-way metrics: disk write and network
    /// upload. Two steps of one hue was the original rule and it failed in
    /// practice — real traffic is asymmetric enough that the weaker trace read
    /// as a rendering artefact rather than as data — so each gets a hue of its
    /// own, chosen from the gaps the seven metric accents leave. Gold and
    /// blue-green are complementary to cyan and orange, so a pair is separable
    /// at a glance, and neither is close enough to another accent to be misread
    /// inside a row that already names its metric.
    pub disk_w: u32,
    pub net_tx: u32,
}

/// Surface palette; selectable at runtime from settings.
pub struct Theme {
    pub name: &'static str,
    pub bg: u32,
    pub card: u32,
    pub card_hover: u32,
    /// Pressed. Recedes rather than lifts, in all three themes — a control that
    /// brightened on press would read as a second hover state.
    pub card_press: u32,
    /// 1 px card border. Elevation is carried by border + fill, never a shadow.
    pub line: u32,
    /// Chart gridlines. Quieter than `line`, which is structural.
    pub grid: u32,
    pub track: u32,
    pub input_bg: u32,
    pub input_border: u32,
    pub text: u32,
    /// The only ink below `text`. Units, ticks, times, prose, secondary figures,
    /// hints, status lines and the direction markers all take it, at 7.0:1 on
    /// `card`.
    ///
    /// There used to be a third step, `mute`, and it was reported as hard to read
    /// three separate times. The first cut of it measured 3.47:1 on the dark card
    /// and was raised to 4.72:1, which clears the 4.5:1 floor — and it was
    /// reported again anyway, as giving a headache in both themes. The floor was
    /// never the binding constraint at this size: 4.5:1 assumes 12–14 px at
    /// normal weight, and this was 11 px at 500 with a pixel of tracking, some of
    /// it drawn as hairline strokes with no mass at all. Rather than keep a
    /// palette entry whose every use had to be argued about, the step is gone and
    /// there are two text inks. See §2 rule 3 of ui-foundation.md.
    pub dim: u32,
    pub danger: u32,
    pub warn: u32,
    /// The healthy end of the severity ramp. No caller yet — nothing in the
    /// panel currently needs to say "this is fine" in colour, and inventing a
    /// use for it would be decoration.
    #[allow(dead_code)]
    pub good: u32,
    pub acc: Accents,
}

/// Accents for the two dark grounds. Adjacent-pair CVD ΔE (OKLab×100, Machado
/// 2009 at 1.0) is 14.2 against a target of 8; every one clears WCAG 4.5:1 on
/// `card`. GPU takes violet and Disk the cyan-teal, which is the swap that
/// separates the old RAM/GPU pair.
const ACC_DARK: Accents = Accents {
    cpu: rgb(74, 156, 246),
    ram: rgb(70, 190, 113),
    gpu: rgb(172, 137, 252),
    disk: rgb(44, 195, 210),
    net: rgb(241, 164, 39),
    fps: rgb(245, 116, 109),
    audio: rgb(235, 110, 201),
    disk_w: rgb(232, 195, 58),
    net_tx: rgb(63, 191, 160),
};

/// The same hue families pulled down for a white card: adjacent-pair ΔE 12.5,
/// all seven at or above 4.5:1 where the dark set managed 1.86–2.78:1.
const ACC_LIGHT: Accents = Accents {
    cpu: rgb(24, 115, 220),
    ram: rgb(26, 135, 49),
    gpu: rgb(139, 91, 212),
    disk: rgb(0, 129, 147),
    net: rgb(167, 102, 3),
    fps: rgb(211, 62, 71),
    audio: rgb(182, 57, 145),
    disk_w: rgb(133, 100, 0),
    net_tx: rgb(0, 122, 100),
};

pub const THEMES: [Theme; 3] = [
    Theme {
        name: "dark",
        bg: rgb(19, 20, 23),
        card: rgb(30, 32, 36),
        card_hover: rgb(40, 43, 49),
        card_press: rgb(25, 27, 31),
        line: rgb(44, 47, 53),
        grid: rgb(48, 50, 54),
        track: rgb(46, 50, 57),
        input_bg: rgb(14, 15, 18),
        input_border: rgb(62, 67, 76),
        text: rgb(232, 234, 237),
        dim: rgb(166, 170, 180),
        danger: rgb(242, 85, 90),
        warn: rgb(232, 163, 58),
        good: rgb(70, 190, 113),
        acc: ACC_DARK,
    },
    Theme {
        name: "black",
        bg: rgb(0, 0, 0),
        card: rgb(11, 12, 14),
        card_hover: rgb(23, 25, 29),
        card_press: rgb(6, 7, 8),
        line: rgb(28, 31, 36),
        grid: rgb(31, 33, 38),
        track: rgb(30, 33, 38),
        input_bg: rgb(6, 6, 7),
        input_border: rgb(52, 57, 65),
        text: rgb(237, 239, 242),
        dim: rgb(151, 155, 165),
        danger: rgb(242, 85, 90),
        warn: rgb(232, 163, 58),
        good: rgb(70, 190, 113),
        acc: ACC_DARK,
    },
    Theme {
        name: "light",
        bg: rgb(241, 242, 245),
        card: rgb(255, 255, 255),
        card_hover: rgb(237, 239, 243),
        card_press: rgb(228, 231, 236),
        line: rgb(227, 230, 235),
        grid: rgb(234, 234, 235),
        track: rgb(223, 227, 233),
        input_bg: rgb(255, 255, 255),
        input_border: rgb(195, 201, 210),
        text: rgb(20, 23, 28),
        dim: rgb(78, 82, 92),
        danger: rgb(194, 38, 46),
        warn: rgb(154, 95, 0),
        good: rgb(26, 135, 49),
        acc: ACC_LIGHT,
    },
];

use std::sync::atomic::{AtomicUsize, Ordering};

static THEME_IDX: AtomicUsize = AtomicUsize::new(0);

pub fn set_theme(idx: usize) {
    THEME_IDX.store(idx.min(THEMES.len() - 1), Ordering::Relaxed);
}

pub fn theme_idx() -> usize {
    THEME_IDX.load(Ordering::Relaxed)
}

/// Active theme; UI-thread reads this every paint.
pub fn t() -> &'static Theme {
    &THEMES[theme_idx()]
}

/// The active theme's accents.
pub fn acc() -> &'static Accents {
    &t().acc
}

/// An accent darkened toward black by `f` (0.0 black, 1.0 unchanged). Lets a
/// control derive its border and interior from whichever accent it carries,
/// so a second colour costs nothing but the constant.
pub fn shade(c: u32, f: f32) -> u32 {
    let ch = |sh: u32| ((((c >> sh) & 0xff) as f32) * f) as u8;
    rgb(ch(0), ch(8), ch(16))
}

/// `a` blended toward `b` by `f` (0.0 is `a`, 1.0 is `b`). The one colour
/// operation the whole design language needs: a lifted border is the card's
/// line mixed a step toward text, a second chart trace is its accent mixed
/// toward `dim`, and neither needs a constant of its own.
pub fn mix(a: u32, b: u32, f: f32) -> u32 {
    let f = f.clamp(0.0, 1.0);
    let ch = |sh: u32| {
        let (x, y) = (((a >> sh) & 0xff) as f32, ((b >> sh) & 0xff) as f32);
        (x + (y - x) * f).round().clamp(0.0, 255.0) as u8
    };
    rgb(ch(0), ch(8), ch(16))
}

/// Relative luminance, WCAG 2.x. Used only to choose ink, so the sRGB
/// linearisation is worth the three `powf`s — an eyeballed threshold on the
/// raw channels picks wrong on saturated mid-tones like `warn`.
fn luminance(c: u32) -> f32 {
    let lin = |sh: u32| {
        let v = (((c >> sh) & 0xff) as f32) / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(0) + 0.7152 * lin(8) + 0.0722 * lin(16)
}

/// Ink for text or a glyph sitting *on* `fill`. Replaces the hardcoded
/// near-black that assumed every filled control carried a light accent — it
/// does not, and `danger` on light needed white.
pub fn on(fill: u32) -> u32 {
    if luminance(fill) > 0.36 { rgb(15, 17, 20) } else { rgb(255, 255, 255) }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub fn make_font(height: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    unsafe {
        CreateFontW(
            -height, 0, 0, 0, weight, 0, 0, 0, 0, 0, 0, CLEARTYPE_QUALITY as u32, 0,
            face.as_ptr(),
        )
    }
}

/// A geometric pen with round caps and joins. `CreatePen` can express neither:
/// it always mitres, which on a 14 px chevron produces a visibly pointed,
/// off-centre corner, and leaves stroke ends square where every glyph in this
/// set wants them round.
fn round_pen(width: i32, color: u32) -> HPEN {
    let brush = LOGBRUSH { lbStyle: BS_SOLID, lbColor: color, lbHatch: 0 };
    unsafe {
        ExtCreatePen(
            (PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND) as u32,
            width.max(1) as u32,
            &brush,
            0,
            std::ptr::null(),
        )
    }
}

/// Letter-spacing for the two steps that carry it, in design px before scale.
/// `SetTextCharacterExtra` is the only tracking GDI offers and it is integral,
/// so these are already rounded to what the call can express.
pub const TRACK_LABEL: i32 = 1;
/// Tracking for the `micro` step: none. It was 1 px, on the reasoning that the
/// smallest type needs air between its letters. That holds for the ALL-CAPS
/// labels the step also draws, and is wrong for the lowercase words it mostly
/// draws — "used", "read", "write", "312 GB free of 1.0 TB" — where spacing the
/// letters stops the word reading as a single shape. Kept as a named constant so
/// the call sites do not have to change, and so this reasoning has somewhere to
/// live.
pub const TRACK_MICRO: i32 = 0;

/// Six type steps, plus two narrower cuts of the value step used only when a
/// value will not otherwise clear its metric name. Weight 600 is what does most
/// of the work: Segoe UI Semibold ships with Windows and the app previously
/// jumped straight from 400 to 700.
///
/// The sizes are one step up from the first cut of this spec, which had
/// `body` at 13 and `micro` at 10. Those were *smaller* than the 14 and 11 the
/// app shipped with, so adopting the scale quietly shrank every list row and
/// every unit in the product. Reported as "the font feels quite small", which
/// it was — a type scale is allowed to re-rank sizes but not to shrink the
/// baseline reading size by accident.
pub struct Fonts {
    /// 28/600 — one hero number per view.
    pub display: HFONT,
    /// 16/600 — view titles, watched app name.
    pub title: HFONT,
    /// 15/600 — metric values, list values.
    pub value: HFONT,
    /// 14/400 — rows, settings, prose. The baseline reading size.
    pub body: HFONT,
    /// 12/600 +track — identity: metric names, headings, chips.
    pub label: HFONT,
    /// 11/500 +track — ticks, times, units, hosts.
    pub micro: HFONT,
    /// 14/600 and 13/600 — the value step's fit ladder. See §2 of the spec.
    pub value_sm: HFONT,
    pub value_xs: HFONT,
}

impl Fonts {
    pub fn new(scale: f32) -> Self {
        let s = |v: f32| (v * scale) as i32;
        Fonts {
            display: make_font(s(28.0), 600),
            title: make_font(s(16.0), 600),
            value: make_font(s(15.0), 600),
            body: make_font(s(14.0), 400),
            label: make_font(s(12.0), 600),
            micro: make_font(s(11.0), 600),
            value_sm: make_font(s(14.0), 600),
            value_xs: make_font(s(13.0), 600),
        }
    }

    fn all(&self) -> [HFONT; 8] {
        [
            self.display, self.title, self.value, self.body, self.label, self.micro,
            self.value_sm, self.value_xs,
        ]
    }

    /// Fonts to try, largest first, when fitting a value into a width. Stops at
    /// 13: below that a value stops being the loudest thing in its row, which
    /// is the whole reason it is set at 15 to begin with.
    pub fn fit_stack(&self) -> [HFONT; 3] {
        [self.value, self.value_sm, self.value_xs]
    }

    /// Release the handles. Only called when replacing the set — GDI font
    /// handles are a limited resource, so rebuilding at a new size must not
    /// leak the old one.
    pub fn destroy(&self) {
        for f in self.all() {
            unsafe { DeleteObject(f as HGDIOBJ) };
        }
    }
}

/// Direct access to the back buffer's pixels.
///
/// This is the one new primitive the whole premium half of the design rests on.
/// GDI has no antialiasing: `Polyline` at 1 px is a staircase, and a chart trace
/// is the thing this product exists to draw. The alternative was GDI+ — 1.7 MB
/// mapped and roughly ten times slower per path, against an 840 KB binary — to
/// draw seven sparklines. Writing coverage into the buffer costs one changed
/// call and about 1–2 KB of code.
#[derive(Copy, Clone)]
pub struct Surface {
    bits: *mut u32,
    w: i32,
    h: i32,
}

impl Surface {
    /// Drain GDI's batch before touching pixels.
    ///
    /// GDI queues drawing and flushes it lazily. A direct write that races a
    /// queued `FillRect` over the same pixels is simply lost when the batch
    /// lands, which shows up as a trace that is there on some frames and not
    /// others. Every entry point that writes pixels calls this first.
    fn flush(&self) {
        unsafe { GdiFlush() };
    }

    /// Blend `color` over the pixel at `(x, y)` with coverage `a` in 0..=1.
    ///
    /// The DIB is top-down 32-bit `BI_RGB`, so a pixel is `0x00RRGGBB` while a
    /// `COLORREF` is `0x00BBGGRR` — the two are byte-swapped and mixing them up
    /// silently swaps every red and blue on the chart.
    #[inline]
    fn blend(&self, x: i32, y: i32, color: u32, a: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || a <= 0.0 {
            return;
        }
        let a = a.min(1.0);
        let (sr, sg, sb) =
            ((color & 0xff) as f32, ((color >> 8) & 0xff) as f32, ((color >> 16) & 0xff) as f32);
        unsafe {
            let p = self.bits.offset((y * self.w + x) as isize);
            let d = *p;
            let (dr, dg, db) =
                (((d >> 16) & 0xff) as f32, ((d >> 8) & 0xff) as f32, (d & 0xff) as f32);
            let m = |s: f32, dv: f32| ((dv + (s - dv) * a).round().clamp(0.0, 255.0) as u32) & 0xff;
            *p = (m(sr, dr) << 16) | (m(sg, dg) << 8) | m(sb, db);
        }
    }
}

/// The off-screen surface every paint draws into.
///
/// Deliberately **cached across paints** rather than rebuilt per frame. At
/// 336 px wide by a window's height this DIB is close to a megabyte, and
/// rebuilding it per paint meant allocating and freeing that much on every
/// hover change — and once the hover cross-fade landed, up to sixty times a
/// second. Churn on that scale is exactly what a resource monitor should not be
/// doing. It is now rebuilt only when the window size actually changes.
pub struct BackBuffer {
    pub dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
}

impl BackBuffer {
    /// `reference` is only used to create the compatible DC and the DIB; the
    /// buffer does not retain it, so it may be a paint DC that expires.
    pub fn new(reference: HDC, w: i32, h: i32) -> Self {
        unsafe {
            let dc = CreateCompatibleDC(reference);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w.max(1);
            // Negative height is a top-down DIB, so row 0 is the top and the
            // pixel index is the obvious `y * w + x`.
            info.bmiHeader.biHeight = -h.max(1);
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB as u32;
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            let old = SelectObject(dc, bmp as HGDIOBJ);
            BackBuffer { dc, bmp, old, bits: bits as *mut u32, w: w.max(1), h: h.max(1) }
        }
    }

    /// The size this buffer was built for, so a caller can tell whether the
    /// cached one still fits.
    pub fn size(&self) -> (i32, i32) {
        (self.w, self.h)
    }

    /// The pixel surface behind this buffer.
    ///
    /// `GdiFlush` first: GDI batches drawing, and reading or writing the bits
    /// while calls are still queued composites against a stale buffer. This is
    /// the failure mode that makes DIB rendering look intermittently correct.
    pub fn surface(&self) -> Option<Surface> {
        if self.bits.is_null() {
            return None;
        }
        unsafe { GdiFlush() };
        Some(Surface { bits: self.bits, w: self.w, h: self.h })
    }

    pub fn present(&self, target: HDC) {
        unsafe {
            BitBlt(target, 0, 0, self.w, self.h, self.dc, 0, 0, SRCCOPY);
        }
    }
}

impl Drop for BackBuffer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp as HGDIOBJ);
            DeleteDC(self.dc);
        }
    }
}

pub fn fill(dc: HDC, r: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(dc, r, brush);
        DeleteObject(brush as HGDIOBJ);
    }
}

/// Draw `s` with `extra` px of tracking. Every text call routes through here so
/// that `SetTextCharacterExtra` is *always* set explicitly: it is DC state, and
/// a tracked label leaving it set would silently widen the next untracked run.
pub fn text_t(dc: HDC, x: i32, y: i32, font: HFONT, extra: i32, color: u32, s: &str) {
    let w: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        SelectObject(dc, font as HGDIOBJ);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        SetTextCharacterExtra(dc, extra);
        TextOutW(dc, x, y, w.as_ptr(), w.len() as i32);
        SetTextCharacterExtra(dc, 0);
    }
}

pub fn text(dc: HDC, x: i32, y: i32, font: HFONT, color: u32, s: &str) {
    text_t(dc, x, y, font, 0, color, s);
}

/// Width of `s` including `extra` px of tracking. `GetTextExtentPoint32W` does
/// not know about `SetTextCharacterExtra`, so the tracking is added back per
/// character — otherwise every right-aligned tracked run lands short.
pub fn text_width_t(dc: HDC, font: HFONT, extra: i32, s: &str) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut size = SIZE { cx: 0, cy: 0 };
    unsafe {
        SelectObject(dc, font as HGDIOBJ);
        GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32, &mut size);
    }
    size.cx + extra * w.len() as i32
}

pub fn text_width(dc: HDC, font: HFONT, s: &str) -> i32 {
    text_width_t(dc, font, 0, s)
}

/// (ascent, descent, internal_leading) for `font`, in pixels. Runs in
/// different fonts only line up when placed off a shared baseline, and the
/// font *cell* — which is what a text-extent measurement returns — is not
/// where the ink sits inside it.
pub fn text_metrics(dc: HDC, font: HFONT) -> (i32, i32, i32) {
    let mut tm: TEXTMETRICW = unsafe { std::mem::zeroed() };
    unsafe {
        SelectObject(dc, font as HGDIOBJ);
        GetTextMetricsW(dc, &mut tm);
    }
    (tm.tmAscent, tm.tmDescent, tm.tmInternalLeading)
}

pub fn text_right(dc: HDC, right: i32, y: i32, font: HFONT, color: u32, s: &str) {
    text_right_t(dc, right, y, font, 0, color, s);
}

pub fn text_right_t(dc: HDC, right: i32, y: i32, font: HFONT, extra: i32, color: u32, s: &str) {
    let cx = text_width_t(dc, font, extra, s);
    text_t(dc, right - cx, y, font, extra, color, s);
}

/// Split `s` into lines that each fit inside `max_w` when drawn in `font`.
/// Breaks on spaces; a single word longer than `max_w` is split mid-word so a
/// long path or hash can never overflow the panel.
pub fn wrap_lines(dc: HDC, font: HFONT, max_w: i32, s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if max_w <= 0 {
        return out;
    }
    // Widths accumulate word by word rather than re-measuring the growing
    // line: GetTextExtentPoint32W ignores kerning, so the sum of the parts is
    // the width of the whole — and re-measuring made wrapping O(line²) per
    // paint, which long agent details turned into visible cost.
    let space_w = text_width(dc, font, " ");
    for para in s.split('\n') {
        let mut line = String::new();
        let mut line_w = 0;
        for word in para.split_whitespace() {
            let word_w = text_width(dc, font, word);
            let joined_w = if line.is_empty() { word_w } else { line_w + space_w + word_w };
            if joined_w <= max_w {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
                line_w = joined_w;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if word_w <= max_w {
                line = word.to_string();
                line_w = word_w;
                continue;
            }
            // The word alone still does not fit — break it by character.
            let mut chunk = String::new();
            let mut chunk_w = 0;
            for ch in word.chars() {
                let ch_w = text_width(dc, font, ch.encode_utf8(&mut [0u8; 4]));
                if !chunk.is_empty() && chunk_w + ch_w > max_w {
                    out.push(std::mem::take(&mut chunk));
                    chunk_w = 0;
                }
                chunk.push(ch);
                chunk_w += ch_w;
            }
            line = chunk;
            line_w = chunk_w;
        }
        out.push(line);
    }
    out
}

/// Draw `s` wrapped inside `max_w`, one line every `line_h`. Returns the total
/// height drawn.
pub fn text_wrap(
    dc: HDC,
    x: i32,
    y: i32,
    max_w: i32,
    line_h: i32,
    font: HFONT,
    color: u32,
    s: &str,
) -> i32 {
    let lines = wrap_lines(dc, font, max_w, s);
    for (i, line) in lines.iter().enumerate() {
        text(dc, x, y + i as i32 * line_h, font, color, line);
    }
    lines.len() as i32 * line_h
}

/// Chevron drawn as two strokes from an exact center point, pointing `dir`
/// ("left" or "right"). Font glyphs like `‹` centre their *cell*, not their
/// ink, so a glyph never lines up with adjacent text; this always does.
pub fn chevron(dc: HDC, cx: i32, cy: i32, size: i32, thickness: i32, color: u32, left: bool) {
    let half = size.max(2) / 2;
    let dx = if left { half } else { -half };
    let pts = [
        POINT { x: cx + dx, y: cy - half },
        POINT { x: cx - dx, y: cy },
        POINT { x: cx + dx, y: cy + half },
    ];
    unsafe {
        let pen = round_pen(thickness.max(1), color);
        let old = SelectObject(dc, pen as HGDIOBJ);
        Polyline(dc, pts.as_ptr(), pts.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen as HGDIOBJ);
    }
}

/// A card: rounded fill plus a 1 px border, drawn from one region so the fill
/// and the frame agree about where the corners are.
///
/// Elevation in this product is carried by border + fill and never by a drop
/// shadow — a shadow in GDI means either a blurred DIB composite every frame or
/// a hard offset rectangle that looks like 2003. The 1 px `line` at 1.22:1
/// against `card` reads as a lifted edge for the cost of one `FrameRgn`.
pub fn card(dc: HDC, r: &RECT, fill_c: u32, line_c: u32, radius: i32) {
    unsafe {
        let d = (radius.max(0) * 2).max(1);
        // Region bounds are exclusive, so +1 to land on the caller's rect.
        let rgn = CreateRoundRectRgn(r.left, r.top, r.right + 1, r.bottom + 1, d, d);
        let body = CreateSolidBrush(fill_c);
        FillRgn(dc, rgn, body);
        let edge = CreateSolidBrush(line_c);
        FrameRgn(dc, rgn, edge, 1, 1);
        DeleteObject(edge as HGDIOBJ);
        DeleteObject(body as HGDIOBJ);
        DeleteObject(rgn as HGDIOBJ);
    }
}

/// Chrome glyphs: navigation, window controls, and the destructive one.
///
/// These retire every word-as-button and every font glyph the panel used to
/// lean on — `"settings"`, `"pin"`, `"top"`, `"pause"`, `"×"`, `"›"`, `"▾"`,
/// `"◆"`, `"◇"`. A font glyph centres its *cell*, not its ink, so it never
/// lines up with adjacent text; geometry always does.
#[derive(Copy, Clone, PartialEq)]
pub enum Icon {
    Gear,
    /// Close. Uses `dim`, `text` on hover — **never** `danger`, because
    /// dismissing a window is not destructive.
    Close,
    /// Terminate. The destructive one, and the whole point of it is that it is
    /// no longer the same mark as `Close`.
    Trash,
    Pin,
    /// `Pin` rotated, for the pinned state.
    Unpin,
    OnTop,
    Search,
    Pause,
    Play,
    Check,
    Grip,
    /// Agent/status markers: filled for live, stroked for finished.
    DotFilled,
    DotHollow,
}

/// Chrome glyph on the same 16-unit grid as [`metric_icon`]. `surface` is the
/// colour behind the glyph, needed only by `Gear` to punch its hub.
pub fn icon(dc: HDC, cx: i32, cy: i32, size: i32, thickness: i32, color: u32, surface: u32, g: Icon) {
    let u = size.max(8) as f32 / 16.0;
    let p = |x: f32, y: f32| POINT { x: cx + (x * u).round() as i32, y: cy + (y * u).round() as i32 };
    let un = |v: f32| (v * u).round() as i32;
    let th = thickness.max(1);
    unsafe {
        let pen = round_pen(th, color);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        let hollow = GetStockObject(NULL_BRUSH) as HGDIOBJ;
        let solid = CreateSolidBrush(color);
        let line = |pts: &[POINT]| Polyline(dc, pts.as_ptr(), pts.len() as i32);
        let filled = |pts: &[POINT]| {
            let ob = SelectObject(dc, solid as HGDIOBJ);
            Polygon(dc, pts.as_ptr(), pts.len() as i32);
            SelectObject(dc, ob);
        };

        match g {
            Icon::Gear => {
                // Six teeth, not eight: at 14 px an 8-tooth gear turns to mush.
                let (ro, ri, hub) = (7.4f32, 5.4f32, 2.4f32);
                let t_n = 6usize;
                let pitch = std::f32::consts::TAU / t_n as f32;
                let (half, gap) = (pitch * 0.21, pitch * 0.06);
                let mut pts: Vec<POINT> = Vec::with_capacity(t_n * 4);
                for k in 0..t_n {
                    let a = k as f32 * pitch - std::f32::consts::FRAC_PI_2;
                    for (r, ang) in [
                        (ro, a - half),
                        (ro, a + half),
                        (ri, a + half + gap),
                        (ri, a + pitch - half - gap),
                    ] {
                        pts.push(p(ang.cos() * r, ang.sin() * r));
                    }
                }
                filled(&pts);
                // Punch the hub in the surface colour: the equivalent of
                // CombineRgn(RGN_DIFF) for two calls instead of four.
                let hb = CreateSolidBrush(surface);
                let hp = CreatePen(PS_SOLID as i32, 1, surface);
                let ob = SelectObject(dc, hb as HGDIOBJ);
                let op = SelectObject(dc, hp as HGDIOBJ);
                let (a, b) = (p(-hub, -hub), p(hub, hub));
                Ellipse(dc, a.x, a.y, b.x + 1, b.y + 1);
                SelectObject(dc, ob);
                SelectObject(dc, op);
                DeleteObject(hb as HGDIOBJ);
                DeleteObject(hp as HGDIOBJ);
            }
            Icon::Close => {
                line(&[p(-4.5, -4.5), p(4.5, 4.5)]);
                line(&[p(4.5, -4.5), p(-4.5, 4.5)]);
            }
            Icon::Trash => {
                line(&[p(-5.0, -3.5), p(5.0, -3.5)]);
                line(&[p(-2.0, -3.5), p(-2.0, -5.0), p(2.0, -5.0), p(2.0, -3.5)]);
                line(&[p(-4.0, -3.5), p(-3.2, 5.5), p(3.2, 5.5), p(4.0, -3.5)]);
                line(&[p(-1.4, -1.0), p(-1.4, 3.5)]);
                line(&[p(1.4, -1.0), p(1.4, 3.5)]);
            }
            Icon::Pin | Icon::Unpin => {
                // Unpin is the same pushpin rotated 35°, done by rotating the
                // points rather than SetWorldTransform: cheaper, and there is
                // no DC state to restore afterwards.
                let a = if g == Icon::Unpin { 35f32.to_radians() } else { 0.0 };
                let (sa, ca) = (a.sin(), a.cos());
                let rp = |x: f32, y: f32| p(x * ca - y * sa, x * sa + y * ca);
                let hr = 3.0;
                let ob = SelectObject(dc, solid as HGDIOBJ);
                let c = rp(0.0, -3.0);
                Ellipse(dc, c.x - un(hr), c.y - un(hr), c.x + un(hr) + 1, c.y + un(hr) + 1);
                SelectObject(dc, ob);
                line(&[rp(0.0, 0.0), rp(0.0, 6.0)]);
                line(&[rp(-4.0, -0.5), rp(4.0, -0.5)]);
            }
            Icon::OnTop => {
                // A ceiling, and something being driven up against it. The
                // previous glyph was a window outline with a chevron over it,
                // which at 13 px was indistinguishable from the pushpin beside
                // it — and the two mean different things.
                let bar = RECT {
                    left: p(-6.5, -6.5).x,
                    top: p(-6.5, -6.5).y,
                    right: p(6.5, -6.5).x + 1,
                    bottom: p(-6.5, -6.5).y + un(1.8).max(2),
                };
                fill(dc, &bar, color);
                line(&[p(0.0, 6.5), p(0.0, -2.6)]);
                line(&[p(-4.0, 1.2), p(0.0, -3.4), p(4.0, 1.2)]);
            }
            Icon::Search => {
                let (a, b) = (p(-5.0, -5.0), p(3.0, 3.0));
                let ob = SelectObject(dc, hollow);
                Ellipse(dc, a.x, a.y, b.x + 1, b.y + 1);
                SelectObject(dc, ob);
                line(&[p(2.0, 2.0), p(5.5, 5.5)]);
            }
            Icon::Pause => {
                for x in [-3.0f32, 3.0] {
                    let (a, b) = (p(x - 1.25, -5.0), p(x + 1.25, 5.0));
                    let r = RECT { left: a.x, top: a.y, right: b.x + 1, bottom: b.y + 1 };
                    fill(dc, &r, color);
                }
            }
            Icon::Play => filled(&[p(-3.5, -5.0), p(5.0, 0.0), p(-3.5, 5.0)]),
            Icon::Check => {
                // A tick, not the smaller filled square the panel used — that
                // is a radio button's idiom, not a checkbox's.
                line(&[p(-3.5, 0.3), p(-1.0, 3.0), p(4.0, -3.2)]);
            }
            Icon::Grip => {
                for y in [-2.0f32, 2.0] {
                    let (a, b) = (p(-5.0, y - 0.5), p(5.0, y + 0.5));
                    let r = RECT { left: a.x, top: a.y, right: b.x + 1, bottom: b.y + 1 };
                    fill(dc, &r, color);
                }
            }
            Icon::DotFilled | Icon::DotHollow => {
                let r = 3.4f32;
                let (a, b) = (p(-r, -r), p(r, r));
                let ob = SelectObject(
                    dc,
                    if g == Icon::DotFilled { solid as HGDIOBJ } else { hollow },
                );
                Ellipse(dc, a.x, a.y, b.x + 1, b.y + 1);
                SelectObject(dc, ob);
            }
        }

        DeleteObject(solid as HGDIOBJ);
        SelectObject(dc, old_pen);
        DeleteObject(pen as HGDIOBJ);
    }
}

/// Which per-metric glyph to draw. These are the second identity channel that
/// makes the accent set's all-pairs colour gap safe: seven categorical hues
/// cannot all be distinguishable, so no accent in this app ever appears without
/// both a text label and its own shape.
#[derive(Copy, Clone, PartialEq)]
pub enum Glyph {
    Cpu,
    Ram,
    Gpu,
    Disk,
    /// A globe. Deliberately *not* the opposed-arrow pair — those became the
    /// row's direction markers, see [`arrow`].
    Net,
    Fps,
    Audio,
}

/// Per-metric glyph on a 16-unit grid with the origin at `(cx, cy)`, exactly as
/// specified in §5. There is no SVG loader here and there does not need to be:
/// every glyph is two to eight calls, which against the ~200 `FillRect` and
/// `TextOut` calls a panel paint already issues is noise.
pub fn metric_icon(dc: HDC, cx: i32, cy: i32, size: i32, thickness: i32, color: u32, g: Glyph) {
    let u = size.max(8) as f32 / 16.0;
    let p = |x: f32, y: f32| POINT { x: cx + (x * u).round() as i32, y: cy + (y * u).round() as i32 };
    unsafe {
        let pen = round_pen(thickness.max(1), color);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        let hollow = SelectObject(dc, GetStockObject(NULL_BRUSH) as HGDIOBJ);
        let solid = CreateSolidBrush(color);

        // Rectangle/Ellipse take an exclusive bottom-right, so every box is
        // widened by one pixel to land on the coordinate the spec names.
        let ell = |x0: f32, y0: f32, x1: f32, y1: f32| {
            let (a, b) = (p(x0, y0), p(x1, y1));
            Ellipse(dc, a.x, a.y, b.x + 1, b.y + 1);
        };
        let rect = |x0: f32, y0: f32, x1: f32, y1: f32| {
            let (a, b) = (p(x0, y0), p(x1, y1));
            Rectangle(dc, a.x, a.y, b.x + 1, b.y + 1);
        };
        let line = |pts: &[POINT]| Polyline(dc, pts.as_ptr(), pts.len() as i32);

        match g {
            Glyph::Cpu => {
                rect(-4.0, -4.0, 4.0, 4.0);
                rect(-1.6, -1.6, 1.6, 1.6);
                for i in [-2.0f32, 0.0, 2.0] {
                    line(&[p(i, -4.0), p(i, -6.5)]);
                    line(&[p(i, 4.0), p(i, 6.5)]);
                    line(&[p(-4.0, i), p(-6.5, i)]);
                    line(&[p(4.0, i), p(6.5, i)]);
                }
            }
            Glyph::Ram => {
                rect(-6.5, -3.5, 6.5, 3.0);
                line(&[p(-1.0, 3.0), p(-1.0, 1.5), p(1.0, 1.5), p(1.0, 3.0)]);
                for x in [-5.0f32, -2.5, 0.0, 2.5, 5.0] {
                    line(&[p(x, 3.0), p(x, 5.0)]);
                }
            }
            Glyph::Gpu => {
                rect(-6.5, -4.0, 6.5, 4.0);
                ell(-2.8, -2.8, 2.8, 2.8);
                for k in 0..3 {
                    let a = k as f32 * std::f32::consts::TAU / 3.0 - std::f32::consts::FRAC_PI_2;
                    line(&[
                        p(a.cos() * 1.0, a.sin() * 1.0),
                        p(a.cos() * 2.6, a.sin() * 2.6),
                    ]);
                }
            }
            Glyph::Disk => {
                ell(-6.0, -4.5, 6.0, -1.5);
                ell(-6.0, 1.5, 6.0, 4.5);
                line(&[p(-6.0, -3.0), p(-6.0, 3.0)]);
                line(&[p(6.0, -3.0), p(6.0, 3.0)]);
            }
            Glyph::Net => {
                ell(-6.3, -6.3, 6.3, 6.3);
                ell(-2.85, -6.3, 2.85, 6.3);
                line(&[p(-6.3, 0.0), p(6.3, 0.0)]);
            }
            Glyph::Fps => {
                rect(-6.5, -4.5, 6.5, 4.5);
                let bolt = [
                    p(0.5, -3.0),
                    p(-2.0, 0.3),
                    p(-0.3, 0.3),
                    p(-0.8, 3.0),
                    p(2.0, -0.3),
                    p(0.2, -0.3),
                ];
                let ob = SelectObject(dc, solid as HGDIOBJ);
                Polygon(dc, bolt.as_ptr(), bolt.len() as i32);
                SelectObject(dc, ob);
            }
            Glyph::Audio => {
                let cone = [
                    p(-5.0, -2.0),
                    p(-2.0, -2.0),
                    p(1.0, -5.0),
                    p(1.0, 5.0),
                    p(-2.0, 2.0),
                    p(-5.0, 2.0),
                ];
                let ob = SelectObject(dc, solid as HGDIOBJ);
                Polygon(dc, cone.as_ptr(), cone.len() as i32);
                SelectObject(dc, ob);
                // Two waves: arcs spanning ±50° about the +x axis. GDI's Arc
                // runs counter-clockwise from (x1,y1) to (x2,y2), so the start
                // point is the lower one.
                for r in [3.5f32, 6.0] {
                    let dy = (50f32.to_radians()).sin() * r;
                    let dx = (50f32.to_radians()).cos() * r;
                    let (a, b) = (p(-r, -r), p(r, r));
                    let (s, e) = (p(dx, dy), p(dx, -dy));
                    Arc(dc, a.x, a.y, b.x + 1, b.y + 1, s.x, s.y, e.x, e.y);
                }
            }
        }

        DeleteObject(solid as HGDIOBJ);
        SelectObject(dc, hollow);
        SelectObject(dc, old_pen);
        DeleteObject(pen as HGDIOBJ);
    }
}

/// Direction marker: a stem and a head, pointing down or up. Two `Polyline`
/// runs on the same round-capped pen as the chevrons.
///
/// This exists because the *words* do not fit. `NETWORK` is the longest metric
/// name in the product and a rate like `12.4 MB/s` is the widest value, and the
/// two share one band about 144 px wide; `down` costs ~25 px of that where the
/// glyph costs 6. It is also why the Network metric glyph is a globe rather
/// than the opposed-arrow pair first specified for it — identity and direction
/// are separate channels and must not wear the same shape.
pub fn arrow(dc: HDC, cx: i32, cy: i32, size: i32, thickness: i32, color: u32, down: bool) {
    let h = size.max(6) as f32 / 2.0;
    let sgn = if down { 1.0 } else { -1.0 };
    let px = |fx: f32, fy: f32| POINT {
        x: cx + (fx * h).round() as i32,
        y: cy + (sgn * fy * h).round() as i32,
    };
    // Fractions of the half-size, matching the 16-unit grid in the spec:
    // stem (0,-6.6)→(0,4.4), head (-3.8,0.9)→(0,5.8)→(3.8,0.9), over half = 8.
    let stem = [px(0.0, -0.825), px(0.0, 0.55)];
    let head = [px(-0.475, 0.1125), px(0.0, 0.725), px(0.475, 0.1125)];
    unsafe {
        let pen = round_pen(thickness.max(1), color);
        let old = SelectObject(dc, pen as HGDIOBJ);
        Polyline(dc, stem.as_ptr(), stem.len() as i32);
        Polyline(dc, head.as_ptr(), head.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen as HGDIOBJ);
    }
}

/// The `y` to hand a text call so the run's *ink* is vertically centred on
/// `mid`. Text calls take the top of the font cell, and the cell is taller than
/// the ink by its internal leading — centring the cell puts the glyphs low, and
/// visibly so once two steps share a line.
pub fn centre_y(dc: HDC, font: HFONT, mid: i32) -> i32 {
    let (asc, desc, ilead) = text_metrics(dc, font);
    mid - ilead - (asc + desc - ilead) / 2
}

/// Like [`centre_y`], but centres the **cap box** rather than the full font
/// cell. An all-caps run uses none of its descent, so centring ascent+descent
/// sits the letters visibly low next to a geometrically centred icon.
pub fn centre_y_caps(dc: HDC, font: HFONT, mid: i32) -> i32 {
    let (asc, _, ilead) = text_metrics(dc, font);
    let cap = asc - ilead;
    // baseline = mid + cap/2, and the call takes the top of the cell.
    mid + cap / 2 - asc
}

/// The `y` so the run's descender lands `gap` px above `bottom`.
pub fn bottom_y(dc: HDC, font: HFONT, bottom: i32, gap: i32) -> i32 {
    let (asc, desc, _) = text_metrics(dc, font);
    bottom - gap - (asc + desc)
}

/// Small filled triangle nested into a window's bottom-right corner, the
/// classic resize-grip affordance — legs along the bottom and right edges,
/// hypotenuse cutting across the corner. Callers gate this on hover, so it
/// only appears while the corner is actually reachable.
pub fn resize_grip(dc: HDC, right: i32, bottom: i32, size: i32, color: u32) {
    let pts = [
        POINT { x: right - size, y: bottom },
        POINT { x: right, y: bottom - size },
        POINT { x: right, y: bottom },
    ];
    unsafe {
        let brush = CreateSolidBrush(color);
        let pen = CreatePen(PS_SOLID as i32, 1, color);
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        Polygon(dc, pts.as_ptr(), pts.len() as i32);
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        DeleteObject(brush as HGDIOBJ);
        DeleteObject(pen as HGDIOBJ);
    }
}

/// Accordion disclosure marker: points right when closed, down when open.
/// Deliberately a different shape from `chevron` — the back control points
/// left, so a row that expands in place must not wear the same arrow as one
/// that navigates away.
pub fn disclosure(dc: HDC, cx: i32, cy: i32, size: i32, thickness: i32, color: u32, open: bool) {
    let half = size.max(2) / 2;
    let pts = if open {
        // ▾ — the row opens downward, so the marker does too.
        [
            POINT { x: cx - half, y: cy - half / 2 },
            POINT { x: cx, y: cy + half / 2 },
            POINT { x: cx + half, y: cy - half / 2 },
        ]
    } else {
        // ▸
        [
            POINT { x: cx - half / 2, y: cy - half },
            POINT { x: cx + half / 2, y: cy },
            POINT { x: cx - half / 2, y: cy + half },
        ]
    };
    unsafe {
        let pen = round_pen(thickness.max(1), color);
        let old = SelectObject(dc, pen as HGDIOBJ);
        Polyline(dc, pts.as_ptr(), pts.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen as HGDIOBJ);
    }
}

/// Draw `s` starting at (x, y) without crossing `right`: tries each font in
/// `fonts` (largest first), then falls back to truncating with an ellipsis.
pub fn text_fit(dc: HDC, x: i32, y: i32, right: i32, fonts: &[HFONT], color: u32, s: &str) {
    let max_w = right - x;
    for &f in fonts {
        if text_width(dc, f, s) <= max_w {
            text(dc, x, y, f, color, s);
            return;
        }
    }
    let f = *fonts.last().expect("text_fit needs at least one font");
    let mut owned: String = s.to_string();
    while owned.chars().count() > 1 && text_width(dc, f, &format!("{}…", owned)) > max_w {
        owned.pop();
    }
    text(dc, x, y, f, color, &format!("{}…", owned));
}

/// Antialiased polyline of `width` px with round joins and caps.
///
/// Coverage is accumulated into a scratch buffer with `max` before any pixel is
/// touched, so a join does not blend twice and show as a dark knot. Each
/// segment only visits its own bounding box, which keeps a 60-sample trace at a
/// few thousand pixel tests rather than a pass over the whole plot.
pub fn aa_polyline(surf: &Surface, clip: &RECT, pts: &[(f32, f32)], width: f32, color: u32) {
    let (cw, ch) = (clip.right - clip.left, clip.bottom - clip.top);
    if pts.len() < 2 || cw <= 0 || ch <= 0 {
        return;
    }
    surf.flush();
    let hw = (width.max(0.5)) / 2.0;
    let mut cov = vec![0f32; (cw * ch) as usize];

    for seg in pts.windows(2) {
        let (ax, ay, bx, by) = (seg[0].0, seg[0].1, seg[1].0, seg[1].1);
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = dx * dx + dy * dy;
        let pad = hw + 1.0;
        let x0 = (ax.min(bx) - pad).floor() as i32;
        let x1 = (ax.max(bx) + pad).ceil() as i32;
        let y0 = (ay.min(by) - pad).floor() as i32;
        let y1 = (ay.max(by) + pad).ceil() as i32;
        for py in y0.max(clip.top)..y1.min(clip.bottom) {
            for px in x0.max(clip.left)..x1.min(clip.right) {
                let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                // Distance to the segment: a capsule, which is what gives round
                // caps and joins without any special-casing at the vertices.
                let t = if len2 > 0.0 {
                    (((cx - ax) * dx + (cy - ay) * dy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (qx, qy) = (ax + dx * t, ay + dy * t);
                let d = ((cx - qx).powi(2) + (cy - qy).powi(2)).sqrt();
                let a = (hw + 0.5 - d).clamp(0.0, 1.0);
                if a > 0.0 {
                    let i = ((py - clip.top) * cw + (px - clip.left)) as usize;
                    if a > cov[i] {
                        cov[i] = a;
                    }
                }
            }
        }
    }

    for iy in 0..ch {
        for ix in 0..cw {
            let a = cov[(iy * cw + ix) as usize];
            if a > 0.0 {
                surf.blend(clip.left + ix, clip.top + iy, color, a);
            }
        }
    }
}

/// Antialiased filled disc — the chart's head dot.
pub fn aa_disc(surf: &Surface, clip: &RECT, cx: f32, cy: f32, r: f32, color: u32) {
    surf.flush();
    let x0 = ((cx - r - 1.0).floor() as i32).max(clip.left);
    let x1 = ((cx + r + 1.0).ceil() as i32).min(clip.right);
    let y0 = ((cy - r - 1.0).floor() as i32).max(clip.top);
    let y1 = ((cy + r + 1.0).ceil() as i32).min(clip.bottom);
    for py in y0..y1 {
        for px in x0..x1 {
            let d = ((px as f32 + 0.5 - cx).powi(2) + (py as f32 + 0.5 - cy).powi(2)).sqrt();
            surf.blend(px, py, color, (r + 0.5 - d).clamp(0.0, 1.0));
        }
    }
}

/// How a chart's own labels — peak, ceiling — should format a value. Without
/// this a rate's peak label printed the raw byte count.
#[derive(Copy, Clone, PartialEq)]
pub enum Units {
    Percent,
    /// Bytes per second, formatted like every other rate in the product.
    Rate,
    /// A bare count, e.g. frames per second.
    Count,
    /// Frames per second. Distinct from `Count` because the unit belongs on the
    /// number: charts.md gives `144 fps` as the ceiling label, not `144`.
    Fps,
}

impl Units {
    pub fn fmt(self, v: f32) -> String {
        match self {
            Units::Percent => format!("{:.0}%", v),
            Units::Rate => crate::util::format_rate(v.max(0.0) as u64),
            Units::Count => format!("{:.0}", v),
            Units::Fps => format!("{:.0} fps", v),
        }
    }
}

/// Where a chart is drawn and how much of it there is room for.
#[derive(Copy, Clone, PartialEq)]
pub enum ChartSize {
    /// The 120×32 sparkline in a metric row: trace, wash and baseline only.
    Row,
    /// The full-width plot in a drill-down: gridlines and a peak marker too.
    Hero,
}

/// The chart. Draw order is plate, un-sampled shading, gridlines, wash,
/// baseline, trace, head dot — the wash under the line, the baseline under
/// both, and the head dot last so it is never occluded.
///
/// `ceiling` is supplied by the caller and is the whole reason this looks
/// different from what it replaced: it used to be `ring.max()`, which rescaled
/// the axis on nearly every tick.
/// Map a cursor x onto a sample slot, or `None` if it is outside the plot or
/// falls on a slot the window has not filled yet.
pub fn chart_hit(r: &RECT, cap: usize, held: usize, hx: i32) -> Option<usize> {
    let w = r.right - r.left;
    if w <= 1 || cap < 2 || held == 0 || hx < r.left || hx >= r.right {
        return None;
    }
    let start = cap - held;
    let slot =
        (((hx - r.left) as f32 * (cap - 1) as f32) / (w - 1) as f32).round() as i64;
    let i = slot - start as i64;
    if i < 0 || i as usize >= held { None } else { Some(i as usize) }
}

/// The second half of a two-way metric: download against upload, read against
/// write. Drawn below the midline, sharing the primary's ceiling.
///
/// `label_hi` and `label_lo` are permanent direct labels — never colour alone.
/// They name the halves so the pair does not depend on a reader noticing that
/// one trace is a shade weaker than the other.
pub struct Mirror<'a> {
    pub ring: &'a Ring,
    pub label_hi: &'a str,
    pub label_lo: &'a str,
    /// This half's own colour, not a shade of the primary's.
    pub color: u32,
    /// This half's own ceiling. Sharing the primary's keeps the two halves
    /// comparable, which is what the specification asked for, but measured
    /// against real traffic the secondary is routinely seven to twelve times
    /// smaller — so on a 15 px half it drew one or two pixels above the midline
    /// and read as flat. Each half now fills its own space and the hero plot
    /// labels both ceilings, which puts the asymmetry on the screen instead of
    /// hiding it in a scale nobody can see.
    pub ceiling: f32,
}

pub fn chart(
    dc: HDC,
    surf: Option<&Surface>,
    r: &RECT,
    ring: &Ring,
    ceiling: f32,
    color: u32,
    size: ChartSize,
    scale: f32,
    hover: Option<usize>,
    font_micro: Option<HFONT>,
    units: Units,
    mirror: Option<Mirror>,
) {
    let cap = ring.capacity();
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 2 || h <= 2 || cap < 2 {
        return;
    }
    let ceiling = ceiling.max(1.0);
    let samples: Vec<f32> = ring.iter().collect();
    let start_slot = cap.saturating_sub(samples.len());
    let px = |i: usize, start: usize| {
        r.left as f32 + ((start + i) as f32 * (w - 1) as f32) / (cap - 1) as f32
    };

    // One quantity, two directions: the axis splits about a midline and each
    // half gets the full ceiling, so a symmetric burst draws symmetrically.
    // Single-series charts keep the bottom of the plot as their zero.
    let two_way = mirror.is_some();
    let anchor = if two_way { (r.top + r.bottom) / 2 } else { r.bottom - 1 };
    let span = if two_way { (h / 2 - 1).max(1) } else { (h - 2).max(1) };
    let py = |v: f32| anchor as f32 - span as f32 * (v / ceiling).clamp(0.0, 1.0);

    // 2. Un-sampled shading — the window visibly fills on first run instead of
    //    pretending the missing history was zero.
    if start_slot > 0 {
        let edge = px(0, start_slot).round() as i32;
        if edge > r.left {
            let unseen = RECT { left: r.left, top: r.top, right: edge, bottom: r.bottom };
            fill(dc, &unseen, mix(t().card, t().grid, 0.5));
        }
    }

    // 3. Gridlines, gated on device height so more DPI buys more detail rather
    //    than a more crowded box. A mirrored chart already has a reference line
    //    through its middle, and gridlines either side of it read as a grid of
    //    two charts rather than one axis.
    if size == ChartSize::Hero && !two_way {
        let fracs: &[f32] = if h >= 80 {
            &[0.25, 0.5, 0.75]
        } else if h >= 40 {
            &[0.5]
        } else {
            &[]
        };
        for f in fracs {
            let gy = py(ceiling * f).round() as i32;
            let g = RECT { left: r.left, top: gy, right: r.right, bottom: gy + 1 };
            fill(dc, &g, t().grid);
        }
    }

    let pts: Vec<(f32, f32)> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| (px(i, start_slot), py(*v)))
        .collect();

    // Crosshair, behind the trace: a rule the line crosses reads as an axis, a
    // rule drawn over it reads as damage.
    if let Some(i) = hover.filter(|i| *i < pts.len()) {
        let hx = pts[i].0.round() as i32;
        let bottom = if two_way { r.bottom } else { anchor };
        let rule = RECT { left: hx, top: r.top, right: hx + 1, bottom };
        fill(dc, &rule, t().grid);
    }

    // 4. Wash: clip to the area between the trace and the anchor, then one
    //    vertical gradient over that half. Because the gradient runs in screen y
    //    rather than per column, a busy chart is luminous and a quiet one nearly
    //    bare — which is the correct reading, and free. Each half of a mirrored
    //    chart runs its gradient *away* from the midline, so the midline is
    //    where both fade out.
    let light = luminance(t().card) > 0.5;
    let top_f = if light { 0.20 } else { 0.26 };
    let wash = |pts: &[(f32, f32)], c: u32, y_strong: i32, y_weak: i32| {
        if pts.len() < 2 {
            return;
        }
        let poly: Vec<POINT> = pts
            .iter()
            .map(|(x, y)| POINT { x: x.round() as i32, y: y.round() as i32 })
            .chain([
                POINT { x: pts[pts.len() - 1].0.round() as i32, y: anchor },
                POINT { x: pts[0].0.round() as i32, y: anchor },
            ])
            .collect();
        let strong = mix(c, t().card, 1.0 - top_f);
        let weak = mix(c, t().card, 0.97);
        // GdiGradientFill wants increasing y, so the vertices are ordered
        // top-to-bottom and the colours swap instead.
        let (y0, c0, y1, c1) = if y_strong <= y_weak {
            (y_strong, strong, y_weak, weak)
        } else {
            (y_weak, weak, y_strong, strong)
        };
        unsafe {
            let rgn = CreatePolygonRgn(poly.as_ptr(), poly.len() as i32, WINDING as i32);
            SelectClipRgn(dc, rgn);
            let mut v = [
                TRIVERTEX {
                    x: r.left,
                    y: y0,
                    Red: chan16(c0, 0),
                    Green: chan16(c0, 8),
                    Blue: chan16(c0, 16),
                    Alpha: 0,
                },
                TRIVERTEX {
                    x: r.right,
                    y: y1,
                    Red: chan16(c1, 0),
                    Green: chan16(c1, 8),
                    Blue: chan16(c1, 16),
                    Alpha: 0,
                },
            ];
            let mut band = GRADIENT_RECT { UpperLeft: 0, LowerRight: 1 };
            GdiGradientFill(
                dc,
                v.as_mut_ptr(),
                2,
                &mut band as *mut _ as *mut core::ffi::c_void,
                1,
                GRADIENT_FILL_RECT_V,
            );
            SelectClipRgn(dc, std::ptr::null_mut());
            DeleteObject(rgn as HGDIOBJ);
        }
    };

    // Direction is not identity: the second half is two steps of the *same* hue,
    // not a second hue. A distinct colour would collide with the accent that
    // already means another metric — an orange disk-write trace reads as the
    // network — so the halves are told apart by the permanent labels below and
    // by strength, never by hue.
    let color_lo = mirror.as_ref().map(|m| m.color).unwrap_or(color);
    let lo_pts: Vec<(f32, f32)> = match &mirror {
        Some(m) => {
            let ceil_lo = m.ceiling.max(1.0);
            let py_own = |v: f32| {
                anchor as f32 + span as f32 * (v / ceil_lo).clamp(0.0, 1.0)
            };
            let s2: Vec<f32> = m.ring.iter().collect();
            let start2 = cap.saturating_sub(s2.len());
            s2.iter().enumerate().map(|(i, v)| (px(i, start2), py_own(*v))).collect()
        }
        None => Vec::new(),
    };

    wash(&pts, color, r.top, anchor);
    if two_way {
        wash(&lo_pts, color_lo, r.bottom, anchor);
    }

    // 5. Baseline — always drawn, even at zero. This is what makes idle legible:
    //    a rate of 0 draws *on* the baseline, so the trace and the line coincide
    //    and the wash has no height, which is the honest picture of nothing
    //    happening. On a mirrored chart the same line is the midline both halves
    //    grow away from.
    let b = RECT { left: r.left, top: anchor, right: r.right, bottom: anchor + 1 };
    fill(dc, &b, t().line);

    // 6. Traces.
    let width = 1.5 * scale.max(1.0);
    let trace = |p: &[(f32, f32)], c: u32| match surf {
        Some(s) => aa_polyline(s, r, p, width, c),
        None => {
            let ip: Vec<POINT> =
                p.iter().map(|(x, y)| POINT { x: x.round() as i32, y: y.round() as i32 }).collect();
            if ip.len() >= 2 {
                unsafe {
                    let pen = CreatePen(PS_SOLID as i32, width.round() as i32, c);
                    let old = SelectObject(dc, pen as HGDIOBJ);
                    Polyline(dc, ip.as_ptr(), ip.len() as i32);
                    SelectObject(dc, old);
                    DeleteObject(pen as HGDIOBJ);
                }
            }
        }
    };
    trace(&pts, color);
    if two_way {
        trace(&lo_pts, color_lo);
    }

    // Permanent direct labels for the two halves — never colour alone. Gated on
    // there being a half tall enough to hold text: at row height the label would
    // sit on the trace, and the row's own value already names both directions.
    if let (Some(m), Some(f)) = (&mirror, font_micro) {
        let (asc, desc, _) = text_metrics(dc, f);
        if span >= asc + desc + 2 {
            text(dc, r.left + 2, r.top + 1, f, t().dim, m.label_hi);
            text(dc, r.left + 2, r.bottom - (asc + desc) - 1, f, t().dim, m.label_lo);
        }
    }

    // Peak marker. A mirrored chart gets one per half: the peak is the whole
    // reason to look at a disk or network graph, and with the halves on separate
    // ceilings an unlabelled high point says nothing about how high it was.
    // Suppressed when the peak is the newest sample — the head dot already says
    // that — or when the window is flat.
    if size == ChartSize::Hero {
        let peak = |pts: &[(f32, f32)], samples: &[f32], below: bool| {
            if pts.len() <= 2 {
                return;
            }
            let (mut pi, mut pv) = (0usize, f32::MIN);
            for (i, v) in samples.iter().enumerate() {
                if *v > pv {
                    (pi, pv) = (i, *v);
                }
            }
            let flat = samples.iter().cloned().fold(f32::MAX, f32::min) >= pv - 0.5;
            if pi + 1 >= pts.len() || flat || pv <= 0.0 {
                return;
            }
            let (hx, hy) = (pts[pi].0.round() as i32, pts[pi].1.round() as i32);
            let tick = RECT { left: hx, top: hy - 3, right: hx + 1, bottom: hy + 4 };
            fill(dc, &tick, t().dim);
            if let Some(f) = font_micro {
                let label = format!("peak {}", units.fmt(pv));
                let w = text_width(dc, f, &label);
                let (asc, desc, _) = text_metrics(dc, f);
                // Whichever side has room; a label that runs off the plate is
                // worse than one on the unexpected side.
                let lx = if hx + 4 + w <= r.right { hx + 4 } else { hx - 4 - w };
                // The lower half's label hangs below its point, or the two halves
                // would write over each other around the midline.
                let ly = if below { hy + 5 } else { hy - (asc + desc) - 3 };
                text(dc, lx.max(r.left), ly, f, t().dim, &label);
            }
        };
        peak(&pts, &samples, false);
        if let Some(m) = &mirror {
            let lo: Vec<f32> = m.ring.iter().collect();
            peak(&lo_pts, &lo, true);
        }
    }

    // The hovered sample's own dot, so the crosshair has something to point at.
    if let (Some(s), Some(i)) = (surf, hover.filter(|i| *i < pts.len())) {
        let (hx, hy) = pts[i];
        let rad = 3.0 * scale.max(1.0);
        aa_disc(s, r, hx, hy, rad + 1.5, t().card);
        aa_disc(s, r, hx, hy, rad, color);
        if let Some(&(lx, ly)) = lo_pts.get(i) {
            aa_disc(s, r, lx, ly, rad + 1.5, t().card);
            aa_disc(s, r, lx, ly, rad, color_lo);
        }
    }

    // Ceiling label, top-right — *on* the line it describes. Below the plot it
    // sat next to the baseline and read as though the axis bottomed out at
    // 100 %, which is the opposite of what it means.
    if size == ChartSize::Hero {
        if let Some(f) = font_micro {
            text_right(dc, r.right, r.top, f, t().dim, &units.fmt(ceiling));
            // A mirrored plot has two scales, so it needs two labels. Without
            // the second one the lower half would be an unlabelled axis and the
            // halves would look comparable when they are not.
            if let Some(m) = &mirror {
                let (asc, desc, _) = text_metrics(dc, f);
                text_right(dc, r.right, r.bottom - (asc + desc), f, t().dim, &units.fmt(m.ceiling));
            }
        }
    }

    // 7. Head dot: the entire motion language for "live".
    if let (Some(s), Some(&(hx, hy))) = (surf, pts.last()) {
        let rad = 2.5 * scale.max(1.0);
        aa_disc(s, r, hx, hy, rad + 1.0, t().card);
        aa_disc(s, r, hx, hy, rad, color);
    }
    if let (Some(s), Some(&(hx, hy))) = (surf, lo_pts.last()) {
        let rad = 2.5 * scale.max(1.0);
        aa_disc(s, r, hx, hy, rad + 1.0, t().card);
        aa_disc(s, r, hx, hy, rad, color_lo);
    }
}

/// A `COLORREF` channel widened to the 0..=65535 a `TRIVERTEX` wants.
fn chan16(c: u32, shift: u32) -> u16 {
    let v = ((c >> shift) & 0xff) as u16;
    (v << 8) | v
}

/// Inset input-field frame drawn behind an EDIT control: 1px border + dark
/// interior, so text boxes are visually distinct from static cards.
pub fn input_frame(dc: HDC, r: &RECT) {
    card(dc, r, t().input_bg, t().input_border, crate::util::RADIUS);
}

/// Horizontal usage bar: `frac` filled with accent, rest with track color.
pub fn bar(dc: HDC, r: &RECT, frac: f32, accent: u32) {
    let frac = frac.clamp(0.0, 1.0);
    // 2 px on bars and chart marks, against 4 on cards: the smaller radius
    // keeps an 8 px bar reading as a measure rather than a lozenge.
    let rad = 2;
    unsafe {
        let d = rad * 2;
        let track_rgn = CreateRoundRectRgn(r.left, r.top, r.right + 1, r.bottom + 1, d, d);
        let b = CreateSolidBrush(t().track);
        FillRgn(dc, track_rgn, b);
        DeleteObject(b as HGDIOBJ);
        let w = ((r.right - r.left) as f32 * frac) as i32;
        if w > 0 {
            // Clip the fill to the track's own region so the filled portion
            // inherits the rounded ends instead of poking square corners
            // through them at 100 %.
            SelectClipRgn(dc, track_rgn);
            let mut f = *r;
            f.right = r.left + w;
            fill(dc, &f, accent);
            SelectClipRgn(dc, std::ptr::null_mut());
        }
        DeleteObject(track_rgn as HGDIOBJ);
    }
}
