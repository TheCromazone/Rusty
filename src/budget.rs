//! Model admission ledger, shared by every call on the client. With
//! `RUSTY_BUDGET_LEDGER` it is also written to a file after every change, so
//! a restarted rusty carries on the same episode's budget instead of starting
//! at zero. Not a tokenizer or a billing cap.
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Limits {
    pub requests: u64,
    pub tokens: u64,
    pub seconds: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self { requests: 256, tokens: 4_000_000, seconds: 3600 }
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// Every HTTP attempt sent, rate-limited ones included: what a proxy sees.
    attempts: u64,
    /// Attempts counted against the request limit (a 429 is given back).
    requests: u64,
    /// Attempts the provider answered with a 2xx status, as a gateway that
    /// only counts successful responses would.
    http_ok: u64,
    /// Attempts that got no HTTP response at all (connect, TLS or timeout).
    transport_errors: u64,
    /// Time spent waiting to retry after rate limits, overloads and dropped
    /// streams: provider time, not the agent's.
    retry_wait_seconds: f64,
    charged_tokens: u64,
    known_tokens: u64,
    settled_requests: u64,
    active_requests: u64,
    denied_requests: u64,
    rate_limited: u64,
    /// Summed from providers that report a price in their usage; the others
    /// leave it out, so it is a lower bound unless every request reported.
    reported_cost_usd: f64,
    cost_reported_requests: u64,
    overrun: bool,
    halted: bool,
    by_role: BTreeMap<String, u64>,
    /// Processes that have used this ledger, this one included.
    runs: u64,
    /// Elapsed provider time carried over from earlier processes.
    elapsed_seconds: f64,
}
#[derive(Serialize)]
pub struct Snapshot {
    limits: Option<Limits>,
    enabled: bool,
    ledger: Option<PathBuf>,
    deadline_reached: bool,
    unknown_usage_requests: u64,
    #[serde(flatten)]
    state: State,
}
pub struct Budget {
    limits: Limits,
    enabled: bool,
    started: Instant,
    #[cfg(test)]
    clock: Option<Arc<dyn Fn() -> Duration + Send + Sync>>,
    /// Provider time spent by earlier processes on the same ledger.
    prior: Duration,
    ledger: Option<(PathBuf, File)>,
    state: Mutex<State>,
}
impl Budget {
    #[cfg(test)]
    pub fn new(limits: Limits) -> Arc<Self> {
        Self::open(Some(limits), None).expect("a budget without a ledger always opens")
    }
    #[cfg(test)]
    pub(crate) fn new_with_clock(limits: Limits, clock: Arc<dyn Fn() -> Duration + Send + Sync>) -> Arc<Self> {
        let mut budget = Self::new(limits);
        Arc::get_mut(&mut budget).expect("new budget is uniquely owned").clock = Some(clock);
        budget
    }
    /// Unbounded runs still collect accounting without enforcement.
    pub fn unbounded() -> Arc<Self> {
        Self::open(None, None).expect("a budget without a ledger always opens")
    }
    /// A budget, resumed from `ledger` when it names an existing file. The
    /// ledger is locked for the life of the process, so two runs can't
    /// spend one episode's budget twice; a ledger that can't be read is an
    /// error rather than a fresh start.
    pub fn open(limits: Option<Limits>, ledger: Option<&Path>) -> Result<Arc<Self>> {
        let mut state = State::default();
        let ledger = match ledger {
            None => None,
            Some(path) => {
                let lock = lock(path)?;
                if path.exists() {
                    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
                    state = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                }
                Some((path.to_path_buf(), lock))
            }
        };
        // In-flight requests of a process that died stay charged and unknown.
        state.active_requests = 0;
        state.runs += 1;
        let budget = Self {
            limits: limits.unwrap_or(Limits { requests: u64::MAX, tokens: u64::MAX, seconds: u64::MAX }),
            enabled: limits.is_some(),
            started: Instant::now(),
            #[cfg(test)]
            clock: None,
            prior: Duration::from_secs_f64(state.elapsed_seconds.max(0.0)),
            ledger,
            state: Mutex::new(state),
        };
        budget.persist(&mut budget.state.lock().unwrap())?;
        Ok(Arc::new(budget))
    }
    fn elapsed(&self) -> Duration {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return self.prior + clock();
        }
        self.prior + self.started.elapsed()
    }
    /// Rewrites the ledger file, if any, whole: written beside it, then
    /// renamed over it, so a crash leaves the old copy or the new one.
    fn persist(&self, s: &mut State) -> Result<()> {
        let Some((path, _)) = &self.ledger else { return Ok(()) };
        s.elapsed_seconds = self.elapsed().as_secs_f64();
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&*s)?)
            .and_then(|()| std::fs::rename(&tmp, path))
            .with_context(|| format!("writing the budget ledger {}", path.display()))
    }
    /// Persists after a change that has no caller to report to.
    fn record(&self, s: &mut State) {
        if let Err(e) = self.persist(s) {
            eprintln!("warning: {e:#}");
        }
    }
    /// Whether limits are set, so requests have a deadline to keep.
    pub fn limited(&self) -> bool {
        self.enabled
    }

    /// Provider admission/response time; shell tools retain their own deadlines.
    pub fn remaining(&self) -> Result<Duration> {
        let s = self.state.lock().unwrap();
        if s.halted {
            bail!("model budget exhausted: required review could not finish; this root is halted");
        }
        if s.overrun {
            bail!("model budget exhausted: reported usage exceeded the admission cap");
        }
        if !self.enabled {
            return Ok(Duration::from_secs(600));
        }
        let remaining = Duration::from_secs(self.limits.seconds).saturating_sub(self.elapsed());
        if remaining.is_zero() {
            bail!("model budget exhausted: provider deadline reached");
        }
        Ok(remaining)
    }
    /// Atomically reserve before each POST, including key rotations/retries.
    /// UTF-8 request bytes plus maximum output is conservative admission
    /// accounting; providers may tokenize differently or exceed their declared cap.
    pub fn reserve(self: &Arc<Self>, role: &str, tokens: u64) -> Result<Reservation> {
        let mut s = self.state.lock().unwrap();
        let deadline = self.elapsed() >= Duration::from_secs(self.limits.seconds);
        if s.halted
            || s.overrun
            || deadline
            || s.requests >= self.limits.requests
            || tokens > self.limits.tokens.saturating_sub(s.charged_tokens)
        {
            s.denied_requests += 1;
            self.record(&mut s);
            let reason = if s.halted {
                "this root is halted after a required review budget failure".to_string()
            } else if s.overrun {
                "reported usage exceeded the token admission cap".to_string()
            } else if deadline {
                "provider deadline reached".to_string()
            } else if s.requests >= self.limits.requests {
                format!("{} HTTP attempts admitted (limit {})", s.requests, self.limits.requests)
            } else {
                format!(
                    "request needs {tokens} admission tokens, {} remain",
                    self.limits.tokens.saturating_sub(s.charged_tokens)
                )
            };
            bail!("model budget exhausted: {role}: {reason}");
        }
        s.attempts += 1;
        s.requests += 1;
        s.charged_tokens += tokens;
        s.active_requests += 1;
        *s.by_role.entry(role.into()).or_default() += 1;
        // Nothing is sent that the ledger hasn't recorded.
        if let Err(e) = self.persist(&mut s) {
            s.attempts -= 1;
            s.requests -= 1;
            s.charged_tokens -= tokens;
            s.active_requests -= 1;
            *s.by_role.entry(role.into()).or_default() -= 1;
            return Err(e);
        }
        Ok(Reservation { budget: self.clone(), tokens, role: role.into() })
    }
    /// An admitted attempt that got a 2xx response.
    pub fn note_ok(&self) {
        let mut s = self.state.lock().unwrap();
        s.http_ok += 1;
        self.record(&mut s);
    }
    /// An admitted attempt that got no HTTP response.
    pub fn note_transport_error(&self) {
        let mut s = self.state.lock().unwrap();
        s.transport_errors += 1;
        self.record(&mut s);
    }
    /// Time spent waiting before a retry.
    pub fn note_wait(&self, waited: Duration) {
        let mut s = self.state.lock().unwrap();
        s.retry_wait_seconds += waited.as_secs_f64();
        self.record(&mut s);
    }
    /// Admissions refused so far; a rise across a call means it ran out.
    pub fn denied(&self) -> u64 {
        self.state.lock().unwrap().denied_requests
    }
    /// Required-review budget failure cannot be bypassed by resuming a goal.
    pub fn halt(&self) {
        let mut s = self.state.lock().unwrap();
        s.halted = true;
        self.record(&mut s);
    }

    /// One line for the model in a bounded run: what is used and what is
    /// left, and when little is left, to finish rather than explore.
    pub fn status(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let s = self.state.lock().unwrap();
        let l = self.limits;
        let elapsed = self.elapsed().as_secs();
        let left = |used: u64, limit: u64| limit.saturating_sub(used) as f64 / limit.max(1) as f64;
        let least = left(s.requests, l.requests).min(left(s.charged_tokens, l.tokens)).min(left(elapsed, l.seconds));
        let mut line = format!(
            "Run budget used: {} of {} model calls, {}k of {}k tokens, {} of {} minutes.",
            s.requests,
            l.requests,
            s.charged_tokens / 1000,
            l.tokens / 1000,
            elapsed / 60,
            l.seconds / 60
        );
        if least < 0.25 {
            line.push_str(
                " Less than a quarter is left: stop exploring, finish the most important open requirement, verify \
                 it and close.",
            );
        }
        Some(line)
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = self.state.lock().unwrap();
        Snapshot {
            limits: self.enabled.then_some(self.limits),
            enabled: self.enabled,
            ledger: self.ledger.as_ref().map(|(p, _)| p.clone()),
            deadline_reached: self.elapsed() >= Duration::from_secs(self.limits.seconds),
            // In-flight attempts have no complete usage yet either.
            unknown_usage_requests: s.requests - s.settled_requests,
            state: State {
                attempts: s.attempts,
                requests: s.requests,
                http_ok: s.http_ok,
                transport_errors: s.transport_errors,
                retry_wait_seconds: s.retry_wait_seconds,
                charged_tokens: s.charged_tokens,
                known_tokens: s.known_tokens,
                settled_requests: s.settled_requests,
                active_requests: s.active_requests,
                denied_requests: s.denied_requests,
                rate_limited: s.rate_limited,
                reported_cost_usd: s.reported_cost_usd,
                cost_reported_requests: s.cost_reported_requests,
                overrun: s.overrun,
                halted: s.halted,
                by_role: s.by_role.clone(),
                runs: s.runs,
                elapsed_seconds: self.elapsed().as_secs_f64(),
            },
        }
    }
}

impl Drop for Budget {
    /// Releases the ledger lock now. Closing the file isn't enough: a child
    /// forked meanwhile shares the open file until it execs, and holds the
    /// lock with it.
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        if let Some((_, file)) = &self.ledger {
            // SAFETY: flock on a descriptor this budget owns.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// An exclusive lock on `<ledger>.lock`, held until the file is closed. The
/// kernel drops it when the process dies, so a crash never leaves it stuck.
fn lock(ledger: &Path) -> Result<File> {
    use std::os::fd::AsRawFd;
    let path = ledger.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(anyhow!("budget ledger {} is in use by another process", ledger.display()));
    }
    Ok(file)
}
pub struct Reservation {
    budget: Arc<Budget>,
    tokens: u64,
    role: String,
}
impl Reservation {
    /// Consume once, only for a complete response with both usage counters.
    /// Partial/error/interrupted streams keep the reservation even when partial
    /// usage appeared. Unexpected excess is charged and stops future admission.
    pub fn settle(self, prompt: u64, completion: u64, cost_usd: Option<f64>) {
        let mut s = self.budget.state.lock().unwrap();
        let actual = prompt.saturating_add(completion);
        s.charged_tokens = s.charged_tokens.saturating_sub(self.tokens).saturating_add(actual);
        s.known_tokens = s.known_tokens.saturating_add(actual);
        s.settled_requests += 1;
        if let Some(cost) = cost_usd.filter(|c| c.is_finite() && *c >= 0.0) {
            s.reported_cost_usd += cost;
            s.cost_reported_requests += 1;
        }
        s.overrun |= s.charged_tokens > self.budget.limits.tokens;
        self.budget.record(&mut s);
    }
}
impl Reservation {
    /// A complete HTTP 429 means the provider refused before doing any work,
    /// so the attempt is returned rather than spending the run's allowance on
    /// someone else's rate limit. The retry window and deadline still bound it.
    pub fn refund_rate_limited(self) {
        let mut s = self.budget.state.lock().unwrap();
        s.requests -= 1;
        s.charged_tokens = s.charged_tokens.saturating_sub(self.tokens);
        if let Some(n) = s.by_role.get_mut(&self.role) {
            *n -= 1;
        }
        s.rate_limited += 1;
        self.budget.record(&mut s);
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.state.lock().unwrap().active_requests -= 1;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn injected_time_expires_reservations_without_wall_clock_delay() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let elapsed = Arc::new(AtomicU64::new(0));
        let clock = elapsed.clone();
        let budget = Budget::new_with_clock(
            Limits { requests: 2, tokens: 1000, seconds: 60 },
            Arc::new(move || Duration::from_millis(clock.load(Ordering::SeqCst))),
        );
        assert_eq!(budget.remaining().unwrap(), Duration::from_secs(60));
        let pending = budget.reserve("lead", 5).unwrap();
        elapsed.store(60_000, Ordering::SeqCst);
        assert!(budget.remaining().unwrap_err().to_string().contains("provider deadline reached"));
        drop(pending);
        let snapshot = serde_json::to_value(budget.snapshot()).unwrap();
        assert_eq!(snapshot["requests"], 1);
        assert_eq!(snapshot["unknown_usage_requests"], 1);
        assert_eq!(snapshot["deadline_reached"], true);
    }

    use std::sync::Barrier;
    #[test]
    fn workers_race_for_one_root_without_overspending() {
        let budget = Budget::new(Limits { requests: 5, tokens: 500, seconds: 30 });
        let barrier = Arc::new(Barrier::new(16));
        let joins: Vec<_> = (0..16)
            .map(|_| {
                let (b, gate) = (budget.clone(), barrier.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    b.reserve("worker", 100).is_ok()
                })
            })
            .collect();
        assert_eq!(joins.into_iter().map(|j| j.join().unwrap() as u64).sum::<u64>(), 5);
        let s = budget.snapshot();
        assert_eq!(
            (s.state.requests, s.state.charged_tokens, s.unknown_usage_requests, s.state.active_requests),
            (5, 500, 5, 0)
        );
        assert_eq!(s.state.denied_requests, 11);
    }
    #[test]
    fn settlement_refunds_only_known_usage_but_never_requests() {
        let b = Budget::new(Limits { requests: 2, tokens: 100, seconds: 30 });
        let r = b.reserve("lead", 100).unwrap();
        assert!(b.reserve("review", 1).is_err());
        r.settle(10, 5, None);
        drop(b.reserve("review", 85).unwrap());
        assert!(b.reserve("compaction", 1).is_err());
        let s = b.snapshot();
        assert_eq!((s.state.charged_tokens, s.state.known_tokens, s.unknown_usage_requests), (100, 15, 1));
        assert_eq!(s.state.by_role["review"], 1);
    }
    #[test]
    fn rate_limited_attempts_are_returned() {
        let b = Budget::new(Limits { requests: 1, tokens: 100, seconds: 30 });
        b.reserve("lead", 100).unwrap().refund_rate_limited();
        let r = b.reserve("lead", 100).unwrap();
        assert!(b.reserve("lead", 1).is_err());
        drop(r);
        let s = b.snapshot();
        assert_eq!((s.state.requests, s.state.charged_tokens, s.state.rate_limited), (1, 100, 1));
        assert_eq!((s.state.by_role["lead"], s.state.active_requests, s.unknown_usage_requests), (1, 0, 1));
    }
    #[test]
    fn overrun_overflow_and_expired_deadlines_fail_closed() {
        let b = Budget::new(Limits { requests: 3, tokens: 100, seconds: 30 });
        b.reserve("lead", 10).unwrap().settle(u64::MAX, 1, None);
        assert!(b.snapshot().state.overrun);
        assert!(b.reserve("worker", 1).is_err());
        assert!(b.remaining().is_err());
        let expired = Budget::new(Limits { seconds: 0, ..Limits::default() });
        assert!(expired.reserve("lead", 1).is_err());
        assert!(expired.remaining().is_err());
    }
    #[test]
    fn attempts_count_what_a_proxy_sees_and_costs_add_up() {
        let b = Budget::new(Limits { requests: 5, tokens: 1000, seconds: 30 });
        b.reserve("lead", 10).unwrap().refund_rate_limited();
        b.reserve("lead", 10).unwrap().settle(3, 2, Some(0.25));
        b.reserve("lead", 10).unwrap().settle(3, 2, None);
        b.reserve("lead", 10).unwrap().settle(3, 2, Some(f64::NAN));
        b.note_ok();
        b.note_transport_error();
        b.note_wait(Duration::from_millis(1500));
        let s = b.snapshot().state;
        assert_eq!((s.attempts, s.requests, s.rate_limited), (4, 3, 1));
        assert_eq!((s.http_ok, s.transport_errors, s.retry_wait_seconds), (1, 1, 1.5));
        assert_eq!((s.reported_cost_usd, s.cost_reported_requests), (0.25, 1));
    }
    #[test]
    fn a_ledger_carries_one_episode_across_restarts() {
        let dir = std::env::temp_dir().join(format!("rusty-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("budget.json");
        let _ = std::fs::remove_file(&path);
        let limits = Some(Limits { requests: 3, tokens: 1000, seconds: 30 });
        {
            let b = Budget::open(limits, Some(&path)).unwrap();
            assert!(Budget::open(limits, Some(&path)).is_err(), "one process at a time");
            b.reserve("lead", 100).unwrap().settle(40, 10, None);
            // Never settled, as when the process dies mid-request: the
            // attempt stays charged and its usage unknown.
            drop(b.reserve("lead", 100).unwrap());
        }
        let b = Budget::open(limits, Some(&path)).unwrap();
        let s = b.snapshot();
        assert_eq!((s.state.requests, s.state.charged_tokens, s.state.runs), (2, 150, 2));
        assert_eq!((s.unknown_usage_requests, s.state.active_requests), (1, 0));
        b.reserve("lead", 1).unwrap();
        assert!(b.reserve("lead", 1).is_err(), "the limit spans both processes");
        drop(b);
        std::fs::write(&path, "not json").unwrap();
        assert!(Budget::open(limits, Some(&path)).is_err(), "a damaged ledger never resets to zero");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
