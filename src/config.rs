//! Settings persisted as a flat key=value file (%LOCALAPPDATA%\resmon.ini).
//! Parsing/serializing is platform-independent and unit-tested.

/// Notification expectations handed to connected AI tools (`notify_presets`).
pub const NOTIFY_FINISHED: u32 = 1;
pub const NOTIFY_ERRORS: u32 = 2;
pub const NOTIFY_INPUT: u32 = 4;
pub const NOTIFY_VERBOSE: u32 = 8;

/// Escape a value so it survives a line-based `key=value` file. Without this
/// a single newline in the free-text box would split into a junk line and
/// silently truncate the setting.
fn escape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            // Unknown escape: keep both characters rather than eat them.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub tray_static: bool,
    pub tray_cpu: bool,
    pub tray_ram: bool,
    pub tray_disk: bool,
    pub tray_net: bool,
    pub tray_fps: bool,
    pub interval_ms: u32,
    /// MCP server: let local AI tools query the monitor and post messages.
    pub mcp_enabled: bool,
    /// Which notification expectations to hand the AI. Bitmask, see NOTIFY_*.
    pub notify_presets: u32,
    /// Extra notification instructions the user typed, each with its own
    /// on/off flag so one can be silenced without deleting it. Stored one per
    /// `notify_rule<N>=` line as `1|text`, escaped because the ini is
    /// line-based.
    pub notify_custom: Vec<(bool, String)>,
    pub pinned: bool,
    pub on_top: bool,
    pub win_x: i32,
    pub win_y: i32,
    /// Outer size of the pinned window; -1 = default.
    pub win_w: i32,
    pub win_h: i32,
    /// FPS overlay: floating always-on-top frame counter.
    pub fps_overlay: bool,
    /// Index into the overlay color presets.
    pub fps_color: u32,
    /// Index into the overlay opacity presets.
    pub fps_opacity: u32,
    pub fps_x: i32,
    pub fps_y: i32,
    /// Index into gdi::THEMES.
    pub theme: u32,
    /// Index into `TEXT_SIZES`. Multiplies the DPI scale, so the whole panel
    /// grows with the text rather than the text outgrowing its layout.
    pub text_size: u32,
    /// Taskbar widget: floating always-visible metric strip.
    pub widget_on: bool,
    /// Bitmask of metrics shown: 1 cpu, 2 ram, 4 net, 8 disk, 16 fps, 32 gpu,
    /// 64 ai (running agents and waiting messages).
    pub widget_mask: u32,
    pub widget_x: i32,
    pub widget_y: i32,
    /// Index into `gdi::THEMES`, independent of the main panel's `theme` — the
    /// widget sits on the taskbar rather than beside the panel, so it does not
    /// have to match it. Clamped where it is read, not here, mirroring `theme`.
    pub widget_theme: u32,
    /// Widget size, as a multiplier on the DPI scale. Its own value rather than
    /// the panel's `text_size`, so the strip can be matched to the taskbar
    /// without dragging the whole panel's type along with it. Set by dragging
    /// the widget's bottom-right corner.
    pub widget_scale: f32,
    /// Where the unpinned flyout was last dragged to; -1 means anchor it above
    /// the taskbar near the cursor, which is what it did before it could be
    /// moved at all.
    pub fly_x: i32,
    pub fly_y: i32,
    /// Raw `logN=` rule lines, kept verbatim (parsed by the rules module).
    pub rule_lines: Vec<String>,
    /// Main-panel metric rows as (name, visible), in display order. Carries
    /// every known metric, hidden ones included, so that a metric the user
    /// deliberately hid stays hidden while one that simply did not exist when
    /// the file was written can still be told apart and shown.
    pub main_metrics: Vec<(String, bool)>,
    /// Where to append finished agents, or empty for session-only history.
    /// Empty means off, as it does for an alert rule's `file=`.
    pub agent_log_file: String,
}

/// Text size presets: (label, multiplier on the DPI scale). Windows already
/// gives us the display's own scaling; this is the user saying that, on top of
/// that, they want it bigger.
pub const TEXT_SIZES: [(&str, f32); 4] = [
    ("small", 0.9),
    ("default", 1.0),
    ("large", 1.15),
    ("larger", 1.3),
];

/// The chosen multiplier, falling back to 1.0 for an out-of-range index.
pub fn text_scale(idx: u32) -> f32 {
    TEXT_SIZES.get(idx as usize).map(|(_, m)| *m).unwrap_or(1.0)
}

/// How far the widget can be dragged. Below the minimum its labels stop being
/// readable; above the maximum it stops being a strip.
pub const WIDGET_SCALE_MIN: f32 = 0.6;
pub const WIDGET_SCALE_MAX: f32 = 3.0;

/// Bring a dragged or parsed widget scale into range, and quantise it to the
/// three decimals the ini stores so that a saved size reloads bit-identical
/// instead of drifting a little on every restart.
pub fn clamp_widget_scale(v: f32) -> f32 {
    if !v.is_finite() {
        return 1.0;
    }
    (v.clamp(WIDGET_SCALE_MIN, WIDGET_SCALE_MAX) * 1000.0).round() / 1000.0
}

/// Every metric the main panel can draw, in the order shipped by default.
pub const MAIN_METRICS: [&str; 7] = ["cpu", "ram", "gpu", "fps", "disk", "net", "audio"];

/// Parse a `main_metrics=` value into (name, visible) pairs.
///
/// A leading `-` hides a metric. Unknown and repeated names are dropped, and
/// any known metric the line does not mention is appended visible at the end —
/// without that, a metric added in a later version would be invisible to
/// everyone holding a saved config, with no way to discover it.
pub fn parse_main_metrics(v: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for tok in v.split(',') {
        let tok = tok.trim();
        let (name, visible) = match tok.strip_prefix('-') {
            Some(rest) => (rest.trim(), false),
            None => (tok, true),
        };
        if !MAIN_METRICS.contains(&name) || out.iter().any(|(n, _)| n == name) {
            continue;
        }
        out.push((name.to_string(), visible));
    }
    for m in MAIN_METRICS {
        if !out.iter().any(|(n, _)| n == m) {
            out.push((m.to_string(), true));
        }
    }
    out
}

/// Inverse of `parse_main_metrics`.
pub fn serialize_main_metrics(list: &[(String, bool)]) -> String {
    list.iter()
        .map(|(n, on)| if *on { n.clone() } else { format!("-{}", n) })
        .collect::<Vec<_>>()
        .join(",")
}

/// Move the metric at `from` so it sits at `to`, shifting the rest along.
/// Out-of-range indices leave the list untouched rather than panicking, since
/// this is driven by a drag whose drop target is computed from mouse position.
pub fn reorder_main_metrics(list: &mut Vec<(String, bool)>, from: usize, to: usize) {
    if from >= list.len() || to >= list.len() || from == to {
        return;
    }
    let item = list.remove(from);
    list.insert(to, item);
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tray_static: false,
            tray_cpu: true,
            tray_ram: true,
            tray_disk: false,
            tray_net: false,
            tray_fps: false,
            interval_ms: 1000,
            mcp_enabled: true,
            notify_presets: NOTIFY_FINISHED | NOTIFY_ERRORS,
            notify_custom: Vec::new(),
            pinned: false,
            on_top: false,
            win_x: -1,
            win_y: -1,
            win_w: -1,
            win_h: -1,
            fps_overlay: false,
            fps_color: 0,
            fps_opacity: 1,
            fps_x: -1,
            fps_y: -1,
            theme: 0,
            text_size: 1,
            widget_on: false,
            widget_mask: 1 | 2 | 4,
            main_metrics: MAIN_METRICS.iter().map(|m| (m.to_string(), true)).collect(),
            agent_log_file: String::new(),
            widget_x: -1,
            widget_y: -1,
            widget_theme: 0,
            widget_scale: 1.0,
            fly_x: -1,
            fly_y: -1,
            rule_lines: Vec::new(),
        }
    }
}

impl Settings {
    /// True if no tray icon is selected; caller should force the static icon
    /// so the app stays reachable.
    pub fn no_tray_icon(&self) -> bool {
        !(self.tray_static
            || self.tray_cpu
            || self.tray_ram
            || self.tray_disk
            || self.tray_net
            || self.tray_fps)
    }

    /// The guidance handed to connected AI tools, composed from the presets
    /// and the free-text box. Returned both in the MCP `initialize`
    /// instructions and from the `notify_rules` tool.
    ///
    /// Empty string when the user has asked for nothing, so callers can tell
    /// "no preferences" from "preferences that happen to be quiet".
    pub fn ai_instructions(&self) -> String {
        let mut whens: Vec<&str> = Vec::new();
        if self.notify_presets & NOTIFY_FINISHED != 0 {
            whens.push("a build, test run or other long-running task finishes");
        }
        if self.notify_presets & NOTIFY_ERRORS != 0 {
            whens.push("something errors, fails or needs attention");
        }
        if self.notify_presets & NOTIFY_INPUT != 0 {
            whens.push("you need their input to continue");
        }
        if self.notify_presets & NOTIFY_VERBOSE != 0 {
            whens.push("you finish any step of note, even a small one");
        }
        let extra: Vec<&str> = self
            .notify_custom
            .iter()
            .filter(|(on, t)| *on && !t.trim().is_empty())
            .map(|(_, t)| t.trim())
            .collect();
        if whens.is_empty() && extra.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "The user of this machine has set notification preferences in \
             resourcemonitor.app. Use the `notify` tool to reach their desktop when:\n",
        );
        for w in &whens {
            out.push_str("  - ");
            out.push_str(w);
            out.push('\n');
        }
        if whens.is_empty() {
            out.push_str("  (no specific triggers selected)\n");
        }
        if !extra.is_empty() {
            out.push_str("Additional instructions from the user, follow them exactly:\n");
            for e in &extra {
                out.push_str("  - ");
                out.push_str(e);
                out.push('\n');
            }
        }
        out.push_str(
            "Also call `report_agents` when you start, finish or change what your \
             sub-agents are doing, so the user can see current activity in the app. \
             Send the full current list each time; it replaces the previous one.",
        );
        out
    }

    pub fn parse(text: &str) -> Self {
        let mut s = Settings::default();
        // Absent is not the same as 1.0 here: a file written before the widget
        // had its own size was drawn at the panel's text size, so the value has
        // to be seeded from that once the whole file is read. Resolved below,
        // because `text_size=` may appear after `widget_scale=`.
        let mut widget_scale: Option<f32> = None;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let b = v == "1" || v.eq_ignore_ascii_case("true");
            match k {
                "tray_static" => s.tray_static = b,
                "tray_cpu" => s.tray_cpu = b,
                "tray_ram" => s.tray_ram = b,
                "tray_disk" => s.tray_disk = b,
                "tray_net" => s.tray_net = b,
                "tray_fps" => s.tray_fps = b,
                "interval_ms" => {
                    if let Ok(n) = v.parse::<u32>() {
                        s.interval_ms = n.clamp(250, 10_000);
                    }
                }
                // "logging" is swallowed, not read: older files carry
                // logging=0/1 from the removed global switch. Honouring it
                // would silently swallow every alert, and letting it fall
                // through to the log<N> catch-all below would turn it into a
                // phantom alert rule.
                "logging" => {}
                "mcp_enabled" => s.mcp_enabled = b,
                "notify_presets" => s.notify_presets = v.parse().unwrap_or(3),
                // Legacy single free-text box, migrated to one list entry.
                "notify_text" if !v.is_empty() => {
                    s.notify_custom.push((true, unescape_value(v)))
                }
                k if k.starts_with("notify_rule") && !v.is_empty() => {
                    let (on, text) = v.split_once('|').unwrap_or(("1", v));
                    let text = unescape_value(text);
                    if !text.trim().is_empty() {
                        s.notify_custom.push((on != "0", text));
                    }
                }
                "pinned" => s.pinned = b,
                "on_top" => s.on_top = b,
                "win_x" => s.win_x = v.parse().unwrap_or(-1),
                "win_y" => s.win_y = v.parse().unwrap_or(-1),
                "win_w" => s.win_w = v.parse().unwrap_or(-1),
                "win_h" => s.win_h = v.parse().unwrap_or(-1),
                "fps_overlay" => s.fps_overlay = b,
                "fps_color" => s.fps_color = v.parse().unwrap_or(0),
                "fps_opacity" => s.fps_opacity = v.parse().unwrap_or(1),
                "fps_x" => s.fps_x = v.parse().unwrap_or(-1),
                "fps_y" => s.fps_y = v.parse().unwrap_or(-1),
                "theme" => s.theme = v.parse().unwrap_or(0),
                "text_size" => {
                    s.text_size = v.parse().unwrap_or(1).min(TEXT_SIZES.len() as u32 - 1)
                }
                "widget_on" => s.widget_on = b,
                "widget_mask" => s.widget_mask = v.parse().unwrap_or(7),
                "main_metrics" => s.main_metrics = parse_main_metrics(v),
                "agent_log_file" => s.agent_log_file = unescape_value(v),
                "widget_x" => s.widget_x = v.parse().unwrap_or(-1),
                "widget_y" => s.widget_y = v.parse().unwrap_or(-1),
                "widget_theme" => s.widget_theme = v.parse().unwrap_or(0),
                "widget_scale" => widget_scale = v.parse::<f32>().ok().map(clamp_widget_scale),
                "fly_x" => s.fly_x = v.parse().unwrap_or(-1),
                "fly_y" => s.fly_y = v.parse().unwrap_or(-1),
                k if k.starts_with("log") && !v.is_empty() => {
                    s.rule_lines.push(v.to_string());
                }
                _ => {}
            }
        }
        s.widget_scale = widget_scale.unwrap_or_else(|| clamp_widget_scale(text_scale(s.text_size)));
        s
    }

    pub fn serialize(&self) -> String {
        let mut out = format!(
            "tray_static={}\ntray_cpu={}\ntray_ram={}\ntray_disk={}\ntray_net={}\ntray_fps={}\n\
             interval_ms={}\npinned={}\non_top={}\nwin_x={}\nwin_y={}\nwin_w={}\nwin_h={}\n",
            self.tray_static as u8,
            self.tray_cpu as u8,
            self.tray_ram as u8,
            self.tray_disk as u8,
            self.tray_net as u8,
            self.tray_fps as u8,
            self.interval_ms,
            self.pinned as u8,
            // mcp_enabled appended below to keep field order stable.
            self.on_top as u8,
            self.win_x,
            self.win_y,
            self.win_w,
            self.win_h,
        );
        out.push_str(&format!(
            "fps_overlay={}\nfps_color={}\nfps_opacity={}\nfps_x={}\nfps_y={}\n",
            self.fps_overlay as u8, self.fps_color, self.fps_opacity, self.fps_x, self.fps_y
        ));
        out.push_str(&format!(
            "theme={}\nwidget_on={}\nwidget_mask={}\nwidget_x={}\nwidget_y={}\nmcp_enabled={}\n\
             notify_presets={}\n",
            self.theme,
            self.widget_on as u8,
            self.widget_mask,
            self.widget_x,
            self.widget_y,
            self.mcp_enabled as u8,
            self.notify_presets
        ));
        out.push_str(&format!("text_size={}\n", self.text_size));
        out.push_str(&format!(
            "widget_scale={:.3}\nwidget_theme={}\nfly_x={}\nfly_y={}\n",
            self.widget_scale, self.widget_theme, self.fly_x, self.fly_y
        ));
        out.push_str(&format!(
            "main_metrics={}\nagent_log_file={}\n",
            serialize_main_metrics(&self.main_metrics),
            escape_value(&self.agent_log_file)
        ));
        for (i, (on, text)) in self.notify_custom.iter().enumerate() {
            out.push_str(&format!(
                "notify_rule{}={}|{}\n",
                i + 1,
                *on as u8,
                escape_value(text)
            ));
        }
        for (i, line) in self.rule_lines.iter().enumerate() {
            out.push_str(&format!("log{}={}\n", i + 1, line));
        }
        out
    }
}

#[cfg(windows)]
fn path() -> Option<std::path::PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|p| std::path::PathBuf::from(p).join("resmon.ini"))
}

#[cfg(windows)]
pub fn load() -> Settings {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| Settings::parse(&t))
        .unwrap_or_default()
}

#[cfg(windows)]
pub fn save(s: &Settings) {
    if let Some(p) = path() {
        let _ = std::fs::write(p, s.serialize());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut s = Settings::default();
        s.tray_fps = true;
        s.tray_cpu = false;
        s.interval_ms = 500;
        s.pinned = true;
        s.win_x = 120;
        s.win_w = 480;
        s.notify_presets = NOTIFY_ERRORS | NOTIFY_INPUT;
        s.notify_custom = vec![
            (true, "Quiet before 9am".to_string()),
            (false, "Always name the repo".to_string()),
        ];
        s.rule_lines = vec![
            "cpu>90; file=C:\\logs\\a.log; data=top".to_string(),
            "fps<30; file=C:\\logs\\fps.log".to_string(),
        ];
        s.text_size = 3;
        s.widget_scale = 1.25;
        s.widget_theme = 2;
        s.fly_x = 220;
        s.fly_y = 640;
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn widget_scale_is_seeded_from_text_size_when_absent() {
        // Upgrading must not resize anyone's widget: before it had its own
        // size it was drawn at the panel's text size, so that is where an old
        // file has to land.
        assert_eq!(Settings::parse("text_size=2\n").widget_scale, text_scale(2));
        assert_eq!(Settings::parse("").widget_scale, 1.0);
        // An explicit value wins over the seed, whichever order they appear in.
        assert_eq!(Settings::parse("widget_scale=1.5\ntext_size=3\n").widget_scale, 1.5);
        assert_eq!(Settings::parse("text_size=3\nwidget_scale=1.5\n").widget_scale, 1.5);
    }

    #[test]
    fn widget_scale_stays_in_range() {
        assert_eq!(Settings::parse("widget_scale=99\n").widget_scale, WIDGET_SCALE_MAX);
        assert_eq!(Settings::parse("widget_scale=0.01\n").widget_scale, WIDGET_SCALE_MIN);
        // Unparseable falls back to the seed rather than to zero, which would
        // otherwise make the widget vanish.
        assert_eq!(Settings::parse("widget_scale=oops\n").widget_scale, 1.0);
        assert_eq!(clamp_widget_scale(f32::NAN), 1.0);
        // Quantised, so a dragged size reloads bit-identical.
        assert_eq!(clamp_widget_scale(1.234_567), 1.235);
    }

    #[test]
    fn text_size_stays_in_range() {
        // An index past the end of the presets would otherwise silently mean
        // "no scaling", which looks like the setting being ignored.
        assert_eq!(Settings::parse("text_size=99\n").text_size, TEXT_SIZES.len() as u32 - 1);
        assert_eq!(Settings::parse("text_size=oops\n").text_size, 1);
        assert_eq!(text_scale(Settings::default().text_size), 1.0);
        assert!(text_scale(3) > text_scale(0));
    }

    #[test]
    fn parse_ignores_junk_and_clamps() {
        let s = Settings::parse("garbage\ninterval_ms=50\ntray_ram=true\nunknown=1\n");
        assert_eq!(s.interval_ms, 250); // clamped
        assert!(s.tray_ram);
        assert!(s.tray_cpu); // default preserved
    }

    #[test]
    fn notify_custom_survives_newlines_and_pipes() {
        // The ini is line-based and the flag is pipe-delimited, so both a
        // newline and a literal '|' in the user's text must survive.
        let mut s = Settings::default();
        s.notify_custom = vec![(
            true,
            "Don't notify before 9am.\nUse C:\\logs | the build log.".to_string(),
        )];
        let round = Settings::parse(&s.serialize());
        assert_eq!(round.notify_custom, s.notify_custom);
        assert_eq!(s.serialize().lines().filter(|l| l.starts_with("notify_rule")).count(), 1);
    }

    #[test]
    fn legacy_notify_text_migrates_to_a_list_entry() {
        let s = Settings::parse("notify_text=Quiet before 9am\n");
        assert_eq!(s.notify_custom, vec![(true, "Quiet before 9am".to_string())]);
        // ...and is not written back under the old key.
        assert!(!s.serialize().contains("notify_text="));
    }

    #[test]
    fn ai_instructions_empty_when_nothing_requested() {
        let mut s = Settings::default();
        s.notify_presets = 0;
        s.notify_custom = vec![(true, "   ".to_string())];
        assert_eq!(s.ai_instructions(), "");
    }

    #[test]
    fn ai_instructions_skip_disabled_custom_entries() {
        let mut s = Settings::default();
        s.notify_presets = NOTIFY_ERRORS;
        s.notify_custom = vec![
            (true, "Always name the repo.".to_string()),
            (false, "Shout at me.".to_string()),
        ];
        let out = s.ai_instructions();
        assert!(out.contains("errors, fails"));
        assert!(!out.contains("long-running task finishes"));
        assert!(out.contains("Always name the repo."));
        assert!(!out.contains("Shout at me."));
        // The agent-reporting ask rides along with any instructions at all.
        assert!(out.contains("report_agents"));
    }

    #[test]
    fn stale_logging_key_is_swallowed_whole() {
        // The global "save alerts to a file" switch is gone; delivery is per
        // alert. An old file carrying logging=0/1 must neither be persisted
        // back out nor read as anything.
        let s = Settings::parse("logging=0\n");
        assert!(!s.serialize().contains("logging="));
        // Nor may the stale key leak into the rule lines through the log<N>
        // catch-all: that would surface a phantom "(invalid) 0" alert rule on
        // upgrade and persist it forever as log1=0.
        assert!(s.rule_lines.is_empty());
        let s = Settings::parse("logging=1\n");
        assert!(s.rule_lines.is_empty());
        assert!(!s.serialize().contains("log1="));
    }

    #[test]
    fn empty_input_gives_defaults() {
        assert_eq!(Settings::parse(""), Settings::default());
    }

    #[test]
    fn main_metrics_keeps_order_and_visibility() {
        let m = parse_main_metrics("cpu,ram,fps,-gpu,disk,net,audio");
        let names: Vec<&str> = m.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["cpu", "ram", "fps", "gpu", "disk", "net", "audio"]);
        assert!(!m.iter().find(|(n, _)| n == "gpu").unwrap().1, "gpu should be hidden");
        assert!(m.iter().find(|(n, _)| n == "cpu").unwrap().1, "cpu should be visible");
    }

    #[test]
    fn main_metrics_appends_unmentioned_metrics_visible() {
        // Standing in for a metric added in a later version: a saved config
        // that predates it must not make it permanently invisible.
        let m = parse_main_metrics("cpu,ram");
        assert_eq!(m.len(), MAIN_METRICS.len());
        assert_eq!(m[0].0, "cpu");
        assert_eq!(m[1].0, "ram");
        for (name, visible) in m.iter().skip(2) {
            assert!(visible, "{} was appended hidden", name);
        }
    }

    #[test]
    fn main_metrics_drops_unknown_and_repeated() {
        let m = parse_main_metrics("cpu,bogus,cpu,ram,,-cpu");
        assert_eq!(m.iter().filter(|(n, _)| n == "cpu").count(), 1);
        assert!(!m.iter().any(|(n, _)| n == "bogus"));
        // The first mention wins, so the later "-cpu" cannot hide it.
        assert!(m.iter().find(|(n, _)| n == "cpu").unwrap().1);
        assert_eq!(m.len(), MAIN_METRICS.len());
    }

    #[test]
    fn main_metrics_empty_falls_back_to_default_order() {
        let m = parse_main_metrics("");
        let names: Vec<&str> = m.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, MAIN_METRICS);
        assert!(m.iter().all(|(_, on)| *on));
    }

    #[test]
    fn main_metrics_round_trip() {
        let src = "fps,-cpu,ram,gpu,disk,net,audio";
        let parsed = parse_main_metrics(src);
        assert_eq!(serialize_main_metrics(&parsed), src);
        // And through the whole settings file, not just the helper.
        let mut s = Settings::default();
        s.main_metrics = parsed.clone();
        assert_eq!(Settings::parse(&s.serialize()).main_metrics, parsed);
    }

    #[test]
    fn reorder_moves_items_including_the_ends() {
        let mut m = parse_main_metrics("");
        reorder_main_metrics(&mut m, 3, 0); // fps to the front
        assert_eq!(m[0].0, "fps");
        assert_eq!(m[1].0, "cpu");
        let last = m.len() - 1;
        reorder_main_metrics(&mut m, 0, last); // and back to the end
        assert_eq!(m[last].0, "fps");
        assert_eq!(m[0].0, "cpu");
    }

    #[test]
    fn reorder_ignores_out_of_range_and_noop_drops() {
        let mut m = parse_main_metrics("");
        let before = m.clone();
        reorder_main_metrics(&mut m, 99, 0);
        reorder_main_metrics(&mut m, 0, 99);
        reorder_main_metrics(&mut m, 2, 2);
        assert_eq!(m, before, "a drop outside the list must change nothing");
    }

    #[test]
    fn agent_log_file_round_trips_and_defaults_off() {
        assert_eq!(Settings::default().agent_log_file, "", "session-only by default");
        let mut s = Settings::default();
        s.agent_log_file = r"C:\Users\a b\AppData\Local\resmon-agents.log".to_string();
        assert_eq!(Settings::parse(&s.serialize()).agent_log_file, s.agent_log_file);
    }

    #[test]
    fn no_tray_icon_detection() {
        let mut s = Settings::default();
        assert!(!s.no_tray_icon());
        s.tray_cpu = false;
        s.tray_ram = false;
        assert!(s.no_tray_icon());
    }
}
