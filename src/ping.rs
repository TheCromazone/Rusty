//! An opt-in ping when a long turn ends or stops to ask for approval, for a
//! user who has looked away: the desktop-notification escape OSC 9 (iTerm2,
//! cmux, Ghostty, WezTerm show it; others ignore it) and, if asked for, the
//! terminal bell. The words come from a fixed table and the project's folder
//! name filtered to plain characters, so nothing said in the session can
//! reach the terminal as an escape sequence. State is two numbers.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

static MODE: AtomicU8 = AtomicU8::new(0);
/// Milliseconds since the epoch: when the current turn started, and when
/// the last ping went out.
static STARTED: AtomicU64 = AtomicU64::new(0);
static LAST: AtomicU64 = AtomicU64::new(0);

pub const MODES: [&str; 3] = ["off", "on", "sound"];
/// Pings closer together than this are dropped.
const SPACING_MS: u64 = 3_000;

/// What the user is told. Only an authoritative goal outcome may say a task
/// was verified; an ordinary turn only says the response is ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Verified,
    Unverified,
    Blocked,
    Incomplete,
    Failed,
    Ready,
    NeedsApproval,
}

impl Outcome {
    pub fn text(self) -> &'static str {
        match self {
            Outcome::Verified => "goal verified",
            Outcome::Unverified => "goal finished, not verified",
            Outcome::Blocked => "goal blocked, needs you",
            Outcome::Incomplete => "goal not finished",
            Outcome::Failed => "turn failed",
            Outcome::Ready => "response ready",
            Outcome::NeedsApproval => "needs approval",
        }
    }

    /// The end of a goal run, from `Agent::goal_outcome`.
    pub fn of_goal(outcome: Option<&str>, failed: bool) -> Outcome {
        if failed {
            return Outcome::Failed;
        }
        match outcome {
            Some("verified") => Outcome::Verified,
            Some("completed_unverified") => Outcome::Unverified,
            Some("blocked") => Outcome::Blocked,
            Some("incomplete") => Outcome::Incomplete,
            _ => Outcome::Ready,
        }
    }

    /// The end of an ordinary turn.
    pub fn of_turn(failed: bool) -> Outcome {
        if failed {
            Outcome::Failed
        } else {
            Outcome::Ready
        }
    }
}

/// Sets the mode from a saved or typed name; false for an unknown one.
pub fn set(name: &str) -> bool {
    match MODES.iter().position(|m| *m == name) {
        Some(i) => {
            MODE.store(i as u8, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

pub fn mode() -> &'static str {
    MODES[MODE.load(Ordering::Relaxed) as usize % MODES.len()]
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Marks the start of a turn or goal run, against which "long" is measured.
pub fn turn_started() {
    STARTED.store(now_ms(), Ordering::Relaxed);
}

/// Seconds a turn must run before its end pings (`RUSTY_PING_AFTER`, default 10).
fn after_ms() -> u64 {
    std::env::var("RUSTY_PING_AFTER").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(10).saturating_mul(1000)
}

/// Pings for `outcome` if pings are on, stdout is a terminal, the user did
/// not just stop the turn themselves, the turn has run long enough (or is
/// waiting on an approval) and no ping went out a moment ago. The end of a
/// turn uses up its start, so a later approval or turn never measures from it.
pub fn notify(outcome: Outcome, project: &Path) {
    use std::io::IsTerminal;
    let started = match outcome {
        Outcome::NeedsApproval => STARTED.load(Ordering::Relaxed),
        _ => STARTED.swap(0, Ordering::Relaxed),
    };
    if mode() == "off" || !std::io::stdout().is_terminal() || crate::signal::interrupted() {
        return;
    }
    let now = now_ms();
    if !long_enough(outcome, started, now, after_ms()) {
        return;
    }
    let last = LAST.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SPACING_MS
        || LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err()
    {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(payload(outcome, project, mode() == "sound").as_bytes());
    let _ = out.flush();
}

/// An approval prompt always counts: the agent is blocked on the user however
/// short the turn was. The end of a turn counts once it ran for `after` ms.
fn long_enough(outcome: Outcome, started: u64, now: u64, after: u64) -> bool {
    outcome == Outcome::NeedsApproval || (started != 0 && now.saturating_sub(started) >= after)
}

/// The bytes written: OSC 9 with fixed words and the filtered folder name,
/// then a bell in sound mode.
fn payload(outcome: Outcome, project: &Path, bell: bool) -> String {
    let name: String = project
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(40)
        .collect();
    let title = if name.is_empty() { "rusty".to_string() } else { format!("rusty · {name}") };
    format!("\x1b]9;{title}: {}\x07{}", outcome.text(), if bell { "\x07" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_authoritative_outcome_says_verified() {
        assert_eq!(Outcome::of_goal(Some("verified"), false), Outcome::Verified);
        assert_eq!(Outcome::of_goal(Some("completed_unverified"), false), Outcome::Unverified);
        assert_eq!(Outcome::of_goal(Some("blocked"), false), Outcome::Blocked);
        assert_eq!(Outcome::of_goal(Some("incomplete"), false), Outcome::Incomplete);
        assert_eq!(Outcome::of_goal(Some("verified"), true), Outcome::Failed, "an error beats a recorded claim");
        assert_eq!(Outcome::of_goal(None, false), Outcome::Ready);
        assert_eq!(Outcome::of_turn(false).text(), "response ready");
        for o in [Outcome::Unverified, Outcome::Blocked, Outcome::Incomplete, Outcome::Failed, Outcome::Ready] {
            assert!(!o.text().contains("verified") || o == Outcome::Unverified, "{o:?}");
        }
    }

    #[test]
    fn the_payload_cannot_carry_an_escape_from_the_folder_name() {
        let hostile = Path::new("/work/x\x1b]0;owned\x07\u{9b}31m;rm -rf ~\n");
        let p = payload(Outcome::Ready, hostile, false);
        let inner = p.strip_prefix("\x1b]9;").and_then(|s| s.strip_suffix('\x07')).expect("one OSC 9 frame");
        assert!(!inner.chars().any(|c| c.is_control() || c == '\u{9b}'), "{inner:?}");
        assert_eq!(inner, "rusty · x0owned31mrm-rf: response ready");
        assert_eq!(payload(Outcome::NeedsApproval, Path::new("/"), true), "\x1b]9;rusty: needs approval\x07\x07");
        assert!(payload(Outcome::Verified, Path::new(&"a".repeat(200)), false).len() < 80, "the name is capped");
    }

    #[test]
    fn an_approval_pings_however_short_the_turn_but_an_ending_needs_a_long_one() {
        let after = 10_000;
        assert!(long_enough(Outcome::NeedsApproval, 1_000, 6_000, after), "blocked on the user at 5s");
        assert!(long_enough(Outcome::NeedsApproval, 0, 6_000, after), "no turn start recorded");
        assert!(!long_enough(Outcome::Ready, 1_000, 6_000, after));
        assert!(long_enough(Outcome::Ready, 1_000, 11_000, after));
        assert!(!long_enough(Outcome::Failed, 0, 60_000, after), "a spent or missing start never counts");
    }

    #[test]
    fn modes_parse_and_unknown_names_are_refused() {
        assert!(set("sound") && mode() == "sound");
        assert!(!set("loud") && mode() == "sound");
        assert!(set("off") && mode() == "off");
    }
}
