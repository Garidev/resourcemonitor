//! User-defined alert/log rules, declared in resmon.ini as lines like:
//!
//!   log1=cpu>90; file=C:\logs\alerts.log; data=top; cooldown=120
//!   log2=proc:chrome.exe:ram>1500; file=C:\logs\chrome.log
//!
//! Metrics: cpu, ram, gpu (%), disk, net (MB/s), fps,
//!          proc:<name>:<cpu|ram|disk|net>  (ram in MB, disk/net in MB/s)
//! data=top appends the top 5 processes by CPU to each log line.
//! cooldown (seconds, default 60) limits how often a rule can fire.
//! Parsing is pure and unit-tested; evaluation runs on the sampler tick.

#[derive(Clone, Debug, PartialEq)]
pub enum ProcSub {
    Cpu,
    RamMb,
    DiskMbs,
    NetMbs,
    SoundPct,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RMetric {
    Cpu,
    RamPct,
    Gpu,
    DiskMbs,
    NetMbs,
    Fps,
    SoundPct,
    Proc { name: String, sub: ProcSub },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub raw: String,
    pub metric: RMetric,
    pub gt: bool,
    pub threshold: f64,
    pub file: String,
    pub top: bool,
    pub cooldown_s: u64,
    /// A `; off` segment disables the rule without deleting it.
    pub enabled: bool,
    /// Pop a desktop notification when it triggers (a `; nonotify` segment
    /// turns this off, so existing rules default to notifying).
    pub notify: bool,
}

impl Rule {
    /// True if evaluating this rule requires the per-process snapshot.
    pub fn needs_procs(&self) -> bool {
        self.top || matches!(self.metric, RMetric::Proc { .. })
    }

    /// True if evaluating this rule requires GPU sampling.
    pub fn needs_gpu(&self) -> bool {
        matches!(self.metric, RMetric::Gpu)
    }

    pub fn triggered(&self, value: f64) -> bool {
        if self.gt {
            value > self.threshold
        } else {
            value < self.threshold
        }
    }
}

pub fn parse_all(lines: &[String]) -> Vec<Rule> {
    lines.iter().filter_map(|l| parse_line(l)).collect()
}

pub fn parse_line(line: &str) -> Option<Rule> {
    let mut cond = None;
    let mut file = None;
    let mut top = false;
    let mut cooldown_s = 60u64;
    let mut enabled = true;
    let mut notify = true;
    for (i, seg) in line.split(';').enumerate() {
        let seg = seg.trim();
        if i == 0 {
            cond = Some(seg.to_string());
            continue;
        }
        if let Some(v) = seg.strip_prefix("file=") {
            file = Some(v.trim().to_string());
        } else if let Some(v) = seg.strip_prefix("data=") {
            top = v.trim().eq_ignore_ascii_case("top");
        } else if let Some(v) = seg.strip_prefix("cooldown=") {
            cooldown_s = v.trim().parse().unwrap_or(60);
        } else if seg.eq_ignore_ascii_case("off") {
            enabled = false;
        } else if seg.eq_ignore_ascii_case("nonotify") {
            notify = false;
        }
    }
    let cond = cond?;
    // File is optional: a rule with no file simply notifies you.
    let file = file.unwrap_or_default();
    let (gt, op_pos) = match (cond.find('>'), cond.find('<')) {
        (Some(p), None) => (true, p),
        (None, Some(p)) => (false, p),
        _ => return None,
    };
    let metric_s = cond[..op_pos].trim().to_lowercase();
    let threshold: f64 = cond[op_pos + 1..].trim().parse().ok()?;
    let metric = if let Some(rest) = metric_s.strip_prefix("proc:") {
        let (name, sub) = rest.rsplit_once(':')?;
        let sub = match sub {
            "cpu" => ProcSub::Cpu,
            "ram" => ProcSub::RamMb,
            "disk" => ProcSub::DiskMbs,
            "net" => ProcSub::NetMbs,
            "sound" => ProcSub::SoundPct,
            _ => return None,
        };
        RMetric::Proc { name: name.to_string(), sub }
    } else {
        match metric_s.as_str() {
            "cpu" => RMetric::Cpu,
            "ram" => RMetric::RamPct,
            "gpu" => RMetric::Gpu,
            "disk" => RMetric::DiskMbs,
            "net" => RMetric::NetMbs,
            "fps" => RMetric::Fps,
            "sound" => RMetric::SoundPct,
            _ => return None,
        }
    };
    Some(Rule { raw: line.to_string(), metric, gt, threshold, file, top, cooldown_s, enabled, notify })
}

/// Build a rule line from parts (the GUI editor uses this).
pub fn build_line(
    metric: &str,
    gt: bool,
    threshold: f64,
    file: &str,
    notify: bool,
    top: bool,
    cooldown_s: u64,
) -> String {
    let mut line = format!("{}{}{}", metric, if gt { '>' } else { '<' }, threshold);
    if !file.trim().is_empty() {
        line.push_str(&format!("; file={}", file.trim()));
    }
    if top {
        line.push_str("; data=top");
    }
    if cooldown_s != 60 {
        line.push_str(&format!("; cooldown={}", cooldown_s));
    }
    if !notify {
        line.push_str("; nonotify");
    }
    line
}

/// Toggle the `; off` segment on a raw rule line.
pub fn set_enabled(line: &str, enabled: bool) -> String {
    let base: Vec<&str> = line
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("off"))
        .collect();
    let mut out = base.join("; ");
    if !enabled {
        out.push_str("; off");
    }
    out
}

/// Short human-readable summary for list rows, e.g. "cpu>90 → alerts.log".
pub fn summary(rule: &Rule) -> String {
    let cond = rule.raw.split(';').next().unwrap_or("?").trim();
    let file = rule.file.rsplit(['\\', '/']).next().unwrap_or(&rule.file);
    let dest = match (rule.notify, rule.file.trim().is_empty()) {
        (true, true) => "notify".to_string(),
        (true, false) => format!("notify + {}", file),
        (false, false) => file.to_string(),
        (false, true) => "nothing".to_string(),
    };
    format!("{} → {}", cond, dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_rule() {
        let r = parse_line("cpu>90; file=C:\\logs\\a.log; data=top; cooldown=120").unwrap();
        assert_eq!(r.metric, RMetric::Cpu);
        assert!(r.gt);
        assert_eq!(r.threshold, 90.0);
        assert_eq!(r.file, "C:\\logs\\a.log");
        assert!(r.top);
        assert_eq!(r.cooldown_s, 120);
        assert!(r.needs_procs());
    }

    #[test]
    fn parses_proc_rule_and_less_than() {
        let r = parse_line("proc:chrome.exe:ram>1500; file=c.log").unwrap();
        assert_eq!(
            r.metric,
            RMetric::Proc { name: "chrome.exe".into(), sub: ProcSub::RamMb }
        );
        assert!(r.needs_procs());
        let r2 = parse_line("fps<30; file=f.log").unwrap();
        assert!(!r2.gt);
        assert_eq!(r2.metric, RMetric::Fps);
        assert!(!r2.needs_procs());
    }

    #[test]
    fn file_is_optional() {
        // A rule with no file is valid: it notifies without logging.
        let r = parse_line("cpu>90").unwrap();
        assert_eq!(r.metric, RMetric::Cpu);
        assert!(r.file.is_empty());
        assert_eq!(summary(&r), "cpu>90 → notify");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_line("bogus>1; file=a.log").is_none());
        assert!(parse_line("cpu=90; file=a.log").is_none()); // bad operator
        assert!(parse_line("proc:x:teleport>1; file=a.log").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn enable_disable_roundtrip() {
        let line = "cpu>90; file=a.log; data=top";
        let off = set_enabled(line, false);
        let r = parse_line(&off).unwrap();
        assert!(!r.enabled);
        assert!(r.top);
        let on = set_enabled(&off, true);
        let r2 = parse_line(&on).unwrap();
        assert!(r2.enabled);
        assert_eq!(on, "cpu>90; file=a.log; data=top");
    }

    #[test]
    fn build_line_matches_parser() {
        let line = build_line("proc:chrome.exe:ram", true, 1500.0, "C:\\l\\c.log", true, true, 300);
        let r = parse_line(&line).unwrap();
        assert_eq!(
            r.metric,
            RMetric::Proc { name: "chrome.exe".into(), sub: ProcSub::RamMb }
        );
        assert_eq!(r.cooldown_s, 300);
        assert!(r.top && r.enabled);
        assert_eq!(summary(&r), "proc:chrome.exe:ram>1500 → notify + c.log");
    }

    #[test]
    fn trigger_directions() {
        let hi = parse_line("cpu>90; file=a.log").unwrap();
        assert!(hi.triggered(95.0));
        assert!(!hi.triggered(45.0));
        let lo = parse_line("fps<30; file=a.log").unwrap();
        assert!(lo.triggered(20.0));
        assert!(!lo.triggered(60.0));
    }
}
