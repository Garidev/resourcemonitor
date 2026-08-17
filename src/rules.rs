//! User-defined alert/log rules, declared in resmon.ini as lines like:
//!
//!   log1=cpu>90; file=C:\logs\alerts.log; data=top; cooldown=120
//!   log2=proc:chrome.exe:ram>1500; file=C:\logs\chrome.log
//!   log3=conn:host=*.asus.com; cooldown=300
//!   log4=conn:port=445; file=C:\logs\smb.log
//!
//! Two kinds of condition:
//!
//!  - a threshold on a number sampled every tick —
//!    cpu, ram, gpu (%), disk, net (MB/s), fps,
//!    proc:<name>:<cpu|ram|disk|net>  (ram in MB, disk/net in MB/s);
//!  - a match against a network connection —
//!    conn:host=<pattern>, conn:ip=<prefix>, conn:port=<n>, conn:proc=<name>.
//!
//! The two are genuinely different shapes, not one shape with unused fields:
//! a threshold is compared against a value, a connection rule fires the moment
//! something matching appears. Keeping numeric fields on a match rule would
//! leave `gt` and `threshold` holding meaningless values that some later
//! reader would inevitably trust.
//!
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

/// Which part of a connection a rule looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnField {
    /// Resolved hostname: substring, or `*` wildcards to anchor.
    Host,
    /// Remote address, matched as a prefix so "204.79." covers the range.
    Ip,
    /// Port at either end.
    Port,
    /// Image name of the process that owns the connection.
    Process,
}

impl ConnField {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConnField::Host => "host",
            ConnField::Ip => "ip",
            ConnField::Port => "port",
            ConnField::Process => "proc",
        }
    }

    pub fn parse(s: &str) -> Option<ConnField> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "host" | "hostname" | "dns" => ConnField::Host,
            "ip" | "address" => ConnField::Ip,
            "port" => ConnField::Port,
            "proc" | "process" | "app" => ConnField::Process,
            _ => return None,
        })
    }

    /// How the rule list and the alert read it out.
    pub fn label(&self) -> &'static str {
        match self {
            ConnField::Host => "hostname",
            ConnField::Ip => "remote IP",
            ConnField::Port => "port",
            ConnField::Process => "app",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cond {
    /// Fires while a sampled number is above or below a threshold.
    Threshold { metric: RMetric, gt: bool, threshold: f64 },
    /// Fires when a connection matching `pattern` appears, or when a DNS
    /// lookup matching it is observed.
    Conn { field: ConnField, pattern: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub raw: String,
    pub cond: Cond,
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
    /// Connection rules count: without process names an alert could only say
    /// that *something* on this machine reached the endpoint.
    pub fn needs_procs(&self) -> bool {
        self.top
            || matches!(&self.cond, Cond::Threshold { metric: RMetric::Proc { .. }, .. })
            || self.needs_conns()
    }

    /// True if evaluating this rule requires GPU sampling.
    pub fn needs_gpu(&self) -> bool {
        matches!(&self.cond, Cond::Threshold { metric: RMetric::Gpu, .. })
    }

    /// True if this rule needs the connection sweep running. An armed rule is
    /// the only thing that keeps it on while the panel is closed.
    pub fn needs_conns(&self) -> bool {
        matches!(&self.cond, Cond::Conn { .. })
    }

    /// The threshold test. Meaningless for a connection rule, which has no
    /// value to compare, so it never claims to have triggered.
    pub fn triggered(&self, value: f64) -> bool {
        match &self.cond {
            Cond::Threshold { gt, threshold, .. } => {
                if *gt {
                    value > *threshold
                } else {
                    value < *threshold
                }
            }
            Cond::Conn { .. } => false,
        }
    }

    pub fn metric(&self) -> Option<&RMetric> {
        match &self.cond {
            Cond::Threshold { metric, .. } => Some(metric),
            Cond::Conn { .. } => None,
        }
    }

    pub fn conn(&self) -> Option<(ConnField, &str)> {
        match &self.cond {
            Cond::Conn { field, pattern } => Some((*field, pattern.as_str())),
            Cond::Threshold { .. } => None,
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

    // Connection rules are matches, so they carry no operator at all and are
    // recognised before anything looks for one.
    if let Some(rest) = cond.trim().strip_prefix("conn:") {
        let (field, pattern) = rest.split_once('=')?;
        let field = ConnField::parse(field)?;
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return None;
        }
        // A port that is not a number would match nothing, forever, silently.
        if field == ConnField::Port && pattern.parse::<u16>().is_err() {
            return None;
        }
        return Some(Rule {
            raw: line.to_string(),
            cond: Cond::Conn { field, pattern: pattern.to_string() },
            file,
            top,
            cooldown_s,
            enabled,
            notify,
        });
    }

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
    Some(Rule {
        raw: line.to_string(),
        cond: Cond::Threshold { metric, gt, threshold },
        file,
        top,
        cooldown_s,
        enabled,
        notify,
    })
}

/// Build a connection rule line from parts (the GUI editor uses this).
pub fn build_conn_line(
    field: ConnField,
    pattern: &str,
    file: &str,
    notify: bool,
    top: bool,
    cooldown_s: u64,
) -> String {
    // Semicolons separate segments, so one inside a pattern would split the
    // rule in half when it was read back.
    let pattern = pattern.trim().replace(';', "");
    let mut line = format!("conn:{}={}", field.as_str(), pattern);
    append_options(&mut line, file, notify, top, cooldown_s);
    line
}

/// The trailing `; key=value` segments, identical for both kinds of rule.
fn append_options(line: &mut String, file: &str, notify: bool, top: bool, cooldown_s: u64) {
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
    append_options(&mut line, file, notify, top, cooldown_s);
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

/// Does this connection rule match a live connection?
///
/// Ports are compared at both ends: "who is on 445" should find the machine
/// connecting out to a share and the machine serving one.
pub fn conn_matches(field: ConnField, pattern: &str, row: &crate::conns::Row) -> bool {
    use crate::conns;
    match field {
        ConnField::Host => row
            .host
            .as_deref()
            .map_or(false, |h| conns::host_matches(pattern, h)),
        ConnField::Ip => row
            .conn
            .remote_ip()
            .map_or(false, |ip| conns::ip_matches(pattern, &ip)),
        ConnField::Port => match pattern.parse::<u16>() {
            Ok(p) => row.conn.remote_port() == Some(p) || row.conn.local_port == p,
            Err(_) => false,
        },
        ConnField::Process => conns::glob_match(pattern, &row.process),
    }
}

/// Does this connection rule match an observed DNS lookup?
///
/// A lookup is the earliest sign that something is about to talk to an
/// endpoint, and it catches beacons that open and close between two sweeps.
/// A port rule has nothing to compare against here, so it never matches.
pub fn dns_matches(
    field: ConnField,
    pattern: &str,
    host: &str,
    addrs: &[std::net::IpAddr],
    process: &str,
) -> bool {
    use crate::conns;
    match field {
        ConnField::Host => conns::host_matches(pattern, host),
        ConnField::Ip => addrs.iter().any(|ip| conns::ip_matches(pattern, ip)),
        ConnField::Process => conns::glob_match(pattern, process),
        ConnField::Port => false,
    }
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
        assert_eq!(
            r.cond,
            Cond::Threshold { metric: RMetric::Cpu, gt: true, threshold: 90.0 }
        );
        assert_eq!(r.file, "C:\\logs\\a.log");
        assert!(r.top);
        assert_eq!(r.cooldown_s, 120);
        assert!(r.needs_procs());
        assert!(!r.needs_conns());
    }

    #[test]
    fn parses_proc_rule_and_less_than() {
        let r = parse_line("proc:chrome.exe:ram>1500; file=c.log").unwrap();
        assert_eq!(
            r.metric(),
            Some(&RMetric::Proc { name: "chrome.exe".into(), sub: ProcSub::RamMb })
        );
        assert!(r.needs_procs());
        let r2 = parse_line("fps<30; file=f.log").unwrap();
        assert!(r2.triggered(20.0) && !r2.triggered(60.0), "reads as 'below 30'");
        assert_eq!(r2.metric(), Some(&RMetric::Fps));
        assert!(!r2.needs_procs());
    }

    #[test]
    fn file_is_optional() {
        // A rule with no file is valid: it notifies without logging.
        let r = parse_line("cpu>90").unwrap();
        assert_eq!(r.metric(), Some(&RMetric::Cpu));
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
            r.metric(),
            Some(&RMetric::Proc { name: "chrome.exe".into(), sub: ProcSub::RamMb })
        );
        assert_eq!(r.cooldown_s, 300);
        assert!(r.top && r.enabled);
        assert_eq!(summary(&r), "proc:chrome.exe:ram>1500 → notify + c.log");
    }

    fn conn_row(process: &str, remote: &str, port: u16, host: Option<&str>) -> crate::conns::Row {
        use crate::conns::{Conn, NameSource, Proto, Row, TCP_ESTABLISHED};
        Row {
            conn: Conn {
                pid: 4828,
                proto: Proto::Tcp,
                local_ip: "192.168.1.10".parse().unwrap(),
                local_port: 51000,
                remote: Some((remote.parse().unwrap(), port)),
                state: TCP_ESTABLISHED,
            },
            process: process.to_string(),
            host: host.map(|h| h.to_string()),
            name_source: host.map(|_| NameSource::DnsEvent),
            name_pid: None,
        }
    }

    #[test]
    fn parses_every_connection_field() {
        for (line, field, pattern) in [
            ("conn:host=*.asus.com", ConnField::Host, "*.asus.com"),
            ("conn:ip=204.79.", ConnField::Ip, "204.79."),
            ("conn:port=445", ConnField::Port, "445"),
            ("conn:proc=mscopilot.exe", ConnField::Process, "mscopilot.exe"),
        ] {
            let r = parse_line(line).unwrap();
            assert_eq!(r.conn(), Some((field, pattern)), "{}", line);
            assert!(r.needs_conns(), "{}", line);
            // A connection rule needs process names to say who connected.
            assert!(r.needs_procs(), "{}", line);
            assert!(r.metric().is_none(), "{}", line);
        }
    }

    #[test]
    fn connection_rules_carry_the_usual_options() {
        let r = parse_line("conn:host=asus.com; file=C:\\logs\\a.log; cooldown=300; nonotify").unwrap();
        assert_eq!(r.file, "C:\\logs\\a.log");
        assert_eq!(r.cooldown_s, 300);
        assert!(!r.notify);
        assert!(r.enabled);
    }

    #[test]
    fn a_connection_rule_never_claims_a_threshold_triggered() {
        // It has no value to compare, so no value may be read as a trigger.
        let r = parse_line("conn:port=445").unwrap();
        assert!(!r.triggered(0.0));
        assert!(!r.triggered(f64::MAX));
    }

    #[test]
    fn rejects_malformed_connection_rules() {
        assert!(parse_line("conn:").is_none());
        assert!(parse_line("conn:host=").is_none(), "an empty pattern matches everything");
        assert!(parse_line("conn:bogus=x").is_none());
        // A port that is not a number would silently match nothing forever.
        assert!(parse_line("conn:port=https").is_none());
        assert!(parse_line("conn:port=99999").is_none());
    }

    #[test]
    fn connection_rule_survives_the_enable_toggle() {
        let line = "conn:host=*.asus.com; file=a.log";
        let off = set_enabled(line, false);
        let r = parse_line(&off).unwrap();
        assert!(!r.enabled);
        assert_eq!(r.conn(), Some((ConnField::Host, "*.asus.com")));
        assert_eq!(set_enabled(&off, true), line);
    }

    #[test]
    fn build_conn_line_matches_parser() {
        let line = build_conn_line(ConnField::Host, "*.asus.com", "C:\\l\\a.log", false, false, 300);
        let r = parse_line(&line).unwrap();
        assert_eq!(r.conn(), Some((ConnField::Host, "*.asus.com")));
        assert_eq!(r.file, "C:\\l\\a.log");
        assert_eq!(r.cooldown_s, 300);
        assert!(!r.notify);
        assert_eq!(summary(&r), "conn:host=*.asus.com → a.log");
    }

    #[test]
    fn a_semicolon_in_a_pattern_cannot_split_the_rule() {
        // Semicolons separate segments; one inside a pattern would turn the
        // rest of the pattern into an options segment on the way back in.
        let line = build_conn_line(ConnField::Host, "evil;file=C:\\x.log", "", true, false, 60);
        let r = parse_line(&line).unwrap();
        assert_eq!(r.conn(), Some((ConnField::Host, "evilfile=C:\\x.log")));
        assert!(r.file.is_empty(), "no file was ever configured for this rule");
    }

    #[test]
    fn matches_connections_by_each_field() {
        let row = conn_row("msedgewebview2.exe", "204.79.197.222", 443, Some("bing.com"));
        assert!(conn_matches(ConnField::Host, "bing", &row));
        assert!(conn_matches(ConnField::Host, "*.com", &row));
        assert!(!conn_matches(ConnField::Host, "asus.com", &row));
        assert!(conn_matches(ConnField::Ip, "204.79.", &row));
        assert!(!conn_matches(ConnField::Ip, "10.", &row));
        assert!(conn_matches(ConnField::Port, "443", &row));
        assert!(!conn_matches(ConnField::Port, "80", &row));
        assert!(conn_matches(ConnField::Process, "msedge*", &row));
        assert!(!conn_matches(ConnField::Process, "chrome.exe", &row));
    }

    #[test]
    fn a_host_rule_ignores_connections_it_could_not_name() {
        // Firing on an unnamed row would report a match nobody can verify.
        let unnamed = conn_row("svchost.exe", "20.86.94.139", 443, None);
        assert!(!conn_matches(ConnField::Host, "asus.com", &unnamed));
    }

    #[test]
    fn a_port_rule_watches_both_ends() {
        let outbound = conn_row("explorer.exe", "192.168.1.5", 445, None);
        assert!(conn_matches(ConnField::Port, "445", &outbound));
        let mut inbound = conn_row("System", "192.168.1.5", 51000, None);
        inbound.conn.local_port = 445;
        assert!(conn_matches(ConnField::Port, "445", &inbound));
    }

    #[test]
    fn matches_dns_lookups() {
        let addrs: Vec<std::net::IpAddr> = vec!["104.20.1.5".parse().unwrap()];
        assert!(dns_matches(ConnField::Host, "asus.com", "mymessage.asus.com", &addrs, "asus.exe"));
        assert!(dns_matches(ConnField::Ip, "104.20.", "mymessage.asus.com", &addrs, "asus.exe"));
        assert!(dns_matches(ConnField::Process, "asus*", "mymessage.asus.com", &addrs, "asus.exe"));
        assert!(!dns_matches(ConnField::Host, "bing", "mymessage.asus.com", &addrs, "asus.exe"));
        // A lookup has no port, so a port rule waits for the connection.
        assert!(!dns_matches(ConnField::Port, "443", "mymessage.asus.com", &addrs, "asus.exe"));
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
