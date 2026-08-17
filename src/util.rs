//! Pure helpers: ring buffers for sparklines, formatting. Platform-independent.

/// Unscaled height of every interactive control: chips, input frames, buttons.
/// One number so a chip can never sit shorter than the input beside it — the
/// UI previously used four different heights for the same job.
pub const CTRL_H: i32 = 24;

/// Gap below a row of controls before the next thing starts.
pub const CTRL_GAP: i32 = SP3;

// ── Spacing scale ────────────────────────────────────────────────────────────
//
// Base unit 4, eight steps. These replace the twenty-one distinct `s(n)` values
// `draw_main` alone used to carry, and they are what makes `panel_height`
// derivable from the same numbers the paint code uses instead of a
// hand-maintained sum that had to agree with it by inspection.

/// Flush.
/// Part of the published scale; not yet consumed — see the sequencing table
/// in `docs/design/ui-foundation.md` §9 for which step claims it.
#[allow(dead_code)]
pub const SP0: i32 = 0;
/// Hairlines, icon-to-baseline nudges, mark gaps.
pub const SP1: i32 = 2;
/// Inside a control; a chip's vertical padding.
pub const SP2: i32 = 4;
/// Between a label and its value; icon to text.
pub const SP3: i32 = 8;
/// The panel gutter, and the gap inside a card.
pub const SP4: i32 = 12;
/// Between rows of related controls.
pub const SP5: i32 = 16;
/// Between sections.
pub const SP6: i32 = 24;
/// Above a section heading that follows content.
/// Part of the published scale; not yet consumed — see the sequencing table
/// in `docs/design/ui-foundation.md` §9 for which step claims it.
#[allow(dead_code)]
pub const SP7: i32 = 32;

// Derived rhythms, all multiples of 4.

/// A list row: process lists, agent rows, connection rows.
pub const ROW_LIST: i32 = 28;
/// A navigation row in settings.
pub const ROW_NAV: i32 = 32;
/// Navigation row plus the gap to the next.
pub const ROW_NAV_STRIDE: i32 = 40;
/// The metric row card itself.
pub const CARD_METRIC: i32 = 48;
/// Metric card plus the gap to the next card.
pub const ROW_METRIC: i32 = 56;
/// The header strip holding search and the window controls.
/// Part of the published scale; not yet consumed — see the sequencing table
/// in `docs/design/ui-foundation.md` §9 for which step claims it.
#[allow(dead_code)]
pub const HEADER_H: i32 = 40;
/// Header strip plus the gap below it.
/// Part of the published scale; not yet consumed — see the sequencing table
/// in `docs/design/ui-foundation.md` §9 for which step claims it.
#[allow(dead_code)]
pub const HEADER_STRIDE: i32 = 52;

/// Render a process affinity mask as a human range list: `all cores`,
/// `cores 0-7`, `cores 0-3, 8-11`.
///
/// Affinity is which cores a process is *allowed* on, which is the per-core
/// question about a process that Windows will answer cheaply. It will not tell
/// you which core a process is actually *on*: the only accurate source for that
/// is ETW context-switch tracing, needing a second kernel session, a
/// thread-to-process map kept from thread lifetime events, and thousands of
/// events a second on a busy machine.
pub fn affinity_label(proc_mask: u64, sys_mask: u64) -> String {
    if proc_mask == sys_mask {
        return "all cores".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0u32;
    while i < 64 {
        if proc_mask >> i & 1 == 1 {
            let start = i;
            while i + 1 < 64 && proc_mask >> (i + 1) & 1 == 1 {
                i += 1;
            }
            parts.push(if start == i { format!("{start}") } else { format!("{start}-{i}") });
        }
        i += 1;
    }
    if parts.is_empty() {
        "no cores".to_string()
    } else {
        format!("cores {}", parts.join(", "))
    }
}

/// Corner radius on cards, chips, input frames, chart plates and the widget
/// strip. Nothing in the product is fully round: a pill chip at `CTRL_H = 24`
/// would fight the rectangular data it sits beside.
pub const RADIUS: i32 = 4;

/// Nearest 1 / 2 / 5 × 10ⁿ at or above `v`. Quantising the ceiling is what
/// stops a decaying maximum from nudging the axis on every tick: the ceiling
/// only moves when the decay crosses a step boundary.
pub fn nice(v: f32) -> f32 {
    if !v.is_finite() || v <= 0.0 {
        return 1.0;
    }
    let p = 10f32.powf(v.log10().floor());
    let m = v / p;
    p * if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    }
}

/// How a chart decides its y-axis ceiling.
///
/// This is the fix that matters most in the whole redesign. Every chart used to
/// pass `ring.max()`, so the axis rescaled on almost every tick: fifty-seven
/// unchanged samples would visibly collapse to a third of their height the
/// moment a spike entered the window. The graph moved while the data did not.
#[derive(Copy, Clone, PartialEq)]
pub enum Scale {
    /// A percentage. Pinned to 100 and never rescales, ever.
    Percent,
    /// Frames per second: `nice(max(60, window_max))`, so a 60/120/144 Hz
    /// display holds one stable frame instead of breathing.
    Fps,
    /// A rate with no natural ceiling — bytes per second. Rises instantly to
    /// meet a spike, then decays slowly, quantised on the way.
    Rate,
    /// A fixed known ceiling, e.g. total installed RAM. Not yet used by a
    /// caller; kept because the hero chart needs it.
    #[allow(dead_code)]
    Fixed(f32),
}

/// The sticky part of [`Scale::Rate`] — one `f32` per ring.
#[derive(Default)]
pub struct Ceiling {
    raw: f32,
}

/// Fraction of the raw ceiling retained per tick. ~7 % decay: fast enough that
/// a finished download stops dominating the axis within a window, slow enough
/// that a bursty transfer does not pump.
const CEIL_DECAY: f32 = 0.93;

impl Ceiling {
    /// Advance one tick against this window's maximum and return the ceiling to
    /// draw with. Call exactly once per sample per chart.
    pub fn update(&mut self, window_max: f32) -> f32 {
        let m = if window_max.is_finite() { window_max.max(0.0) } else { 0.0 };
        self.raw = m.max(self.raw * CEIL_DECAY);
        nice(self.raw)
    }

    /// The ceiling without advancing the decay — for repaints that are not
    /// driven by a new sample (hover, resize, theme change).
    #[allow(dead_code)]
    pub fn peek_raw(&self) -> f32 {
        self.raw
    }

    pub fn peek(&self) -> f32 {
        nice(self.raw)
    }
}

impl Scale {
    /// Resolve to a ceiling. `sticky` is only consulted for [`Scale::Rate`].
    pub fn ceiling(self, window_max: f32, sticky: &Ceiling) -> f32 {
        match self {
            Scale::Percent => 100.0,
            Scale::Fps => nice(window_max.max(60.0)),
            Scale::Rate => sticky.peek(),
            Scale::Fixed(v) => v.max(1.0),
        }
    }
}

/// Scale a design-time pixel value for the current DPI.
pub fn scaled(v: i32, scale: f32) -> i32 {
    (v as f32 * scale) as i32
}

/// Height of any interactive control at this DPI.
pub fn ctrl_h(scale: f32) -> i32 {
    scaled(CTRL_H, scale)
}

/// Vertical advance past a row of controls at this DPI.
pub fn ctrl_row(scale: f32) -> i32 {
    ctrl_h(scale) + scaled(CTRL_GAP, scale)
}

/// Back out the taskbar widget's scale from a width the user dragged it to.
/// `base` is the strip's width at scale 1.0, so the ratio between the two is
/// the size being asked for. The display's own scaling is divided out, which
/// keeps the stored value meaningful when the widget is dragged to a monitor
/// with different DPI.
pub fn widget_scale_from_width(base: i32, width: i32, dpi: f32) -> f32 {
    if base <= 0 || !dpi.is_finite() || dpi <= 0.0 {
        return 1.0;
    }
    crate::config::clamp_widget_scale(width as f32 / base as f32 / dpi)
}

/// Fixed-capacity ring buffer holding the last N samples for a sparkline.
pub struct Ring {
    buf: Vec<f32>,
    head: usize,
    len: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Ring { buf: vec![0.0; cap], head: 0, len: 0 }
    }

    pub fn push(&mut self, v: f32) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % self.buf.len();
        self.len = (self.len + 1).min(self.buf.len());
    }

    /// Samples oldest-to-newest.
    pub fn iter(&self) -> impl Iterator<Item = f32> + '_ {
        let cap = self.buf.len();
        let start = (self.head + cap - self.len) % cap;
        (0..self.len).map(move |i| self.buf[(start + i) % cap])
    }

    pub fn max(&self) -> f32 {
        self.iter().fold(0.0f32, f32::max)
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }
}

/// "512 B", "1.2 KB", "3.4 MB", "1.2 GB"
pub fn format_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", b, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// Bytes/sec with /s suffix.
pub fn format_rate(bps: u64) -> String {
    format!("{}/s", format_bytes(bps))
}

/// Ultra-compact rate for a 16px tray icon: "0", "37K", "1.2M", "12M", "3G".
pub fn compact_rate(bps: u64) -> String {
    const K: u64 = 1024;
    match bps {
        0..=1023 => "0".to_string(),
        v if v < K * K => format!("{}K", v / K),
        v if v < 10 * K * K => format!("{:.1}M", v as f64 / (K * K) as f64),
        v if v < K * K * K => format!("{}M", v / (K * K)),
        v => format!("{}G", v / (K * K * K)),
    }
}

/// Percentage clamped to 0..=100, no decimals.
pub fn format_pct(p: f32) -> String {
    format!("{:.0}%", p.clamp(0.0, 100.0))
}

/// Compute a per-second rate from two monotonically increasing counters.
/// Handles counter resets (returns 0) and sub-second elapsed times.
pub fn rate(prev: u64, cur: u64, elapsed_secs: f64) -> u64 {
    if cur < prev || elapsed_secs <= 0.0 {
        return 0;
    }
    ((cur - prev) as f64 / elapsed_secs) as u64
}

/// CPU percent from busy/total time deltas, clamped.
pub fn cpu_pct(busy_delta: u64, total_delta: u64) -> f32 {
    if total_delta == 0 {
        return 0.0;
    }
    ((busy_delta as f64 / total_delta as f64) * 100.0).clamp(0.0, 100.0) as f32
}

/// The trail of screens leading to the current one, which "back" walks out of.
///
/// Revisiting a screen already on the trail collapses back to it rather than
/// stacking a second copy. Without that, two screens that can each open the
/// other — a watched app and its connection list — end up pointing at each
/// other, and back bounces between them forever. Collapsing means the trail
/// can only shrink toward a screen it already holds, so back always makes
/// progress and always terminates.
pub struct NavTrail<T> {
    items: Vec<T>,
    cap: usize,
}

impl<T: Copy + PartialEq> NavTrail<T> {
    pub fn new(cap: usize) -> Self {
        NavTrail { items: Vec::new(), cap }
    }

    /// Moving forward from `current` to `next`.
    pub fn advance(&mut self, current: T, next: T) {
        if let Some(pos) = self.items.iter().position(|v| *v == next) {
            self.items.truncate(pos);
        } else {
            self.items.push(current);
            // A trail this long means someone is exploring, not lost; the
            // oldest entry is the least useful thing to keep.
            if self.items.len() > self.cap {
                self.items.remove(0);
            }
        }
    }

    /// One step back, or None when there is nowhere further to go.
    pub fn back(&mut self) -> Option<T> {
        self.items.pop()
    }

    /// Only the tests care how deep the trail is; the panel just walks it.
    #[cfg(test)]
    pub fn depth(&self) -> usize {
        self.items.len()
    }
}

/// Escape a string for a JSON value (control characters, quotes, backslash).
/// The app links no JSON crate on the writing side either: every response it
/// builds has a fixed shape, so escaping is the only part that needs care.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Pull `"key":"value"` out of a compact JSON object slice. The app links no
/// JSON crate and the shape here is fixed and produced by our own MCP shim,
/// so a scanner is enough — it only has to survive escapes and missing keys,
/// not parse arbitrary JSON. Returns "" when the key is absent or non-string.
pub fn json_str_field(obj: &str, key: &str) -> String {
    let needle = format!("\"{}\"", key);
    let Some(k) = obj.find(&needle) else { return String::new() };
    let rest = &obj[k + needle.len()..];
    let Some(colon) = rest.find(':') else { return String::new() };
    let rest = rest[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return String::new();
    }
    let chars: Vec<char> = rest.chars().collect();
    let mut out = String::new();
    let mut i = 1;
    while i < chars.len() {
        match chars[i] {
            '"' => break,
            '\\' if i + 1 < chars.len() => {
                i += 1;
                match chars[i] {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let (c, consumed) = decode_unicode_escape(&chars[i + 1..]);
                        out.push(c);
                        i += consumed;
                    }
                    c => out.push(c),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// Decode the hex tail of a `\u` escape starting after the `u`, returning the
/// character and how many input chars were consumed. serde encodes non-BMP
/// characters as surrogate pairs, so `😀` must come back as one 😀;
/// anything malformed decodes as U+FFFD without derailing the rest.
fn decode_unicode_escape(rest: &[char]) -> (char, usize) {
    fn hex4(chars: &[char]) -> Option<u32> {
        if chars.len() < 4 {
            return None;
        }
        chars[..4].iter().try_fold(0u32, |v, c| c.to_digit(16).map(|d| v * 16 + d))
    }
    let Some(hi) = hex4(rest) else {
        return ('\u{fffd}', rest.len().min(4));
    };
    if (0xD800..0xDC00).contains(&hi) {
        // High surrogate: only meaningful with a low surrogate right behind.
        if rest.len() >= 10 && rest[4] == '\\' && rest[5] == 'u' {
            if let Some(lo) = hex4(&rest[6..]) {
                if (0xDC00..0xE000).contains(&lo) {
                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                    return (char::from_u32(cp).unwrap_or('\u{fffd}'), 10);
                }
            }
        }
        return ('\u{fffd}', 4);
    }
    (char::from_u32(hi).unwrap_or('\u{fffd}'), 4)
}

/// Split a compact JSON array of objects into its top-level object slices,
/// tracking strings and nesting so a `}` inside a title cannot end an entry.
pub fn json_objects(arr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in arr.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(arr[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_label_reads_as_ranges() {
        assert_eq!(affinity_label(0xff, 0xff), "all cores");
        assert_eq!(affinity_label(0b0000_1111, 0xffff), "cores 0-3");
        assert_eq!(affinity_label(0b1111_0000_1111, 0xffff), "cores 0-3, 8-11");
        assert_eq!(affinity_label(0b0010_0001, 0xffff), "cores 0, 5");
        // Stated, not blank: a process pinned to nothing is worth saying.
        assert_eq!(affinity_label(0, 0xff), "no cores");
        // A single core still reads as one number, not a degenerate range.
        assert_eq!(affinity_label(1, 0xffff), "cores 0");
    }

    #[test]
    fn nice_quantises_to_1_2_5_decades() {
        assert_eq!(nice(0.0), 1.0);
        assert_eq!(nice(-5.0), 1.0);
        assert_eq!(nice(f32::NAN), 1.0);
        assert_eq!(nice(1.0), 1.0);
        assert_eq!(nice(1.5), 2.0);
        assert_eq!(nice(2.0), 2.0);
        assert_eq!(nice(4.9), 5.0);
        assert_eq!(nice(5.1), 10.0);
        assert_eq!(nice(37.0), 50.0);
        assert_eq!(nice(1_200_000.0), 2_000_000.0);
    }

    #[test]
    fn percent_scale_never_moves() {
        // The whole point: a percentage chart's axis is a constant, so the
        // history cannot shift under a spike.
        let c = Ceiling::default();
        for m in [0.0, 3.0, 41.7, 99.9, 100.0] {
            assert_eq!(Scale::Percent.ceiling(m, &c), 100.0);
        }
    }

    #[test]
    fn fps_scale_holds_a_stable_frame() {
        let c = Ceiling::default();
        // An idle or 60 Hz display sits on one ceiling rather than breathing.
        assert_eq!(Scale::Fps.ceiling(0.0, &c), 100.0);
        assert_eq!(Scale::Fps.ceiling(59.0, &c), 100.0);
        assert_eq!(Scale::Fps.ceiling(144.0, &c), 200.0);
    }

    #[test]
    fn rate_ceiling_rises_at_once_and_decays_slowly() {
        let mut c = Ceiling::default();
        // Rises instantly to meet a spike.
        assert_eq!(c.update(30.0), 50.0);
        // Traffic stops. The ceiling must not snap down with it, or the
        // remaining history would leap up the plot on the very next frame.
        assert_eq!(c.update(0.0), 50.0);
        assert_eq!(c.update(0.0), 50.0);
        // ...but it does eventually come down.
        let mut ticks = 0;
        while c.peek() > 5.0 && ticks < 500 {
            c.update(0.0);
            ticks += 1;
        }
        assert!(ticks > 20, "decay should take tens of ticks, took {ticks}");
        assert!(ticks < 200, "decay should not take forever, took {ticks}");
    }

    #[test]
    fn rate_ceiling_is_stable_across_ordinary_variation() {
        // The failure this replaces: a window whose max wobbles between 30 and
        // 45 used to rescale the axis on nearly every tick. Quantising means
        // the drawn ceiling is identical throughout.
        let mut c = Ceiling::default();
        c.update(45.0);
        let first = c.peek();
        for m in [30.0, 44.0, 33.0, 41.0, 38.0, 45.0, 31.0] {
            assert_eq!(c.update(m), first, "ceiling moved on a {m} sample");
        }
    }

    #[test]
    fn rate_ceiling_survives_nonsense_input() {
        let mut c = Ceiling::default();
        assert!(c.update(f32::NAN).is_finite());
        assert!(c.update(f32::INFINITY).is_finite());
        assert!(c.update(-1.0).is_finite());
    }

    #[test]
    fn json_field_reads_strings_and_escapes() {
        let obj = r#"{"id":"a1","title":"code \"review\"","detail":"line\nbreak"}"#;
        assert_eq!(json_str_field(obj, "id"), "a1");
        assert_eq!(json_str_field(obj, "title"), "code \"review\"");
        assert_eq!(json_str_field(obj, "detail"), "line\nbreak");
        assert_eq!(json_str_field(obj, "missing"), "");
    }

    #[test]
    fn json_field_decodes_every_json_escape() {
        // serde on the shim side emits \uXXXX for control characters (ANSI
        // colour codes in captured build output, for one) and surrogate pairs
        // for emoji; a decoder that only knows n/t/r turns them into garbage.
        let obj = concat!(
            r#"{"a":"\u0041","esc":"\u001b[31m","emoji":"\ud83d\ude00","#,
            r#""bf":"\b\f","slash":"a\/b"}"#
        );
        assert_eq!(json_str_field(obj, "a"), "A");
        assert_eq!(json_str_field(obj, "esc"), "\u{1b}[31m");
        assert_eq!(json_str_field(obj, "emoji"), "\u{1f600}");
        assert_eq!(json_str_field(obj, "bf"), "\u{8}\u{c}");
        assert_eq!(json_str_field(obj, "slash"), "a/b");
    }

    #[test]
    fn json_field_survives_malformed_unicode_escapes() {
        // A lone high surrogate or truncated hex must not panic or derail the
        // rest of the string.
        assert_eq!(json_str_field(r#"{"x":"a\ud83dz"}"#, "x"), "a\u{fffd}z");
        assert_eq!(json_str_field(r#"{"x":"a\uZZZZz"}"#, "x"), "a\u{fffd}z");
        assert_eq!(json_str_field(r#"{"x":"a\u00"}"#, "x"), "a\u{fffd}");
    }

    #[test]
    fn json_field_ignores_non_string_values() {
        let obj = r#"{"count":3,"title":"x"}"#;
        assert_eq!(json_str_field(obj, "count"), "");
        assert_eq!(json_str_field(obj, "title"), "x");
    }

    #[test]
    fn json_objects_splits_top_level_only() {
        let arr = r#"[{"id":"a","meta":{"n":1}},{"id":"b"}]"#;
        let objs = json_objects(arr);
        assert_eq!(objs.len(), 2);
        assert_eq!(json_str_field(&objs[0], "id"), "a");
        assert_eq!(json_str_field(&objs[1], "id"), "b");
    }

    #[test]
    fn json_objects_survives_braces_inside_strings() {
        // A title containing a brace must not terminate the entry early.
        let arr = r#"[{"id":"a","title":"fix } bug"},{"id":"b"}]"#;
        let objs = json_objects(arr);
        assert_eq!(objs.len(), 2);
        assert_eq!(json_str_field(&objs[0], "title"), "fix } bug");
        assert_eq!(json_str_field(&objs[1], "id"), "b");
    }

    #[test]
    fn json_objects_handles_empty_and_garbage() {
        assert!(json_objects("[]").is_empty());
        assert!(json_objects("").is_empty());
        assert!(json_objects("{unterminated").is_empty());
    }

    #[test]
    fn ring_wraps_and_iterates_in_order() {
        let mut r = Ring::new(3);
        assert_eq!(r.iter().count(), 0);
        r.push(1.0);
        r.push(2.0);
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![1.0, 2.0]);
        r.push(3.0);
        r.push(4.0); // overwrites 1.0
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![2.0, 3.0, 4.0]);
        assert_eq!(r.max(), 4.0);
    }

    #[test]
    fn bytes_formatting() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(format_bytes(u64::MAX / 2), "8388608.0 TB");
    }

    #[test]
    fn rate_handles_reset_and_zero_elapsed() {
        assert_eq!(rate(100, 300, 2.0), 100);
        assert_eq!(rate(300, 100, 1.0), 0); // counter reset
        assert_eq!(rate(0, 100, 0.0), 0);
    }

    #[test]
    fn cpu_pct_clamps() {
        assert_eq!(cpu_pct(50, 100), 50.0);
        assert_eq!(cpu_pct(0, 0), 0.0);
        assert_eq!(cpu_pct(200, 100), 100.0);
    }

    #[test]
    fn compact_rate_tiers() {
        assert_eq!(compact_rate(0), "0");
        assert_eq!(compact_rate(500), "0");
        assert_eq!(compact_rate(37 * 1024), "37K");
        assert_eq!(compact_rate(1258291), "1.2M"); // ~1.2 MiB
        assert_eq!(compact_rate(12 * 1024 * 1024), "12M");
        assert_eq!(compact_rate(3 * 1024 * 1024 * 1024), "3G");
    }

    #[test]
    fn pct_formatting() {
        assert_eq!(format_pct(42.4), "42%");
        assert_eq!(format_pct(150.0), "100%");
        assert_eq!(format_pct(-5.0), "0%");
    }

    #[test]
    fn control_height_is_one_number_at_every_scale() {
        // Chips, input frames and buttons all size from ctrl_h. If any of them
        // ever grows its own constant again, this is the test that should have
        // caught it — the mismatch that started this was 20 vs 24 at scale 1.
        for scale in [1.0f32, 1.25, 1.5, 2.0, 3.0] {
            let h = ctrl_h(scale);
            assert_eq!(h, scaled(CTRL_H, scale));
            assert!(h > 0, "control height collapsed at scale {}", scale);
            assert_eq!(ctrl_row(scale), h + scaled(CTRL_GAP, scale));
            assert!(ctrl_row(scale) > h, "no gap below controls at scale {}", scale);
        }
    }

    #[test]
    fn widget_scale_reads_back_the_width_it_was_dragged_to() {
        // Dragging to twice the base width asks for twice the size.
        assert_eq!(widget_scale_from_width(300, 600, 1.0), 2.0);
        // The display's scaling is divided out, so the stored value means the
        // same thing on a 200% monitor as on a 100% one.
        assert_eq!(widget_scale_from_width(300, 600, 2.0), 1.0);
        // Whatever the widget is drawn at should come back out of its width.
        for want in [0.75f32, 1.0, 1.5, 2.5] {
            let width = (300.0 * want * 1.5) as i32;
            let got = widget_scale_from_width(300, width, 1.5);
            assert!((got - want).abs() < 0.01, "{} came back as {}", want, got);
        }
    }

    #[test]
    fn widget_scale_clamps_and_survives_nonsense() {
        assert_eq!(widget_scale_from_width(300, 100_000, 1.0), crate::config::WIDGET_SCALE_MAX);
        assert_eq!(widget_scale_from_width(300, 1, 1.0), crate::config::WIDGET_SCALE_MIN);
        // A zero base or DPI would divide the widget out of existence.
        assert_eq!(widget_scale_from_width(0, 600, 1.0), 1.0);
        assert_eq!(widget_scale_from_width(300, 600, 0.0), 1.0);
        assert_eq!(widget_scale_from_width(300, 600, f32::NAN), 1.0);
    }

    #[test]
    fn control_height_grows_monotonically_with_scale() {
        // A taller display must never yield a shorter control.
        let mut last = 0;
        for scale in [1.0f32, 1.25, 1.5, 2.0, 3.0] {
            let h = ctrl_h(scale);
            assert!(h >= last, "ctrl_h shrank going up to scale {}", scale);
            last = h;
        }
    }

    /// Stand-in for the panel's `View`, with only the screens that mattered.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Screen {
        Main,
        NetDrill,
        Conns,
        App,
        Settings,
    }

    /// Walk back until there is nowhere left, and report the path taken.
    /// Bounded so a trail that cannot terminate fails the test instead of
    /// hanging it.
    fn walk_back(trail: &mut NavTrail<Screen>) -> Vec<Screen> {
        let mut seen = Vec::new();
        for _ in 0..50 {
            match trail.back() {
                Some(v) => seen.push(v),
                None => return seen,
            }
        }
        panic!("back never reached the end: {:?}", seen);
    }

    #[test]
    fn back_walks_out_the_way_it_came() {
        let mut t = NavTrail::new(16);
        t.advance(Screen::Main, Screen::NetDrill);
        t.advance(Screen::NetDrill, Screen::Conns);
        t.advance(Screen::Conns, Screen::App);
        assert_eq!(
            walk_back(&mut t),
            vec![Screen::Conns, Screen::NetDrill, Screen::Main]
        );
    }

    #[test]
    fn an_app_and_its_connections_cannot_trap_back() {
        // The reported bug: opening an app from the connections list and then
        // its connections again left back bouncing between the two forever.
        let mut t = NavTrail::new(16);
        t.advance(Screen::Main, Screen::NetDrill);
        t.advance(Screen::NetDrill, Screen::Conns);
        t.advance(Screen::Conns, Screen::App);
        // ...and back to the connections, filtered to that app.
        t.advance(Screen::App, Screen::Conns);
        // The second visit collapsed onto the first, so back still leads out.
        assert_eq!(walk_back(&mut t), vec![Screen::NetDrill, Screen::Main]);
    }

    #[test]
    fn bouncing_between_two_screens_never_deepens_the_trail() {
        let mut t = NavTrail::new(16);
        t.advance(Screen::Main, Screen::App);
        let depth = t.depth();
        for _ in 0..20 {
            t.advance(Screen::App, Screen::Conns);
            t.advance(Screen::Conns, Screen::App);
        }
        assert_eq!(t.depth(), depth, "a round trip must not grow the trail");
        assert_eq!(walk_back(&mut t), vec![Screen::Main]);
    }

    #[test]
    fn back_from_the_first_screen_has_nowhere_to_go() {
        let mut t: NavTrail<Screen> = NavTrail::new(16);
        assert_eq!(t.back(), None, "the caller falls back to the main view");
    }

    #[test]
    fn a_long_trail_is_capped_but_still_walks_out() {
        let mut t = NavTrail::new(3);
        let mut prev = Screen::Main;
        // Alternating so no entry collapses; only the cap can trim it.
        for next in [Screen::NetDrill, Screen::Settings, Screen::NetDrill, Screen::Settings, Screen::NetDrill] {
            t.advance(prev, next);
            prev = next;
        }
        assert!(t.depth() <= 3, "depth {} exceeded the cap", t.depth());
        walk_back(&mut t);
    }
}
