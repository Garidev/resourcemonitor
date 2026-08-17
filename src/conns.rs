//! Live network connections: which process is talking to which endpoint.
//!
//! Two halves that meet in `Row`:
//!  - the connection table itself (`sweep`), from GetExtendedTcp/UdpTable —
//!    pid, protocol, local and remote endpoint, TCP state;
//!  - a name map (`NameMap`), filled from Microsoft-Windows-DNS-Client ETW
//!    events (see `etw.rs`) and from reverse lookups, which turns a remote
//!    address back into the name the machine actually asked for.
//!
//! Nothing here judges a connection. It reports what is open and where the
//! name came from (`NameSource`); deciding whether svchost should be talking
//! to a Delivery Optimization host is left to the person reading it.
//!
//! The enumeration is Windows-only; the classification, filtering and DNS
//! payload parsing are plain Rust so they can be unit-tested anywhere.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

// --------------------------------------------------------------- data model

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn as_str(&self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// MIB_TCP_STATE values. UDP rows carry `UDP_STATE`, which is not a TCP state
/// at all — it exists so one enum can describe every row.
pub const TCP_LISTEN: u32 = 2;
pub const TCP_ESTABLISHED: u32 = 5;
pub const UDP_STATE: u32 = 0;

pub fn state_name(state: u32) -> &'static str {
    match state {
        UDP_STATE => "",
        1 => "closed",
        TCP_LISTEN => "listening",
        3 => "syn_sent",
        4 => "syn_received",
        TCP_ESTABLISHED => "established",
        6 => "fin_wait1",
        7 => "fin_wait2",
        8 => "close_wait",
        9 => "closing",
        10 => "last_ack",
        11 => "time_wait",
        12 => "delete_tcb",
        _ => "unknown",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conn {
    pub pid: u32,
    pub proto: Proto,
    pub local_ip: IpAddr,
    pub local_port: u16,
    /// None for UDP (connectionless) and for sockets that are only listening.
    pub remote: Option<(IpAddr, u16)>,
    /// TCP state, or `UDP_STATE` for UDP rows.
    pub state: u32,
}

impl Conn {
    pub fn remote_ip(&self) -> Option<IpAddr> {
        self.remote.map(|(ip, _)| ip)
    }
    pub fn remote_port(&self) -> Option<u16> {
        self.remote.map(|(_, p)| p)
    }
}

/// The last sweep. Kept in its own lock rather than in `Snapshot`, which is
/// cloned on every panel paint and every MCP request: a machine-wide
/// connection table is hundreds of rows, and most views never read it.
#[derive(Clone, Debug, Default)]
pub struct ConnTable {
    pub rows: Vec<Conn>,
    /// Unix ms of the sweep, or 0 if we have never swept.
    pub swept_ms: u64,
}

/// A connection joined with the things that make it readable: the image name
/// of the owning process and the hostname the address resolved from.
#[derive(Clone, Debug)]
pub struct Row {
    pub conn: Conn,
    pub process: String,
    pub host: Option<String>,
    pub name_source: Option<NameSource>,
    /// The process that resolved the name, when a DNS event told us. Equal to
    /// `conn.pid` means this process asked for this name itself; a different
    /// pid means the name is only a machine-wide association for the address,
    /// which is a weaker claim and should read as one.
    pub name_pid: Option<u32>,
}

// ------------------------------------------------------------ classification

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Loopback,
    LinkLocal,
    Private,
    Multicast,
    Public,
    Unspecified,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Loopback => "loopback",
            Scope::LinkLocal => "link_local",
            Scope::Private => "private",
            Scope::Multicast => "multicast",
            Scope::Public => "public",
            Scope::Unspecified => "unspecified",
        }
    }
    /// Everything that never leaves the machine or the LAN. The MCP tool
    /// defaults to public-only because that is the question people ask.
    pub fn is_local(&self) -> bool {
        !matches!(self, Scope::Public)
    }
}

pub fn scope_of(ip: &IpAddr) -> Scope {
    match ip {
        IpAddr::V4(v4) => scope_v4(v4),
        IpAddr::V6(v6) => {
            // A v4-mapped address (::ffff:a.b.c.d) is a v4 address wearing a
            // v6 hat; classifying it as "public v6" would be wrong.
            if let Some(v4) = v6_as_v4(v6) {
                return scope_v4(&v4);
            }
            let seg = v6.segments();
            if v6.is_loopback() {
                Scope::Loopback
            } else if v6.is_unspecified() {
                Scope::Unspecified
            } else if seg[0] & 0xffc0 == 0xfe80 {
                Scope::LinkLocal
            } else if seg[0] & 0xfe00 == 0xfc00 {
                // Unique local (fc00::/7) — the v6 equivalent of 10/8.
                Scope::Private
            } else if seg[0] & 0xff00 == 0xff00 {
                Scope::Multicast
            } else {
                Scope::Public
            }
        }
    }
}

fn scope_v4(ip: &Ipv4Addr) -> Scope {
    let o = ip.octets();
    match o {
        [0, 0, 0, 0] => Scope::Unspecified,
        [127, ..] => Scope::Loopback,
        [169, 254, ..] => Scope::LinkLocal,
        [10, ..] => Scope::Private,
        [172, b, ..] if (16..32).contains(&b) => Scope::Private,
        [192, 168, ..] => Scope::Private,
        // Carrier-grade NAT: not routable on the public internet either.
        [100, b, ..] if (64..128).contains(&b) => Scope::Private,
        [255, 255, 255, 255] => Scope::Multicast,
        [a, ..] if (224..240).contains(&a) => Scope::Multicast,
        _ => Scope::Public,
    }
}

fn v6_as_v4(v6: &Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    if seg[..5] == [0, 0, 0, 0, 0] && seg[5] == 0xffff {
        let o = v6.octets();
        Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]))
    } else {
        None
    }
}

/// Well-known port → protocol name. Deliberately short: this is here so a
/// reader recognises 443 without a lookup, not to be a services database.
pub fn service_name(port: u16) -> Option<&'static str> {
    Some(match port {
        20 | 21 => "ftp",
        22 => "ssh",
        23 => "telnet",
        25 | 465 | 587 => "smtp",
        53 => "dns",
        67 | 68 => "dhcp",
        80 | 8080 => "http",
        110 => "pop3",
        123 => "ntp",
        135 => "rpc",
        137..=139 => "netbios",
        143 => "imap",
        161 => "snmp",
        389 => "ldap",
        443 | 8443 => "https",
        445 => "smb",
        853 => "dns_over_tls",
        993 => "imaps",
        995 => "pop3s",
        1433 => "mssql",
        1900 => "ssdp",
        3306 => "mysql",
        3389 => "rdp",
        3478 | 3479 => "stun",
        5222 => "xmpp",
        5353 => "mdns",
        5432 => "postgres",
        5355 => "llmnr",
        6379 => "redis",
        27017 => "mongodb",
        _ => return None,
    })
}

// ----------------------------------------------------------------- name map

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameSource {
    /// A DNS-Client ETW event: the strongest source, because it also tells us
    /// which process asked.
    DnsEvent,
    /// A PTR lookup we made ourselves. Often absent or unhelpful for CDNs.
    Reverse,
}

impl NameSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            NameSource::DnsEvent => "dns_event",
            NameSource::Reverse => "reverse",
        }
    }
}

#[derive(Clone, Debug)]
pub struct NameEntry {
    /// Empty means "we looked and found nothing" — kept so a failed reverse
    /// lookup is not retried on every sweep.
    pub host: String,
    pub source: NameSource,
    /// The process that resolved the name, when the source can say.
    pub pid: Option<u32>,
    pub seen_ms: u64,
}

/// Address → name, bounded. An IP can be resolved by several processes and
/// re-resolved constantly; the map keeps one entry per address and prefers the
/// better source rather than letting a reverse lookup overwrite a real
/// DNS answer.
pub struct NameMap {
    map: HashMap<IpAddr, NameEntry>,
    cap: usize,
}

impl Default for NameMap {
    fn default() -> Self {
        NameMap::new(4096)
    }
}

impl NameMap {
    pub fn new(cap: usize) -> Self {
        NameMap { map: HashMap::new(), cap }
    }

    /// Record a name. A DNS event always wins over a reverse lookup; between
    /// two events the newer one wins, because a CDN address really can change
    /// which name it serves.
    pub fn insert(&mut self, ip: IpAddr, host: &str, source: NameSource, pid: Option<u32>, now_ms: u64) {
        if let Some(existing) = self.map.get(&ip) {
            let downgrade = existing.source == NameSource::DnsEvent
                && source == NameSource::Reverse
                && !existing.host.is_empty();
            if downgrade {
                return;
            }
        }
        if self.map.len() >= self.cap && !self.map.contains_key(&ip) {
            self.evict_oldest();
        }
        self.map.insert(
            ip,
            NameEntry { host: host.to_string(), source, pid, seen_ms: now_ms },
        );
    }

    /// The name for an address, or None when unknown or known-nameless.
    pub fn get(&self, ip: &IpAddr) -> Option<&NameEntry> {
        self.map.get(ip).filter(|e| !e.host.is_empty())
    }

    /// True once we have any verdict for this address, including "no name".
    /// The reverse-lookup queue uses this so it asks each address once.
    pub fn contains(&self, ip: &IpAddr) -> bool {
        self.map.contains_key(ip)
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self
            .map
            .iter()
            .min_by_key(|(_, e)| e.seen_ms)
            .map(|(ip, _)| *ip)
        {
            self.map.remove(&oldest);
        }
    }
}

// -------------------------------------------------------- DNS event parsing

/// Pull the addresses out of a DNS-Client QueryResults string.
///
/// The field is a `;`-separated list where each item is either a bare address
/// or `type: <n> <value>`; CNAME items carry a name rather than an address and
/// are skipped. Example:
/// `type: 5 e13678.dscb.akamaiedge.net;type: 1 23.202.231.169;`
pub fn parse_query_results(results: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for item in results.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        // Drop a leading "type: <n>" if present; what remains is the value.
        let value = match item.strip_prefix("type:") {
            Some(rest) => rest.trim_start().split_once(' ').map(|(_, v)| v).unwrap_or(""),
            None => item,
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if let Ok(ip) = value.parse::<IpAddr>() {
            // Normalise ::ffff:1.2.3.4 to 1.2.3.4 so it joins against the v4
            // rows the connection table reports.
            let ip = match ip {
                IpAddr::V6(v6) => v6_as_v4(&v6).map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
                v4 => v4,
            };
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

/// Decode a Microsoft-Windows-DNS-Client event 3008 (query completed) payload.
///
/// The manifest lays the fields out as QueryName (UTF-16, NUL-terminated),
/// QueryType (u32), QueryOptions (u64), QueryStatus (u32), QueryResults
/// (UTF-16, NUL-terminated). We decode by hand rather than pulling in TDH:
/// the layout is fixed, and a wrong guess costs a dropped event, not a crash,
/// because every read is bounds-checked.
///
/// Returns None for a failed query, an unparseable payload, or a query that
/// resolved to no addresses (a CNAME-only or NXDOMAIN answer).
pub fn parse_dns_query_event(payload: &[u8]) -> Option<(String, Vec<IpAddr>)> {
    let (name, next) = read_utf16z(payload, 0)?;
    // QueryType (4) + QueryOptions (8) + QueryStatus (4).
    let status_at = next.checked_add(12)?;
    if status_at + 4 > payload.len() {
        return None;
    }
    let status = u32::from_le_bytes(payload[status_at..status_at + 4].try_into().ok()?);
    if status != 0 {
        return None;
    }
    let (results, _) = read_utf16z(payload, status_at + 4)?;
    let ips = parse_query_results(&results);
    if name.is_empty() || ips.is_empty() {
        return None;
    }
    Some((name.trim_end_matches('.').to_ascii_lowercase(), ips))
}

/// Read a NUL-terminated UTF-16 string at `at`, returning it and the offset
/// just past its terminator.
fn read_utf16z(buf: &[u8], at: usize) -> Option<(String, usize)> {
    if at >= buf.len() {
        return None;
    }
    let mut units = Vec::new();
    let mut i = at;
    loop {
        if i + 2 > buf.len() {
            return None; // unterminated: refuse rather than guess
        }
        let u = u16::from_le_bytes([buf[i], buf[i + 1]]);
        i += 2;
        if u == 0 {
            break;
        }
        units.push(u);
    }
    Some((String::from_utf16_lossy(&units), i))
}

// ------------------------------------------------------------------ filters

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StateFilter {
    /// Connections that are actually carrying traffic — the useful default.
    #[default]
    Established,
    Listening,
    All,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ScopeFilter {
    /// Anything leaving the machine. Unfiltered, the table is mostly loopback.
    #[default]
    Public,
    Local,
    All,
}

impl StateFilter {
    pub fn parse(s: &str) -> StateFilter {
        match s.trim().to_ascii_lowercase().as_str() {
            "listening" | "listen" => StateFilter::Listening,
            "all" => StateFilter::All,
            _ => StateFilter::Established,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            StateFilter::Established => "established",
            StateFilter::Listening => "listening",
            StateFilter::All => "all",
        }
    }
}

impl ScopeFilter {
    pub fn parse(s: &str) -> ScopeFilter {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" | "private" => ScopeFilter::Local,
            "all" => ScopeFilter::All,
            _ => ScopeFilter::Public,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            ScopeFilter::Public => "public",
            ScopeFilter::Local => "local",
            ScopeFilter::All => "all",
        }
    }
}

/// Every field is optional and they combine with AND, so a caller can ask
/// "what is msedgewebview2 talking to" or "who is on port 445" or both.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub process: Option<String>,
    pub pid: Option<u32>,
    /// Exact address, or a prefix: "204.79." matches the whole /16.
    pub remote_ip: Option<String>,
    /// Substring of the resolved hostname, case-insensitive.
    pub host: Option<String>,
    pub port: Option<u16>,
    pub state: StateFilter,
    pub scope: ScopeFilter,
}

impl Filter {
    pub fn matches(&self, row: &Row) -> bool {
        let c = &row.conn;
        match self.state {
            // A UDP row has no state; it is "carrying traffic" by nature, so
            // it belongs in the established view rather than nowhere.
            StateFilter::Established => {
                if !(c.state == TCP_ESTABLISHED || (c.proto == Proto::Udp && c.remote.is_some()))
                {
                    return false;
                }
            }
            StateFilter::Listening => {
                if !(c.state == TCP_LISTEN || (c.proto == Proto::Udp && c.remote.is_none())) {
                    return false;
                }
            }
            StateFilter::All => {}
        }
        // Scope is judged on the remote end; a listener has none, so it is
        // only reachable through the "all" scope.
        let scope = c.remote_ip().map(|ip| scope_of(&ip));
        match self.scope {
            ScopeFilter::Public => {
                if scope != Some(Scope::Public) {
                    return false;
                }
            }
            ScopeFilter::Local => match scope {
                Some(s) if s.is_local() => {}
                _ => return false,
            },
            ScopeFilter::All => {}
        }
        if let Some(pid) = self.pid {
            if c.pid != pid {
                return false;
            }
        }
        if let Some(p) = &self.process {
            if !glob_match(p, &row.process) {
                return false;
            }
        }
        if let Some(prefix) = &self.remote_ip {
            match c.remote_ip() {
                Some(ip) if ip_matches(prefix, &ip) => {}
                _ => return false,
            }
        }
        if let Some(h) = &self.host {
            match &row.host {
                Some(host) if host_matches(h, host) => {}
                _ => return false,
            }
        }
        if let Some(port) = self.port {
            if c.remote_port() != Some(port) && c.local_port != port {
                return false;
            }
        }
        true
    }
}

/// Prefix match on the printed address, so "204.79." selects a /16 and
/// "204.79.197.222" selects one host, without asking anyone for CIDR.
pub fn ip_matches(prefix: &str, ip: &IpAddr) -> bool {
    let prefix = prefix.trim().to_ascii_lowercase();
    if prefix.is_empty() {
        return true;
    }
    let text = ip.to_string().to_ascii_lowercase();
    if prefix.contains('*') {
        return glob_match(&prefix, &text);
    }
    text.starts_with(&prefix)
}

/// Hostname match: a bare string is a substring test ("asus.com" finds
/// mymessage.asus.com), and `*` is a wildcard for people who want to anchor.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let host = host.trim().to_ascii_lowercase();
    if pattern.is_empty() {
        return true;
    }
    if pattern.contains('*') {
        glob_match(&pattern, &host)
    } else {
        host.contains(&pattern)
    }
}

/// Case-insensitive glob with `*` only. Iterative with backtracking, so a
/// pathological pattern cannot blow the stack.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.trim().to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.trim().to_ascii_lowercase().chars().collect();
    if p.is_empty() {
        return true;
    }
    if !p.contains(&'*') {
        return p == t;
    }
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Parse the pipe command's `key=value` arguments, tab-separated. Unknown
/// keys are ignored rather than rejected: a newer MCP shim talking to an older
/// app should lose a filter, not the whole answer.
pub fn parse_filter(args: &str) -> (Filter, usize) {
    let mut f = Filter::default();
    let mut limit = DEFAULT_LIMIT;
    for part in args.split('\t') {
        let Some((k, v)) = part.split_once('=') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if v.is_empty() {
            continue;
        }
        match k.as_str() {
            "process" => f.process = Some(v.to_string()),
            "pid" => f.pid = v.parse().ok(),
            "remote_ip" | "ip" => f.remote_ip = Some(v.to_string()),
            "host" => f.host = Some(v.to_string()),
            "port" => f.port = v.parse().ok(),
            "state" => f.state = StateFilter::parse(v),
            "scope" => f.scope = ScopeFilter::parse(v),
            "limit" => limit = v.parse().unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            _ => {}
        }
    }
    (f, limit)
}

pub const DEFAULT_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 500;

/// The MCP response body. `dns_available` is reported rather than assumed:
/// without it every row is either reverse-resolved or unnamed, and a caller
/// that cannot tell the difference would read absence as evidence.
pub fn to_json(
    rows: &[Row],
    total_matched: usize,
    dns_available: bool,
    swept_ms: u64,
    filter: &Filter,
) -> String {
    use crate::util::json_escape as esc;
    let items: Vec<String> = rows
        .iter()
        .map(|r| {
            let c = &r.conn;
            let remote_addr = match c.remote_ip() {
                Some(ip) => format!("\"{}\"", ip),
                None => "null".to_string(),
            };
            let remote_port = match c.remote_port() {
                Some(p) => p.to_string(),
                None => "null".to_string(),
            };
            let host = match &r.host {
                Some(h) => format!("\"{}\"", esc(h)),
                None => "null".to_string(),
            };
            let source = match &r.name_source {
                Some(s) => format!("\"{}\"", s.as_str()),
                None => "null".to_string(),
            };
            let name_pid = match r.name_pid {
                Some(p) => p.to_string(),
                None => "null".to_string(),
            };
            let scope = match c.remote_ip() {
                Some(ip) => format!("\"{}\"", scope_of(&ip).as_str()),
                None => "null".to_string(),
            };
            // The service label describes the remote end for an outbound
            // connection and the local end for a listener.
            let service = match c.remote_port().or(Some(c.local_port)).and_then(service_name) {
                Some(s) => format!("\"{}\"", s),
                None => "null".to_string(),
            };
            format!(
                "{{\"pid\":{},\"process\":\"{}\",\"proto\":\"{}\",\"local_addr\":\"{}\",\
                 \"local_port\":{},\"remote_addr\":{},\"remote_port\":{},\"state\":\"{}\",\
                 \"host\":{},\"name_source\":{},\"name_pid\":{},\"scope\":{},\"service\":{}}}",
                c.pid,
                esc(&r.process),
                c.proto.as_str(),
                c.local_ip,
                c.local_port,
                remote_addr,
                remote_port,
                state_name(c.state),
                host,
                source,
                name_pid,
                scope,
                service,
            )
        })
        .collect();
    let opt = |v: &Option<String>| match v {
        Some(s) => format!("\"{}\"", esc(s)),
        None => "null".to_string(),
    };
    let num = |v: Option<u32>| match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    };
    format!(
        "{{\"dns_available\":{},\"swept_unix_ms\":{},\"total_matched\":{},\"returned\":{},\
         \"truncated\":{},\"filter\":{{\"process\":{},\"pid\":{},\"remote_ip\":{},\"host\":{},\
         \"port\":{},\"state\":\"{}\",\"scope\":\"{}\"}},\"connections\":[{}]}}",
        dns_available,
        swept_ms,
        total_matched,
        rows.len(),
        total_matched > rows.len(),
        opt(&filter.process),
        num(filter.pid),
        opt(&filter.remote_ip),
        opt(&filter.host),
        num(filter.port.map(|p| p as u32)),
        filter.state.as_str(),
        filter.scope.as_str(),
        items.join(","),
    )
}

/// Join a sweep with process names and resolved hostnames, apply the filter,
/// and sort. Returns the rows to show and how many matched before the limit.
pub fn build_rows(
    conns: &[Conn],
    process_of: &HashMap<u32, String>,
    names: &NameMap,
    filter: &Filter,
    limit: usize,
) -> (Vec<Row>, usize) {
    let mut rows: Vec<Row> = conns
        .iter()
        .map(|c| {
            let entry = c.remote_ip().and_then(|ip| names.get(&ip));
            Row {
                conn: c.clone(),
                process: process_of.get(&c.pid).cloned().unwrap_or_default(),
                host: entry.map(|e| e.host.clone()),
                name_source: entry.map(|e| e.source),
                name_pid: entry.and_then(|e| e.pid),
            }
        })
        .filter(|r| filter.matches(r))
        .collect();
    let total = rows.len();
    sort_rows(&mut rows);
    rows.truncate(limit);
    (rows, total)
}

/// Group rows so one app's twenty connections read as a block: by process
/// name, then by remote host or address, then by port.
pub fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        let key = |r: &Row| {
            (
                r.process.to_ascii_lowercase(),
                r.host.clone().unwrap_or_else(|| {
                    r.conn.remote_ip().map(|i| i.to_string()).unwrap_or_default()
                }),
                r.conn.remote_port().unwrap_or(0),
                r.conn.pid,
            )
        };
        key(a).cmp(&key(b))
    });
}

// ------------------------------------------------------------ enumeration

/// Port fields in the MIB tables are a DWORD holding a network-byte-order
/// port in the low two bytes.
pub fn mib_port(dw: u32) -> u16 {
    u16::from_be((dw & 0xffff) as u16)
}

#[cfg(windows)]
pub use imp::{sweep, start_reverse_resolver, queue_reverse_lookups};

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::sync::{Arc, Mutex, OnceLock};

    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
        MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };

    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    const NO_ERROR: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    /// Every live TCP and UDP socket with its owning pid. Cheap enough
    /// (a few hundred microseconds) to run on the sampler tick.
    pub fn sweep() -> Vec<Conn> {
        let mut out = Vec::with_capacity(256);
        unsafe {
            tcp_table(&mut out, false);
            tcp_table(&mut out, true);
            udp_table(&mut out, false);
            udp_table(&mut out, true);
        }
        out
    }

    /// Call an Extended*Table function, growing the buffer until it fits.
    /// The size the API reports can go stale between calls when sockets open,
    /// so this retries rather than trusting the first answer.
    unsafe fn table_bytes(v6: bool, tcp: bool) -> Option<Vec<u8>> {
        let family = if v6 { AF_INET6 } else { AF_INET };
        let mut size: u32 = 32 * 1024;
        for _ in 0..5 {
            let mut buf = vec![0u8; size as usize];
            let rc = if tcp {
                GetExtendedTcpTable(
                    buf.as_mut_ptr() as *mut _,
                    &mut size,
                    0,
                    family,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                )
            } else {
                GetExtendedUdpTable(
                    buf.as_mut_ptr() as *mut _,
                    &mut size,
                    0,
                    family,
                    UDP_TABLE_OWNER_PID,
                    0,
                )
            };
            match rc {
                NO_ERROR => {
                    buf.truncate(size as usize);
                    return Some(buf);
                }
                ERROR_INSUFFICIENT_BUFFER => continue,
                _ => return None,
            }
        }
        None
    }

    /// The MIB tables are `{ u32 count; ROW rows[count]; }`. The buffer is a
    /// byte vector, so every field is read unaligned.
    unsafe fn rows_of<T: Copy>(buf: &[u8]) -> Vec<T> {
        if buf.len() < 4 {
            return Vec::new();
        }
        let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let stride = std::mem::size_of::<T>();
        // The row array starts after the count, at the struct's alignment.
        let start = std::mem::align_of::<T>().max(4);
        let available = buf.len().saturating_sub(start) / stride;
        let count = count.min(available);
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let p = buf.as_ptr().add(start + i * stride) as *const T;
            out.push(p.read_unaligned());
        }
        out
    }

    unsafe fn tcp_table(out: &mut Vec<Conn>, v6: bool) {
        let Some(buf) = table_bytes(v6, true) else { return };
        if v6 {
            for r in rows_of::<MIB_TCP6ROW_OWNER_PID>(&buf) {
                let remote_ip = IpAddr::V6(Ipv6Addr::from(r.ucRemoteAddr));
                let remote_port = mib_port(r.dwRemotePort);
                out.push(Conn {
                    pid: r.dwOwningPid,
                    proto: Proto::Tcp,
                    local_ip: IpAddr::V6(Ipv6Addr::from(r.ucLocalAddr)),
                    local_port: mib_port(r.dwLocalPort),
                    remote: remote_endpoint(remote_ip, remote_port, r.dwState),
                    state: r.dwState,
                });
            }
        } else {
            for r in rows_of::<MIB_TCPROW_OWNER_PID>(&buf) {
                let remote_ip = IpAddr::V4(Ipv4Addr::from(r.dwRemoteAddr.to_ne_bytes()));
                let remote_port = mib_port(r.dwRemotePort);
                out.push(Conn {
                    pid: r.dwOwningPid,
                    proto: Proto::Tcp,
                    local_ip: IpAddr::V4(Ipv4Addr::from(r.dwLocalAddr.to_ne_bytes())),
                    local_port: mib_port(r.dwLocalPort),
                    remote: remote_endpoint(remote_ip, remote_port, r.dwState),
                    state: r.dwState,
                });
            }
        }
    }

    unsafe fn udp_table(out: &mut Vec<Conn>, v6: bool) {
        let Some(buf) = table_bytes(v6, false) else { return };
        if v6 {
            for r in rows_of::<MIB_UDP6ROW_OWNER_PID>(&buf) {
                out.push(Conn {
                    pid: r.dwOwningPid,
                    proto: Proto::Udp,
                    local_ip: IpAddr::V6(Ipv6Addr::from(r.ucLocalAddr)),
                    local_port: mib_port(r.dwLocalPort),
                    remote: None,
                    state: UDP_STATE,
                });
            }
        } else {
            for r in rows_of::<MIB_UDPROW_OWNER_PID>(&buf) {
                out.push(Conn {
                    pid: r.dwOwningPid,
                    proto: Proto::Udp,
                    local_ip: IpAddr::V4(Ipv4Addr::from(r.dwLocalAddr.to_ne_bytes())),
                    local_port: mib_port(r.dwLocalPort),
                    remote: None,
                    state: UDP_STATE,
                });
            }
        }
    }

    /// A listening socket reports 0.0.0.0:0 as its remote end; reporting that
    /// as an endpoint would invent a connection that does not exist.
    fn remote_endpoint(ip: IpAddr, port: u16, state: u32) -> Option<(IpAddr, u16)> {
        if state == TCP_LISTEN || port == 0 || scope_of(&ip) == Scope::Unspecified {
            None
        } else {
            Some((ip, port))
        }
    }

    // ------------------------------------------------------ reverse lookups

    /// PTR lookups block for as long as the resolver takes, so they never run
    /// on the sampler or pipe thread. One worker drains a bounded queue and
    /// writes answers — including "no name", so nothing is asked twice —
    /// straight into the shared name map.
    static SENDER: OnceLock<SyncSender<IpAddr>> = OnceLock::new();
    static WSA_READY: AtomicBool = AtomicBool::new(false);

    pub fn start_reverse_resolver(names: Arc<Mutex<NameMap>>) {
        if SENDER.get().is_some() {
            return;
        }
        let (tx, rx) = sync_channel::<IpAddr>(256);
        if SENDER.set(tx).is_err() {
            return;
        }
        std::thread::spawn(move || {
            init_winsock();
            while let Ok(ip) = rx.recv() {
                let host = reverse_lookup(&ip).unwrap_or_default();
                let now = crate::sampler::unix_ms();
                names
                    .lock()
                    .unwrap()
                    .insert(ip, &host, NameSource::Reverse, None, now);
            }
        });
    }

    /// Queue addresses we have no verdict on yet. Never blocks: if the worker
    /// is behind, the extra addresses are simply asked about next sweep.
    pub fn queue_reverse_lookups(conns: &[Conn], names: &Mutex<NameMap>) {
        let Some(tx) = SENDER.get() else { return };
        // One lock for the whole sweep rather than one per row: this runs on
        // the sampler tick against a table that can be hundreds of rows.
        // `try_send` never blocks, so holding it here cannot stall anyone.
        let map = names.lock().unwrap();
        let mut asked = std::collections::HashSet::new();
        for c in conns {
            let Some(ip) = c.remote_ip() else { continue };
            if scope_of(&ip) != Scope::Public || map.contains(&ip) || !asked.insert(ip) {
                continue;
            }
            if tx.try_send(ip).is_err() {
                break; // worker is behind; the next sweep asks again
            }
        }
    }

    fn init_winsock() {
        use windows_sys::Win32::Networking::WinSock::{WSAStartup, WSADATA};
        unsafe {
            let mut data: WSADATA = std::mem::zeroed();
            // 2.2; failure just means reverse lookups stay unavailable.
            if WSAStartup(0x0202, &mut data) == 0 {
                WSA_READY.store(true, Ordering::Relaxed);
            }
        }
    }

    fn reverse_lookup(ip: &IpAddr) -> Option<String> {
        use windows_sys::Win32::Networking::WinSock::{
            getnameinfo, NI_NAMEREQD, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
        };
        if !WSA_READY.load(Ordering::Relaxed) {
            return None;
        }
        unsafe {
            let mut host = [0u8; 256];
            let rc = match ip {
                IpAddr::V4(v4) => {
                    let mut sa: SOCKADDR_IN = std::mem::zeroed();
                    sa.sin_family = AF_INET as u16;
                    sa.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.octets());
                    getnameinfo(
                        &sa as *const _ as *const SOCKADDR,
                        std::mem::size_of::<SOCKADDR_IN>() as i32,
                        host.as_mut_ptr(),
                        host.len() as u32,
                        std::ptr::null_mut(),
                        0,
                        NI_NAMEREQD as i32,
                    )
                }
                IpAddr::V6(v6) => {
                    let mut sa: SOCKADDR_IN6 = std::mem::zeroed();
                    sa.sin6_family = AF_INET6 as u16;
                    sa.sin6_addr.u.Byte = v6.octets();
                    getnameinfo(
                        &sa as *const _ as *const SOCKADDR,
                        std::mem::size_of::<SOCKADDR_IN6>() as i32,
                        host.as_mut_ptr(),
                        host.len() as u32,
                        std::ptr::null_mut(),
                        0,
                        NI_NAMEREQD as i32,
                    )
                }
            };
            if rc != 0 {
                return None;
            }
            let end = host.iter().position(|&b| b == 0).unwrap_or(host.len());
            let name = String::from_utf8_lossy(&host[..end]).trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some(name.to_ascii_lowercase())
            }
        }
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn row(pid: u32, process: &str, remote: &str, port: u16, host: Option<&str>, state: u32) -> Row {
        Row {
            conn: Conn {
                pid,
                proto: Proto::Tcp,
                local_ip: ip("192.168.1.10"),
                local_port: 51000,
                remote: Some((ip(remote), port)),
                state,
            },
            process: process.to_string(),
            host: host.map(|h| h.to_string()),
            name_source: host.map(|_| NameSource::DnsEvent),
            name_pid: host.map(|_| pid),
        }
    }

    /// Value of a top-level JSON key in our own compact output.
    fn field<'a>(json: &'a str, key: &str) -> &'a str {
        let needle = format!("\"{}\":", key);
        let at = json.find(&needle).unwrap_or_else(|| panic!("no key {} in {}", key, json));
        let rest = &json[at + needle.len()..];
        let end = rest
            .find(|c| c == ',' || c == '}')
            .unwrap_or(rest.len());
        rest[..end].trim_matches('"')
    }

    #[test]
    fn classifies_address_scope() {
        assert_eq!(scope_of(&ip("127.0.0.1")), Scope::Loopback);
        assert_eq!(scope_of(&ip("192.168.1.10")), Scope::Private);
        assert_eq!(scope_of(&ip("10.0.0.1")), Scope::Private);
        assert_eq!(scope_of(&ip("172.16.0.1")), Scope::Private);
        assert_eq!(scope_of(&ip("172.32.0.1")), Scope::Public, "172.32 is outside the /12");
        assert_eq!(scope_of(&ip("169.254.1.1")), Scope::LinkLocal);
        assert_eq!(scope_of(&ip("100.64.0.1")), Scope::Private, "carrier-grade NAT");
        assert_eq!(scope_of(&ip("224.0.0.251")), Scope::Multicast);
        assert_eq!(scope_of(&ip("0.0.0.0")), Scope::Unspecified);
        assert_eq!(scope_of(&ip("204.79.197.222")), Scope::Public);
        assert_eq!(scope_of(&ip("::1")), Scope::Loopback);
        assert_eq!(scope_of(&ip("fe80::1")), Scope::LinkLocal);
        assert_eq!(scope_of(&ip("fd00::1")), Scope::Private);
        assert_eq!(scope_of(&ip("ff02::1")), Scope::Multicast);
        assert_eq!(scope_of(&ip("2606:4700::1111")), Scope::Public);
    }

    #[test]
    fn v4_mapped_v6_is_judged_as_v4() {
        // ::ffff:127.0.0.1 is loopback, not a public v6 address.
        assert_eq!(scope_of(&ip("::ffff:127.0.0.1")), Scope::Loopback);
        assert_eq!(scope_of(&ip("::ffff:192.168.0.1")), Scope::Private);
    }

    #[test]
    fn mib_port_undoes_network_byte_order() {
        // 443 on the wire is 0x01BB, stored byte-swapped in the low word.
        assert_eq!(mib_port(0xbb01), 443);
        assert_eq!(mib_port(0x5000), 80);
        assert_eq!(mib_port(0), 0);
        // High bytes of the DWORD are ignored.
        assert_eq!(mib_port(0xffff_bb01), 443);
    }

    #[test]
    fn names_well_known_ports_only() {
        assert_eq!(service_name(443), Some("https"));
        assert_eq!(service_name(445), Some("smb"));
        assert_eq!(service_name(53), Some("dns"));
        assert_eq!(service_name(137), Some("netbios"));
        assert_eq!(service_name(51234), None);
    }

    #[test]
    fn parses_query_results_skipping_cnames() {
        let ips = parse_query_results("type: 5 e13678.dscb.akamaiedge.net;type: 1 23.202.231.169;");
        assert_eq!(ips, vec![ip("23.202.231.169")]);
    }

    #[test]
    fn parses_bare_and_v6_query_results() {
        let ips = parse_query_results("204.79.197.222;2606:4700::1111;");
        assert_eq!(ips, vec![ip("204.79.197.222"), ip("2606:4700::1111")]);
    }

    #[test]
    fn normalises_v4_mapped_results_so_they_join_v4_rows() {
        // The resolver reports some answers in v4-mapped form; the connection
        // table reports the same host as plain v4. They must meet.
        let ips = parse_query_results("::ffff:23.202.231.169;");
        assert_eq!(ips, vec![ip("23.202.231.169")]);
    }

    #[test]
    fn query_results_deduplicate() {
        let ips = parse_query_results("type: 1 1.2.3.4;type: 1 1.2.3.4;");
        assert_eq!(ips.len(), 1);
    }

    /// Build a 3008-shaped payload the way the provider lays it out.
    fn dns_payload(name: &str, status: u32, results: &str) -> Vec<u8> {
        let mut v = Vec::new();
        let mut push = |s: &str| {
            for u in s.encode_utf16() {
                v.extend_from_slice(&u.to_le_bytes());
            }
            v.extend_from_slice(&0u16.to_le_bytes());
        };
        push(name);
        v.extend_from_slice(&1u32.to_le_bytes()); // QueryType
        v.extend_from_slice(&0u64.to_le_bytes()); // QueryOptions
        v.extend_from_slice(&status.to_le_bytes()); // QueryStatus
        let mut tail = Vec::new();
        for u in results.encode_utf16() {
            tail.extend_from_slice(&u.to_le_bytes());
        }
        tail.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&tail);
        v
    }

    #[test]
    fn decodes_a_successful_dns_query_event() {
        let p = dns_payload("mymessage.asus.com", 0, "type: 1 104.20.1.5;");
        let (name, ips) = parse_dns_query_event(&p).unwrap();
        assert_eq!(name, "mymessage.asus.com");
        assert_eq!(ips, vec![ip("104.20.1.5")]);
    }

    #[test]
    fn lowercases_and_strips_the_root_dot() {
        let p = dns_payload("Bing.COM.", 0, "204.79.197.222;");
        assert_eq!(parse_dns_query_event(&p).unwrap().0, "bing.com");
    }

    #[test]
    fn drops_failed_and_addressless_queries() {
        // NXDOMAIN: a name with no answer teaches us nothing about an IP.
        let failed = dns_payload("nope.invalid", 9003, "");
        assert!(parse_dns_query_event(&failed).is_none());
        // CNAME-only answer: no address to attach the name to.
        let cname = dns_payload("x.example", 0, "type: 5 y.example;");
        assert!(parse_dns_query_event(&cname).is_none());
    }

    #[test]
    fn refuses_truncated_payloads_instead_of_guessing() {
        assert!(parse_dns_query_event(&[]).is_none());
        assert!(parse_dns_query_event(&[0x41, 0x00]).is_none(), "no terminator");
        let full = dns_payload("a.example", 0, "1.2.3.4;");
        for cut in [2usize, 8, 20, full.len() - 4] {
            // Every truncation must be rejected, never read out of bounds.
            let _ = parse_dns_query_event(&full[..cut]);
        }
    }

    #[test]
    fn name_map_prefers_dns_events_over_reverse_lookups() {
        let mut m = NameMap::new(16);
        m.insert(ip("1.2.3.4"), "real.example", NameSource::DnsEvent, Some(42), 100);
        m.insert(ip("1.2.3.4"), "ptr.example", NameSource::Reverse, None, 200);
        let e = m.get(&ip("1.2.3.4")).unwrap();
        assert_eq!(e.host, "real.example");
        assert_eq!(e.pid, Some(42));
        // ...but a newer DNS event does replace an older one.
        m.insert(ip("1.2.3.4"), "moved.example", NameSource::DnsEvent, Some(7), 300);
        assert_eq!(m.get(&ip("1.2.3.4")).unwrap().host, "moved.example");
    }

    #[test]
    fn empty_name_means_asked_and_answered_nothing() {
        let mut m = NameMap::new(16);
        m.insert(ip("1.2.3.4"), "", NameSource::Reverse, None, 100);
        assert!(m.get(&ip("1.2.3.4")).is_none(), "no name to show");
        assert!(m.contains(&ip("1.2.3.4")), "but never ask again");
    }

    #[test]
    fn name_map_evicts_oldest_at_capacity() {
        let mut m = NameMap::new(2);
        m.insert(ip("1.1.1.1"), "a", NameSource::DnsEvent, None, 10);
        m.insert(ip("2.2.2.2"), "b", NameSource::DnsEvent, None, 20);
        m.insert(ip("3.3.3.3"), "c", NameSource::DnsEvent, None, 30);
        assert!(!m.contains(&ip("1.1.1.1")), "oldest is dropped");
        assert!(m.contains(&ip("2.2.2.2")));
        assert!(m.contains(&ip("3.3.3.3")));
    }

    #[test]
    fn glob_matches_wildcards() {
        assert!(glob_match("*.asus.com", "mymessage.asus.com"));
        assert!(!glob_match("*.asus.com", "asus.com.evil.net"));
        assert!(glob_match("msedge*", "msedgewebview2.exe"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("CHROME.EXE", "chrome.exe"), "case-insensitive");
    }

    #[test]
    fn host_match_is_substring_unless_wildcarded() {
        assert!(host_matches("asus.com", "rog-content-platform.asus.com"));
        assert!(host_matches("ASUS", "mymessage.asus.com"));
        assert!(!host_matches("*.asus.com", "asus.com"), "wildcard anchors");
        assert!(host_matches("*.asus.com", "cloud-portal.asus.com"));
    }

    #[test]
    fn ip_match_is_a_prefix() {
        assert!(ip_matches("204.79.", &ip("204.79.197.222")));
        assert!(ip_matches("204.79.197.222", &ip("204.79.197.222")));
        assert!(!ip_matches("204.79.", &ip("204.7.9.1")));
        assert!(ip_matches("2606:", &ip("2606:4700::1111")));
    }

    #[test]
    fn filter_defaults_to_established_public() {
        let f = Filter::default();
        assert!(f.matches(&row(1, "edge.exe", "204.79.197.222", 443, None, TCP_ESTABLISHED)));
        // Loopback is noise for this question.
        assert!(!f.matches(&row(1, "edge.exe", "127.0.0.1", 443, None, TCP_ESTABLISHED)));
        // So is a socket that is only listening.
        assert!(!f.matches(&row(1, "edge.exe", "204.79.197.222", 443, None, TCP_LISTEN)));
    }

    #[test]
    fn filters_combine_with_and() {
        let r = row(4828, "msedgewebview2.exe", "204.79.197.222", 443, Some("bing.com"), TCP_ESTABLISHED);
        let f = Filter { process: Some("msedgewebview2.exe".into()), port: Some(443), ..Default::default() };
        assert!(f.matches(&r));
        let f = Filter { process: Some("msedgewebview2.exe".into()), port: Some(80), ..Default::default() };
        assert!(!f.matches(&r), "port must also match");
        let f = Filter { pid: Some(27616), ..Default::default() };
        assert!(!f.matches(&r));
        let f = Filter { host: Some("bing".into()), ..Default::default() };
        assert!(f.matches(&r));
        let f = Filter { remote_ip: Some("204.79.".into()), ..Default::default() };
        assert!(f.matches(&r));
    }

    #[test]
    fn host_filter_never_matches_an_unnamed_row() {
        // A row we could not name must not be swept into "everything asus".
        let unnamed = row(1, "svchost.exe", "20.86.94.139", 443, None, TCP_ESTABLISHED);
        let f = Filter { host: Some("asus".into()), ..Default::default() };
        assert!(!f.matches(&unnamed));
    }

    #[test]
    fn listening_filter_finds_servers_not_clients() {
        let f = Filter { state: StateFilter::Listening, scope: ScopeFilter::All, ..Default::default() };
        let mut listener = row(1, "sshd.exe", "0.0.0.0", 0, None, TCP_LISTEN);
        listener.conn.remote = None;
        listener.conn.local_port = 22;
        assert!(f.matches(&listener));
        assert!(!f.matches(&row(1, "edge.exe", "1.1.1.1", 443, None, TCP_ESTABLISHED)));
    }

    #[test]
    fn port_filter_looks_at_both_ends() {
        // "who is on 445" should find both the client and the listener.
        let f = Filter { port: Some(445), scope: ScopeFilter::All, state: StateFilter::All, ..Default::default() };
        assert!(f.matches(&row(1, "a.exe", "192.168.1.5", 445, None, TCP_ESTABLISHED)));
        let mut listener = row(2, "System", "0.0.0.0", 0, None, TCP_LISTEN);
        listener.conn.remote = None;
        listener.conn.local_port = 445;
        assert!(f.matches(&listener));
    }

    #[test]
    fn udp_rows_count_as_established_traffic() {
        let mut u = row(1, "svchost.exe", "8.8.8.8", 53, None, UDP_STATE);
        u.conn.proto = Proto::Udp;
        assert!(Filter::default().matches(&u));
    }

    #[test]
    fn sorting_groups_a_browser_together() {
        let mut rows = vec![
            row(1, "svchost.exe", "20.86.94.139", 443, None, TCP_ESTABLISHED),
            row(2, "msedgewebview2.exe", "150.171.1.1", 443, Some("cdn.example"), TCP_ESTABLISHED),
            row(3, "msedgewebview2.exe", "204.79.197.222", 443, Some("bing.com"), TCP_ESTABLISHED),
        ];
        sort_rows(&mut rows);
        assert_eq!(rows[0].process, "msedgewebview2.exe");
        assert_eq!(rows[1].process, "msedgewebview2.exe");
        assert_eq!(rows[0].host.as_deref(), Some("bing.com"), "named endpoints sort together");
        assert_eq!(rows[2].process, "svchost.exe");
    }

    #[test]
    fn parses_pipe_arguments() {
        let (f, limit) = parse_filter("process=msedgewebview2.exe\tport=443\tstate=all\tlimit=25");
        assert_eq!(f.process.as_deref(), Some("msedgewebview2.exe"));
        assert_eq!(f.port, Some(443));
        assert_eq!(f.state, StateFilter::All);
        assert_eq!(limit, 25);
        assert_eq!(f.scope, ScopeFilter::Public, "unset filters keep their defaults");
    }

    #[test]
    fn ignores_unknown_and_empty_arguments() {
        // A newer shim talking to an older app should lose a filter, not the
        // whole answer.
        let (f, limit) = parse_filter("future_key=1\thost=\tpid=notanumber");
        assert!(f.host.is_none() && f.pid.is_none());
        assert_eq!(limit, DEFAULT_LIMIT);
        let (f, limit) = parse_filter("");
        assert!(f.process.is_none());
        assert_eq!(limit, DEFAULT_LIMIT);
    }

    #[test]
    fn clamps_limit_to_the_documented_range() {
        assert_eq!(parse_filter("limit=99999").1, MAX_LIMIT);
        assert_eq!(parse_filter("limit=0").1, 1);
    }

    fn conn(pid: u32, remote: &str, port: u16) -> Conn {
        Conn {
            pid,
            proto: Proto::Tcp,
            local_ip: ip("192.168.1.10"),
            local_port: 51000,
            remote: Some((ip(remote), port)),
            state: TCP_ESTABLISHED,
        }
    }

    #[test]
    fn builds_rows_by_joining_process_names_and_hostnames() {
        let conns = vec![conn(27616, "204.79.197.222", 443), conn(7204, "172.211.123.249", 443)];
        let mut procs = HashMap::new();
        procs.insert(27616, "msedgewebview2.exe".to_string());
        procs.insert(7204, "svchost.exe".to_string());
        let mut names = NameMap::new(16);
        names.insert(ip("204.79.197.222"), "bing.com", NameSource::DnsEvent, Some(27616), 1);
        let (rows, total) = build_rows(&conns, &procs, &names, &Filter::default(), 10);
        assert_eq!(total, 2);
        assert_eq!(rows[0].process, "msedgewebview2.exe");
        assert_eq!(rows[0].host.as_deref(), Some("bing.com"));
        assert_eq!(rows[0].name_pid, Some(27616), "this process asked for it itself");
        // The unnamed row is still reported; absence of a name is not a
        // reason to hide a connection.
        assert_eq!(rows[1].process, "svchost.exe");
        assert!(rows[1].host.is_none() && rows[1].name_source.is_none());
    }

    #[test]
    fn unknown_pids_still_produce_a_row() {
        // A process that exited between the process sample and the sweep has
        // no name; dropping the row would hide a live connection.
        let (rows, total) = build_rows(&[conn(999, "1.1.1.1", 443)], &HashMap::new(), &NameMap::new(4), &Filter::default(), 10);
        assert_eq!(total, 1);
        assert_eq!(rows[0].process, "");
    }

    #[test]
    fn limit_truncates_but_the_total_still_counts_everything() {
        let conns: Vec<Conn> = (0..5).map(|i| conn(100 + i, "1.1.1.1", 443)).collect();
        let (rows, total) = build_rows(&conns, &HashMap::new(), &NameMap::new(4), &Filter::default(), 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(total, 5, "the caller must be able to see what it did not get");
    }

    #[test]
    fn payload_reports_truncation_and_dns_availability() {
        let rows = vec![row(1, "a.exe", "1.1.1.1", 443, Some("one.example"), TCP_ESTABLISHED)];
        let json = to_json(&rows, 9, true, 1_700_000_000_000, &Filter::default());
        assert_eq!(field(&json, "total_matched"), "9");
        assert_eq!(field(&json, "returned"), "1");
        assert_eq!(field(&json, "truncated"), "true");
        assert_eq!(field(&json, "dns_available"), "true");
        assert_eq!(field(&json, "swept_unix_ms"), "1700000000000");
        let json = to_json(&rows, 1, false, 0, &Filter::default());
        assert_eq!(field(&json, "truncated"), "false");
        assert_eq!(field(&json, "dns_available"), "false");
    }

    #[test]
    fn payload_row_carries_every_documented_field() {
        let rows = vec![row(4828, "msedgewebview2.exe", "204.79.197.222", 443, Some("bing.com"), TCP_ESTABLISHED)];
        let json = to_json(&rows, 1, true, 0, &Filter::default());
        for key in [
            "pid", "process", "proto", "local_addr", "local_port", "remote_addr",
            "remote_port", "state", "host", "name_source", "name_pid", "scope", "service",
        ] {
            assert!(json.contains(&format!("\"{}\":", key)), "missing {}", key);
        }
        assert!(json.contains("\"remote_addr\":\"204.79.197.222\""));
        assert!(json.contains("\"service\":\"https\""));
        assert!(json.contains("\"scope\":\"public\""));
        assert!(json.contains("\"state\":\"established\""));
        assert!(json.contains("\"name_source\":\"dns_event\""));
    }

    #[test]
    fn unnamed_rows_emit_null_not_an_empty_string() {
        // "" would read as a hostname that is blank; null reads as unknown.
        let rows = vec![row(1, "svchost.exe", "20.86.94.139", 443, None, TCP_ESTABLISHED)];
        let json = to_json(&rows, 1, true, 0, &Filter::default());
        assert!(json.contains("\"host\":null"));
        assert!(json.contains("\"name_source\":null"));
        assert!(json.contains("\"name_pid\":null"));
    }

    #[test]
    fn listeners_emit_null_endpoints_rather_than_zeros() {
        // 0.0.0.0:0 would look like a connection to a real address.
        let mut listener = row(1, "sshd.exe", "0.0.0.0", 0, None, TCP_LISTEN);
        listener.conn.remote = None;
        listener.conn.local_port = 22;
        let json = to_json(&listener_rows(listener), 1, true, 0, &Filter::default());
        assert!(json.contains("\"remote_addr\":null"));
        assert!(json.contains("\"remote_port\":null"));
        assert!(json.contains("\"scope\":null"));
        assert!(json.contains("\"service\":\"ssh\""), "falls back to the local port");
    }

    fn listener_rows(r: Row) -> Vec<Row> {
        vec![r]
    }

    #[test]
    fn payload_escapes_hostile_process_names() {
        // A process name is attacker-controllable; an unescaped quote would
        // corrupt the response the assistant parses.
        let mut r = row(1, "evil\".exe", "1.1.1.1", 443, Some("a\\b\"c"), TCP_ESTABLISHED);
        r.process = "evil\"\n.exe".to_string();
        let json = to_json(&[r], 1, true, 0, &Filter::default());
        assert!(json.contains("evil\\\"\\n.exe"));
        assert!(json.contains("a\\\\b\\\"c"));
    }

    #[test]
    fn payload_echoes_the_filter_it_applied() {
        // The caller has to be able to tell "nothing matched" from
        // "you asked a narrower question than you meant".
        let f = Filter {
            process: Some("edge*".into()),
            port: Some(443),
            state: StateFilter::All,
            scope: ScopeFilter::All,
            ..Default::default()
        };
        let json = to_json(&[], 0, true, 0, &f);
        assert!(json.contains("\"process\":\"edge*\""));
        assert!(json.contains("\"port\":443"));
        assert!(json.contains("\"state\":\"all\""));
        assert!(json.contains("\"scope\":\"all\""));
        assert!(json.contains("\"pid\":null"));
        assert!(json.contains("\"connections\":[]"));
    }

    #[test]
    fn state_and_scope_filters_parse_leniently() {
        assert_eq!(StateFilter::parse("LISTENING"), StateFilter::Listening);
        assert_eq!(StateFilter::parse("listen"), StateFilter::Listening);
        assert_eq!(StateFilter::parse("nonsense"), StateFilter::Established);
        assert_eq!(ScopeFilter::parse("private"), ScopeFilter::Local);
        assert_eq!(ScopeFilter::parse("all"), ScopeFilter::All);
        assert_eq!(ScopeFilter::parse(""), ScopeFilter::Public);
    }
}
