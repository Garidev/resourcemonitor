//! Minimal GDI helpers: colors, fonts, double buffer, primitives, sparklines.

use windows_sys::Win32::Foundation::{POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;

use crate::util::Ring;

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Surface palette; selectable at runtime from settings.
pub struct Theme {
    pub name: &'static str,
    pub bg: u32,
    pub card: u32,
    pub card_hover: u32,
    pub input_bg: u32,
    pub input_border: u32,
    pub text: u32,
    pub dim: u32,
    pub track: u32,
}

pub const THEMES: [Theme; 3] = [
    Theme {
        name: "dark",
        bg: rgb(24, 25, 28),
        card: rgb(33, 35, 40),
        card_hover: rgb(45, 48, 55),
        input_bg: rgb(15, 16, 19),
        input_border: rgb(76, 82, 92),
        text: rgb(230, 232, 235),
        dim: rgb(140, 145, 152),
        track: rgb(50, 53, 60),
    },
    Theme {
        name: "black",
        bg: rgb(0, 0, 0),
        card: rgb(16, 17, 20),
        card_hover: rgb(30, 32, 38),
        input_bg: rgb(8, 8, 10),
        input_border: rgb(60, 64, 72),
        text: rgb(235, 237, 240),
        dim: rgb(130, 135, 142),
        track: rgb(34, 36, 42),
    },
    Theme {
        name: "light",
        bg: rgb(243, 244, 246),
        card: rgb(255, 255, 255),
        card_hover: rgb(229, 231, 235),
        input_bg: rgb(255, 255, 255),
        input_border: rgb(160, 166, 175),
        text: rgb(24, 28, 33),
        dim: rgb(105, 112, 120),
        track: rgb(209, 213, 219),
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

pub const ACC_CPU: u32 = rgb(79, 163, 255);
pub const ACC_RAM: u32 = rgb(52, 211, 153);
pub const ACC_DISK: u32 = rgb(167, 139, 250);
pub const ACC_NET: u32 = rgb(245, 158, 11);
pub const ACC_FPS: u32 = rgb(255, 107, 107);
pub const ACC_GPU: u32 = rgb(45, 212, 191);
pub const ACC_AUDIO: u32 = rgb(232, 121, 249);

/// An accent darkened toward black by `f` (0.0 black, 1.0 unchanged). Lets a
/// control derive its border and interior from whichever accent it carries,
/// so a second colour costs nothing but the constant.
pub fn shade(c: u32, f: f32) -> u32 {
    let ch = |sh: u32| ((((c >> sh) & 0xff) as f32) * f) as u8;
    rgb(ch(0), ch(8), ch(16))
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

pub struct Fonts {
    pub normal: HFONT,
    pub bold: HFONT,
    pub bold_sm: HFONT,
    pub small: HFONT,
}

impl Fonts {
    pub fn new(scale: f32) -> Self {
        let s = |v: f32| (v * scale) as i32;
        Fonts {
            normal: make_font(s(14.0), 400),
            bold: make_font(s(14.0), 700),
            bold_sm: make_font(s(12.0), 700),
            small: make_font(s(11.0), 400),
        }
    }

    /// Fonts to try, largest first, when fitting text into a width.
    pub fn fit_stack(&self) -> [HFONT; 3] {
        [self.bold, self.bold_sm, self.small]
    }

    /// Release the handles. Only called when replacing the set — GDI font
    /// handles are a limited resource, so rebuilding at a new size must not
    /// leak the old one.
    pub fn destroy(&self) {
        for f in [self.normal, self.bold, self.bold_sm, self.small] {
            unsafe { DeleteObject(f as HGDIOBJ) };
        }
    }
}

pub struct BackBuffer {
    pub dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    target: HDC,
    w: i32,
    h: i32,
}

impl BackBuffer {
    pub fn new(target: HDC, w: i32, h: i32) -> Self {
        unsafe {
            let dc = CreateCompatibleDC(target);
            let bmp = CreateCompatibleBitmap(target, w, h);
            let old = SelectObject(dc, bmp as HGDIOBJ);
            BackBuffer { dc, bmp, old, target, w, h }
        }
    }

    pub fn present(&self) {
        unsafe {
            BitBlt(self.target, 0, 0, self.w, self.h, self.dc, 0, 0, SRCCOPY);
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

pub fn text(dc: HDC, x: i32, y: i32, font: HFONT, color: u32, s: &str) {
    let w: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        SelectObject(dc, font as HGDIOBJ);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        TextOutW(dc, x, y, w.as_ptr(), w.len() as i32);
    }
}

pub fn text_width(dc: HDC, font: HFONT, s: &str) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut size = SIZE { cx: 0, cy: 0 };
    unsafe {
        SelectObject(dc, font as HGDIOBJ);
        GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32, &mut size);
    }
    size.cx
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
    let cx = text_width(dc, font, s);
    text(dc, right - cx, y, font, color, s);
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
        let pen = CreatePen(PS_SOLID as i32, thickness.max(1), color);
        let old = SelectObject(dc, pen as HGDIOBJ);
        Polyline(dc, pts.as_ptr(), pts.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen as HGDIOBJ);
    }
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
        let pen = CreatePen(PS_SOLID as i32, thickness.max(1), color);
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

/// Line sparkline of `ring` inside `r`, values scaled to `max` (min 1.0).
pub fn sparkline(dc: HDC, r: &RECT, ring: &Ring, max: f32, color: u32) {
    let n = ring.capacity();
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if w <= 2 || h <= 2 {
        return;
    }
    let max = max.max(1.0);
    let mut pts: Vec<POINT> = Vec::with_capacity(n);
    let samples: Vec<f32> = ring.iter().collect();
    // Right-align: newest sample at the right edge.
    let start_slot = n.saturating_sub(samples.len());
    for (i, v) in samples.iter().enumerate() {
        let x = r.left + ((start_slot + i) as i32 * (w - 1)) / (n.max(2) as i32 - 1);
        let frac = (v / max).clamp(0.0, 1.0);
        let y = r.bottom - 1 - ((h - 2) as f32 * frac) as i32;
        pts.push(POINT { x, y });
    }
    if pts.len() < 2 {
        return;
    }
    unsafe {
        let pen = CreatePen(PS_SOLID as i32, 1, color);
        let old = SelectObject(dc, pen as HGDIOBJ);
        Polyline(dc, pts.as_ptr(), pts.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen as HGDIOBJ);
    }
}

/// Inset input-field frame drawn behind an EDIT control: 1px border + dark
/// interior, so text boxes are visually distinct from static cards.
pub fn input_frame(dc: HDC, r: &RECT) {
    fill(dc, r, t().input_border);
    let inner = RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 };
    fill(dc, &inner, t().input_bg);
}

/// Horizontal usage bar: `frac` filled with accent, rest with track color.
pub fn bar(dc: HDC, r: &RECT, frac: f32, accent: u32) {
    let frac = frac.clamp(0.0, 1.0);
    fill(dc, r, t().track);
    let mut f = *r;
    f.right = r.left + ((r.right - r.left) as f32 * frac) as i32;
    fill(dc, &f, accent);
}
