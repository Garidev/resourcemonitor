//! What connected AI tools report they are working on, and what they have
//! finished. Pure data and rules, platform-independent and unit-tested — the
//! pipe layer parses, this decides.
//!
//! Nothing here is measured. It is whatever an assistant said it was doing.

use std::collections::VecDeque;

/// One agent as reported over MCP.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentEntry {
    pub id: String,
    pub title: String,
    /// running | waiting | done | failed (free text; unknown values render dim)
    pub status: String,
    pub detail: String,
    /// Unix ms when this entry was last reported, for staleness.
    pub seen_ms: u64,
    /// Unix ms when this id was first seen in its session, so a finished agent
    /// can report how long it ran.
    pub started_ms: u64,
}

/// One AI session — a single `resmon-mcp.exe` process, which clients spawn
/// once per session. Keyed by that process so two sessions stay separate.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSession {
    pub key: String,
    /// Human-readable, e.g. "claude-code · resourcemonitor.app".
    pub label: String,
    pub agents: Vec<AgentEntry>,
    pub last_seen_ms: u64,
}

/// An agent that has finished, kept so the user can refer back to it.
#[derive(Clone, Debug, PartialEq)]
pub struct FinishedAgent {
    pub id: String,
    pub title: String,
    pub status: String,
    pub detail: String,
    pub session_label: String,
    pub started_ms: u64,
    pub finished_ms: u64,
}

/// Entries not refreshed within this window are shown as stale rather than
/// presented as live, because an assistant that crashed will never say so.
pub const AGENT_STALE_MS: u64 = 5 * 60 * 1000;
/// ...and are archived and dropped from the live list after this long.
pub const AGENT_DROP_MS: u64 = 30 * 60 * 1000;
/// How many finished agents to keep for the session.
pub const AGENT_HISTORY_MAX: usize = 200;
/// Longest title/detail kept in history. The cap above is by count, so without
/// this an assistant sending kilobyte-long details could grow history without
/// bound.
pub const AGENT_TEXT_MAX: usize = 200;

/// The identity an entry is tracked by. Assistants are asked for a stable id
/// but not required to send one; falling back to the title keeps an id-less
/// agent the same agent across calls, where anything positional would rebind
/// identities whenever the list changed and misattribute start times.
pub fn effective_id(id: &str, title: &str) -> String {
    if !id.is_empty() {
        return id.to_string();
    }
    if !title.is_empty() {
        return title.to_string();
    }
    "agent".to_string()
}

/// True when a reported status means the agent is still working.
pub fn is_live(status: &str) -> bool {
    status.is_empty() || status == "running" || status == "waiting"
}

/// Flatten control characters to spaces. Agent text is drawn as single lines
/// and logged one record per line, so a newline or stray ANSI code in a detail
/// must not be able to break layout or the log format.
pub fn clean_text(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Truncate on a character boundary, since agent text is arbitrary UTF-8.
pub fn clamp_text(s: &str) -> String {
    if s.chars().count() <= AGENT_TEXT_MAX {
        return s.to_string();
    }
    s.chars().take(AGENT_TEXT_MAX).collect()
}

/// An agent we stopped hearing about rather than one reported finished. Its
/// client died mid-task, so claiming "done" would be a lie and keeping
/// "running" would contradict the list it now sits in.
pub const STATUS_ABANDONED: &str = "abandoned";

/// Record a finished agent, replacing any earlier record of the same run.
///
/// Keyed by (id, started_ms): replace-all means a `done` agent may be sent on
/// every call, and a later session reusing an id is a genuinely different run.
///
/// `status` is passed rather than taken from the entry because how a run ended
/// is known by the caller, not by the last thing the assistant said about it —
/// an omitted agent is still marked `running` in its own last report.
pub fn archive(
    history: &mut VecDeque<FinishedAgent>,
    entry: &AgentEntry,
    session_label: &str,
    finished_ms: u64,
    status: &str,
) -> FinishedAgent {
    let rec = FinishedAgent {
        id: entry.id.clone(),
        title: clamp_text(&entry.title),
        status: status.to_string(),
        detail: clamp_text(&entry.detail),
        session_label: session_label.to_string(),
        started_ms: entry.started_ms,
        finished_ms,
    };
    if let Some(slot) = history.iter_mut().find(|f| f.id == rec.id && f.started_ms == rec.started_ms) {
        *slot = rec.clone();
        return rec;
    }
    history.push_front(rec.clone());
    while history.len() > AGENT_HISTORY_MAX {
        history.pop_back();
    }
    rec
}

/// What one report changed: how many agents are now live in that session, and
/// which finished. The archived list lets the caller write the optional log
/// file without this module doing any IO.
#[derive(Default, Debug)]
pub struct Report {
    pub live: usize,
    pub archived: Vec<FinishedAgent>,
}

/// Apply one `report_agents` call.
///
/// Replace-all is scoped to `key`, so one session can never clear another's
/// work. Within the session, agents split into:
///
/// - live: reported with a working status
/// - finished: reported done/failed, or previously live and now absent
///
/// `started_ms` carries over for an id already live in the session, so an
/// agent that runs across several reports keeps one start time.
pub fn apply_report(
    sessions: &mut Vec<AgentSession>,
    history: &mut VecDeque<FinishedAgent>,
    key: &str,
    label: &str,
    reported: Vec<AgentEntry>,
    now: u64,
) -> Report {
    let mut out = Report::default();
    let idx = match sessions.iter().position(|s| s.key == key) {
        Some(i) => i,
        None => {
            sessions.push(AgentSession {
                key: key.to_string(),
                label: label.to_string(),
                agents: Vec::new(),
                last_seen_ms: now,
            });
            sessions.len() - 1
        }
    };
    sessions[idx].label = label.to_string();
    sessions[idx].last_seen_ms = now;
    let previous = std::mem::take(&mut sessions[idx].agents);

    let mut live: Vec<AgentEntry> = Vec::new();
    for mut a in reported {
        if let Some(old) = previous.iter().find(|p| p.id == a.id) {
            a.started_ms = old.started_ms;
        }
        if is_live(&a.status) {
            live.push(a);
        } else {
            let status = a.status.clone();
            out.archived.push(archive(history, &a, label, now, &status));
        }
    }
    // Anything previously live and not mentioned in this report is finished —
    // which is exactly what the tool description promises the user.
    for old in &previous {
        if !live.iter().any(|a| a.id == old.id) && !history.iter().any(|f| f.id == old.id && f.started_ms == old.started_ms) {
            out.archived.push(archive(history, old, label, now, "done"));
        }
    }
    sessions[idx].agents = live;
    out.live = sessions[idx].agents.len();
    out
}

/// Archive agents from clients that stopped reporting, and drop sessions that
/// have gone quiet. An assistant that crashes never says so, so without this
/// its agents would sit live forever and never reach history.
pub fn expire(
    sessions: &mut Vec<AgentSession>,
    history: &mut VecDeque<FinishedAgent>,
    now: u64,
) -> Vec<FinishedAgent> {
    let mut archived = Vec::new();
    for s in sessions.iter_mut() {
        let label = s.label.clone();
        let (keep, gone): (Vec<AgentEntry>, Vec<AgentEntry>) = s
            .agents
            .drain(..)
            .partition(|a| now.saturating_sub(a.seen_ms) < AGENT_DROP_MS);
        for a in &gone {
            // Finished when last heard from: the 30 minutes we waited before
            // giving up is our patience, not the agent's runtime.
            archived.push(archive(history, a, &label, a.seen_ms, STATUS_ABANDONED));
        }
        s.agents = keep;
    }
    sessions.retain(|s| !s.agents.is_empty() || now.saturating_sub(s.last_seen_ms) < AGENT_DROP_MS);
    archived
}

/// Live agents across every session, for the main panel's footer count.
pub fn live_count(sessions: &[AgentSession], now: u64) -> usize {
    sessions
        .iter()
        .flat_map(|s| s.agents.iter())
        .filter(|a| now.saturating_sub(a.seen_ms) < AGENT_STALE_MS && is_live(&a.status))
        .count()
}

/// One line per finished agent for the optional log file, in the same shape as
/// the existing rule log.
pub fn log_line(f: &FinishedAgent, stamp: &str) -> String {
    format!(
        "{} | agent {} | {} | {} | {}",
        stamp,
        f.status,
        f.title,
        f.detail,
        format_duration(f.finished_ms.saturating_sub(f.started_ms))
    )
}

/// "3m 41s" / "12s" / "1h 04m".
pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{}s", secs);
    }
    if secs < 3600 {
        return format!("{}m {:02}s", secs / 60, secs % 60);
    }
    format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, status: &str, now: u64) -> AgentEntry {
        AgentEntry {
            id: id.to_string(),
            title: id.to_string(),
            status: status.to_string(),
            detail: format!("doing {}", id),
            seen_ms: now,
            started_ms: now,
        }
    }

    fn apply(s: &mut Vec<AgentSession>, h: &mut VecDeque<FinishedAgent>, key: &str, e: Vec<AgentEntry>, now: u64) -> usize {
        apply_report(s, h, key, "claude-code · proj", e, now).live
    }

    #[test]
    fn omitted_agent_is_archived() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        apply(&mut s, &mut h, "k", vec![entry("a", "running", 100), entry("b", "running", 100)], 100);
        assert_eq!(h.len(), 0);
        // "b" simply stops being mentioned.
        let live = apply(&mut s, &mut h, "k", vec![entry("a", "running", 200)], 200);
        assert_eq!(live, 1);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].id, "b");
        assert_eq!(h[0].status, "done");
    }

    #[test]
    fn done_and_failed_are_archived_not_left_live() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        let live = apply(
            &mut s,
            &mut h,
            "k",
            vec![entry("a", "running", 100), entry("b", "done", 100), entry("c", "failed", 100)],
            100,
        );
        assert_eq!(live, 1, "only the running agent stays live");
        assert_eq!(h.len(), 2);
        assert!(h.iter().any(|f| f.id == "b" && f.status == "done"));
        assert!(h.iter().any(|f| f.id == "c" && f.status == "failed"));
    }

    #[test]
    fn repeating_a_finished_agent_does_not_duplicate_it() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        for t in [100, 200, 300] {
            let mut e = entry("a", "done", 100);
            e.seen_ms = t;
            apply(&mut s, &mut h, "k", vec![e], t);
        }
        assert_eq!(h.len(), 1, "replace-all resends done agents every call");
    }

    #[test]
    fn duration_measured_from_first_sighting() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        apply(&mut s, &mut h, "k", vec![entry("a", "running", 1_000)], 1_000);
        let mut later = entry("a", "running", 5_000);
        later.started_ms = 5_000; // a fresh report claims "now"...
        apply(&mut s, &mut h, "k", vec![later], 5_000);
        apply(&mut s, &mut h, "k", vec![], 8_000);
        // ...but the session remembers when the id first appeared.
        assert_eq!(h[0].started_ms, 1_000);
        assert_eq!(h[0].finished_ms, 8_000);
        assert_eq!(format_duration(h[0].finished_ms - h[0].started_ms), "7s");
    }

    #[test]
    fn reusing_an_id_after_it_finished_is_a_new_run() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        apply(&mut s, &mut h, "k", vec![entry("main", "running", 100)], 100);
        apply(&mut s, &mut h, "k", vec![], 200); // finishes
        apply(&mut s, &mut h, "k", vec![entry("main", "running", 900)], 900);
        apply(&mut s, &mut h, "k", vec![], 1000); // finishes again
        assert_eq!(h.len(), 2, "two separate runs of the same id");
        assert_ne!(h[0].started_ms, h[1].started_ms);
    }

    #[test]
    fn two_sessions_do_not_overwrite_each_other() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        apply_report(&mut s, &mut h, "one", "A", vec![entry("a", "running", 100)], 100);
        let r = apply_report(&mut s, &mut h, "two", "B", vec![entry("b", "running", 100)], 100);
        assert!(r.archived.is_empty(), "starting a session must not finish another's work");
        assert_eq!(s.len(), 2);
        assert_eq!(live_count(&s, 100), 2, "session B must not clear session A");
        // B replacing its own list leaves A untouched, and archives only its own.
        apply_report(&mut s, &mut h, "two", "B", vec![], 200);
        assert_eq!(live_count(&s, 200), 1);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].id, "b");
        assert_eq!(h[0].session_label, "B");
    }

    #[test]
    fn history_is_capped_and_evicts_oldest() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        for i in 0..(AGENT_HISTORY_MAX + 50) {
            let t = 1000 + i as u64;
            apply(&mut s, &mut h, "k", vec![entry(&format!("a{}", i), "running", t)], t);
        }
        apply(&mut s, &mut h, "k", vec![], 999_999);
        assert_eq!(h.len(), AGENT_HISTORY_MAX);
        assert_eq!(h[0].id, format!("a{}", AGENT_HISTORY_MAX + 49), "newest first");
        assert!(!h.iter().any(|f| f.id == "a0"), "oldest evicted");
    }

    #[test]
    fn long_text_is_truncated_on_archive() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        let mut e = entry("a", "done", 100);
        e.detail = "x".repeat(5000);
        e.title = "t".repeat(5000);
        apply(&mut s, &mut h, "k", vec![e], 100);
        assert_eq!(h[0].detail.chars().count(), AGENT_TEXT_MAX);
        assert_eq!(h[0].title.chars().count(), AGENT_TEXT_MAX);
    }

    #[test]
    fn control_characters_become_spaces() {
        // The agent log is one record per line and the panel draws single
        // lines; a detail with a newline or ANSI colour codes must not be able
        // to break either.
        assert_eq!(clean_text("step 1\nstep 2"), "step 1 step 2");
        assert_eq!(clean_text("a\u{1b}[31mred\u{1b}[0m\tb"), "a [31mred [0m b");
        assert!(!log_line(
            &FinishedAgent {
                id: "x".into(),
                title: clean_text("multi\r\nline"),
                status: "done".into(),
                detail: clean_text("more\nlines"),
                session_label: "L".into(),
                started_ms: 0,
                finished_ms: 1000,
            },
            "2026-08-13 09:00:00"
        )
        .contains('\n'));
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        // Bytes would panic mid-codepoint; agent text is arbitrary UTF-8.
        let s = "é🚀".repeat(500);
        assert_eq!(clamp_text(&s).chars().count(), AGENT_TEXT_MAX);
    }

    #[test]
    fn missing_ids_bind_to_title_not_position() {
        assert_eq!(effective_id("build", "compile everything"), "build");
        // No id: the title is the identity, so the same agent re-reported in a
        // different list position stays the same agent.
        assert_eq!(effective_id("", "compile everything"), "compile everything");
        assert_eq!(effective_id("", ""), "agent");
    }

    #[test]
    fn crashed_client_agents_are_archived_not_left_live_forever() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        apply(&mut s, &mut h, "k", vec![entry("a", "running", 60_000)], 60_000);
        // The client dies: no further report ever arrives.
        expire(&mut s, &mut h, 60_000 + AGENT_DROP_MS + 1);
        assert_eq!(h.len(), 1, "a crashed session's work must still reach history");
        assert_eq!(h[0].id, "a");
        assert_eq!(h[0].status, STATUS_ABANDONED, "we stopped hearing; it did not report done");
        assert_eq!(
            h[0].finished_ms, 60_000,
            "finished when last heard from — counting the 30-minute silence as runtime would inflate every abandoned duration"
        );
        assert!(s.is_empty(), "the quiet session is dropped");
    }

    #[test]
    fn expire_keeps_recent_sessions_and_agents() {
        let (mut s, mut h) = (Vec::new(), VecDeque::new());
        apply(&mut s, &mut h, "k", vec![entry("a", "running", 1000)], 1000);
        expire(&mut s, &mut h, 2000);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].agents.len(), 1);
        assert!(h.is_empty());
    }

    #[test]
    fn live_count_ignores_stale_and_finished() {
        let now = 1_000_000;
        let sessions = vec![AgentSession {
            key: "k".into(),
            label: "L".into(),
            agents: vec![
                entry("fresh", "running", now),
                entry("waiting", "waiting", now),
                entry("stale", "running", now - AGENT_STALE_MS - 1),
            ],
            last_seen_ms: now,
        }];
        assert_eq!(live_count(&sessions, now), 2);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(12_000), "12s");
        assert_eq!(format_duration(221_000), "3m 41s");
        assert_eq!(format_duration(3_840_000), "1h 04m");
    }

    #[test]
    fn log_line_matches_the_rule_log_shape() {
        let f = FinishedAgent {
            id: "r".into(),
            title: "code review".into(),
            status: "done".into(),
            detail: "Reviewing the MCP pipe changes".into(),
            session_label: "claude-code · proj".into(),
            started_ms: 0,
            finished_ms: 221_000,
        };
        assert_eq!(
            log_line(&f, "2026-08-12 14:22:07"),
            "2026-08-12 14:22:07 | agent done | code review | Reviewing the MCP pipe changes | 3m 41s"
        );
    }
}
