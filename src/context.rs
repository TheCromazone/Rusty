//! Token budgeting. The context window is split into fixed allowances:
//!
//! - system prompt + project instructions: fixed, kept small
//! - memory: at most `MEMORY_CHARS`
//! - plan and goal: pinned, never compacted
//! - reply reserve: the mode's reply size, at most an eighth of the window
//! - history: everything else
//!
//! When history outgrows its share, rusty gives tokens back in order of how
//! cheap they are to get back: first it shrinks old tool outputs (the model
//! can re-run a read), and only then does it summarise older turns. Your own
//! messages, the plan and the goal are never dropped. [`Compaction`] sets how
//! full history gets before each step happens.

use serde_json::{json, Value};
use std::collections::HashMap;

pub const MEMORY_CHARS: usize = 3_000;
/// The reply reserve before a mode sets it, and the floor under it.
pub const REPLY_RESERVE: usize = 16_000;
const MIN_REPLY_RESERVE: usize = 4_000;
const ELIDE_OVER_CHARS: usize = 1_500;
/// The largest window or history cap rusty accepts from any source, well
/// past any model's and far below where the threshold arithmetic overflows.
pub const MAX_WINDOW: usize = 100_000_000;
/// The window the fixed sizes below were tuned for.
const REFERENCE_WINDOW: usize = 128_000;

/// `at_reference` scaled by `window` against a 128k window, kept within [lo, hi].
fn scaled(window: usize, at_reference: usize, lo: usize, hi: usize) -> usize {
    (at_reference * window / REFERENCE_WINDOW).clamp(lo, hi)
}

/// Tool results among the newest this many stay whole: 8 at 128k, 4 to 16.
pub fn kept_tool_results(window: usize) -> usize {
    scaled(window, 8, 4, 16)
}

/// Characters an elided tool result keeps: 400 at 128k, 200 to 800.
pub fn elision_head(window: usize) -> usize {
    scaled(window, 400, 200, 800)
}

/// Characters of the user's own messages a compaction carries: a tenth of
/// the history budget (`history_tokens`), 2,000 to 16,000 tokens' worth.
pub fn carry_chars(history_tokens: usize) -> usize {
    (history_tokens / 10).clamp(2_000, 16_000) * 4
}
const ELIDED_MARK: &str = "[elided to save context";

/// When compaction happens, as a share of the usable window: the window
/// less the reply reserve, filled by the system prompt, tool schemas and
/// history, as [`Meter`] measures it. Each fixed mode names the
/// share at which older turns are summarised; old tool output is trimmed 20
/// points earlier, which keeps `standard` at rusty's original 60% and 80%.
/// `smart` also lets the model compact at a change of task, with `standard`'s
/// thresholds as the backstop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compaction {
    Aggressive,
    #[default]
    Standard,
    Passive,
    Smart,
}

impl Compaction {
    pub const NAMES: &'static str = "aggressive, standard, passive, smart";

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "aggressive" => Some(Self::Aggressive),
            "standard" => Some(Self::Standard),
            "passive" => Some(Self::Passive),
            "smart" => Some(Self::Smart),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Aggressive => "aggressive",
            Self::Standard => "standard",
            Self::Passive => "passive",
            Self::Smart => "smart",
        }
    }

    /// The mode used when none is chosen: aggressive for a window under 48k,
    /// where every token counts, standard otherwise.
    pub fn for_window(window: usize) -> Self {
        if window < 48_000 {
            Self::Aggressive
        } else {
            Self::Standard
        }
    }

    /// Percent of the usable window past which older turns are summarised.
    pub fn summarize_at(self) -> usize {
        match self {
            Self::Aggressive => 60,
            Self::Standard | Self::Smart => 80,
            Self::Passive => 95,
        }
    }

    /// Percent past which old tool output is trimmed: 20 points earlier.
    pub fn trim_at(self) -> usize {
        self.summarize_at() - 20
    }

    /// One line for `/compact settings`.
    pub fn describe(self) -> String {
        let fixed = format!("trims old tool output past {}%, summarises past {}%", self.trim_at(), self.summarize_at());
        match self {
            Self::Smart => format!("smart: the model may compact when the work changes; otherwise {fixed}"),
            m => format!("{}: {fixed}", m.name()),
        }
    }
}

/// Smart mode's own compaction is refused below this share of the budget.
pub const SMART_FLOOR: usize = 20;
/// The history cap used when none is chosen: 128k tokens for a
/// window over 200k, so a huge window isn't filled just because it can be.
pub fn default_history_cap(window: usize) -> Option<usize> {
    (window > 200_000).then_some(REFERENCE_WINDOW)
}

/// Nor below this many tokens, however large the window.
pub const SMART_FLOOR_TOKENS: usize = 4_000;
/// The share above which the prompt invites the model to compact.
pub const SMART_INVITE: usize = 25;
/// Steps a smart compaction keeps verbatim.
pub const SMART_TAIL: usize = 2;
/// Tool calls that must pass after any compaction before the model's next.
pub const SMART_SPACING: usize = 10;
/// How the working set lists files that were edited and are back as they started.
pub const TOUCHED_UNCHANGED: &str = "- touched, now unchanged: ";

/// Moves the `changed` files that `unchanged` says are back as they started
/// to their own line, so a summary doesn't report an edit that was undone.
pub fn collapse_reverts(working_set: &str, unchanged: impl Fn(&str) -> bool) -> String {
    let mut out = String::new();
    for line in working_set.lines() {
        match line.strip_prefix("- changed: ") {
            Some(files) => {
                let (back, kept): (Vec<&str>, Vec<&str>) = files.split(", ").partition(|f| unchanged(f));
                if !kept.is_empty() {
                    out.push_str(&format!("- changed: {}\n", kept.join(", ")));
                }
                if !back.is_empty() {
                    out.push_str(&format!("{TOUCHED_UNCHANGED}{}\n", back.join(", ")));
                }
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// How the working set marks an error no later command has cleared.
pub const UNRESOLVED_ERROR: &str = "- last unresolved error:";

/// `used` as a whole percent of `budget`.
pub fn percent(used: usize, budget: usize) -> usize {
    used * 100 / budget.max(1)
}

/// History as a request sent it, shared with side questions and side chats
/// instead of copied for each of them. `of` copies; `view` shares.
#[derive(Clone, Debug)]
pub struct Snapshot {
    messages: std::sync::Arc<Vec<Value>>,
    range: std::ops::Range<usize>,
}

impl Snapshot {
    pub fn of(history: &[Value]) -> Self {
        Self { messages: std::sync::Arc::new(history.to_vec()), range: 0..history.len() }
    }

    /// `range` of a request's messages, without copying them.
    pub fn view(messages: &std::sync::Arc<Vec<Value>>, range: std::ops::Range<usize>) -> Self {
        Self { messages: messages.clone(), range }
    }

    pub fn messages(&self) -> &[Value] {
        &self.messages[self.range.clone()]
    }
}

/// Rough token count: about 4 characters per token for code and English.
pub fn estimate(messages: &[Value]) -> usize {
    messages.iter().map(estimate_one).sum()
}

pub fn estimate_one(m: &Value) -> usize {
    let mut chars = 16; // role and framing overhead
    if let Some(s) = m["content"].as_str() {
        chars += s.len();
    }
    if let Some(calls) = m["tool_calls"].as_array() {
        for c in calls {
            chars += c["function"]["arguments"].as_str().map_or(0, str::len) + 32;
        }
    }
    chars / 4
}

/// The window assumed when nothing says otherwise.
pub const ASSUMED_WINDOW: usize = 128_000;

/// How full the context is, read from the provider's own token counts.
///
/// The prompt tokens a provider reports for a request are the context at
/// that request: system prompt, tool schemas and history. Between requests,
/// what was appended since is estimated and scaled by a calibration ratio
/// (measured over estimated tokens, an average updated on every reported
/// request), so no tokenizer ships and no model name is consulted. Without
/// any report the whole context is estimated.
#[derive(Clone, Debug)]
pub struct Meter {
    pub window: usize,
    /// Where `window` came from: flag, endpoint, table, assumed or overflow.
    pub window_source: &'static str,
    ratio: f64,
    calibrated: bool,
    /// The last reported prompt and the history length it covered.
    last: Option<(usize, usize)>,
    /// The largest prompt a request has been accepted with.
    largest_accepted: usize,
    /// Tokens kept free for the reply (see [`Meter::set_reply`]).
    pub reserve: usize,
    pub requests: Vec<RequestUsage>,
}

/// One request's token counts, as the provider reported them or estimated.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RequestUsage {
    pub prompt: u64,
    pub completion: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// `provider` when the response carried usage, `estimated` otherwise.
    pub source: &'static str,
}

/// Calibration: weight of the newest measurement, and the ratio's bounds.
const CALIBRATION_WEIGHT: f64 = 0.3;
const CALIBRATION_BOUNDS: (f64, f64) = (0.5, 2.0);

impl Default for Meter {
    fn default() -> Self {
        Self::new(ASSUMED_WINDOW, "assumed")
    }
}

impl Meter {
    pub fn new(window: usize, window_source: &'static str) -> Self {
        Self {
            window: window.clamp(1, MAX_WINDOW),
            window_source,
            ratio: 1.0,
            calibrated: false,
            last: None,
            largest_accepted: 0,
            reserve: REPLY_RESERVE.min(window.clamp(1, MAX_WINDOW) / 8).max(MIN_REPLY_RESERVE),
            requests: Vec::new(),
        }
    }

    /// The reply reserve: the mode's reply size, at most an eighth of the
    /// window, at least 4,000 tokens.
    pub fn set_reply(&mut self, reply_tokens: usize) {
        self.reserve = self.reserve_for(reply_tokens);
    }

    /// The reserve a reply of `reply_tokens` would get in this window.
    pub fn reserve_for(&self, reply_tokens: usize) -> usize {
        reply_tokens.min(self.window / 8).max(MIN_REPLY_RESERVE)
    }

    /// The calibration ratio, or None before the first reported request.
    pub fn calibration(&self) -> Option<f64> {
        self.calibrated.then_some(self.ratio)
    }

    /// Records a finished request: `sent` is the estimate of everything it
    /// sent (system prompt and tool schemas included) and `history_len` how
    /// much history it covered.
    pub fn observe(&mut self, usage: Option<&crate::llm::Usage>, sent: usize, history_len: usize) {
        match usage {
            Some(u) => {
                let prompt = u.prompt as usize;
                if sent > 0 && prompt > 0 {
                    let measured = prompt as f64 / sent as f64;
                    // The first report sets the ratio; later ones are averaged in.
                    let next = if self.calibrated {
                        (1.0 - CALIBRATION_WEIGHT) * self.ratio + CALIBRATION_WEIGHT * measured
                    } else {
                        measured
                    };
                    self.ratio = next.clamp(CALIBRATION_BOUNDS.0, CALIBRATION_BOUNDS.1);
                    self.calibrated = true;
                }
                self.last = Some((prompt, history_len));
                self.largest_accepted = self.largest_accepted.max(prompt);
                self.requests.push(RequestUsage {
                    prompt: u.prompt,
                    completion: u.completion,
                    cache_read: u.cache_read,
                    cache_write: u.cache_write,
                    source: "provider",
                });
            }
            None => {
                // The last report still describes its prefix of history; only
                // what came after it is estimated.
                self.requests.push(RequestUsage {
                    prompt: self.scaled(sent) as u64,
                    completion: 0,
                    cache_read: 0,
                    cache_write: 0,
                    source: "estimated",
                });
            }
        }
    }

    /// History was rewritten (trimmed or compacted), so the last report no
    /// longer describes it.
    pub fn forget_last(&mut self) {
        self.last = None;
    }

    fn scaled(&self, estimate: usize) -> usize {
        (estimate as f64 * self.ratio).round() as usize
    }

    /// Tokens in context now: the last report plus what history gained since,
    /// or, without a usable report, `fixed` (system prompt and tool schemas)
    /// plus all of history, estimated.
    pub fn fill(&self, history: &[Value], fixed: usize) -> usize {
        match self.last {
            Some((prompt, len)) if len <= history.len() => prompt + self.scaled(estimate(&history[len..])),
            _ => self.scaled(fixed + estimate(history)),
        }
    }

    /// What a request may fill: the window less the reply reserve.
    pub fn capacity(&self) -> usize {
        self.window.saturating_sub(self.reserve).max(1)
    }

    /// A reply stopped because it reached the window: the window is at least
    /// that request's prompt plus its output, and at least the largest prompt
    /// accepted plus the reply reserve. Never raises it.
    pub fn reached(&mut self, prompt_and_output: usize) -> bool {
        let window = prompt_and_output.max(self.largest_accepted + self.reserve);
        if window == 0 || window >= self.window {
            return false;
        }
        self.window = window;
        self.window_source = "overflow";
        self.last = None;
        true
    }

    /// Corrects the window from a provider's overflow error. Returns whether
    /// it changed. An OpenAI-style message names the limit; one that doesn't
    /// lowers the window to the largest prompt the endpoint accepted.
    pub fn overflow(&mut self, error: &str) -> bool {
        let lower = error.to_lowercase();
        let named = number_after(&lower, "maximum context length is ")
            .or_else(|| number_after(&lower, "tokens > ").filter(|_| lower.contains("maximum")));
        let window = match named {
            Some(n) => n,
            None if lower.contains("model_context_window_exceeded")
                || lower.contains("context_length_exceeded")
                || lower.contains("prompt is too long") =>
            {
                if self.largest_accepted == 0 {
                    return false;
                }
                self.largest_accepted + self.reserve
            }
            None => return false,
        };
        if window == 0 || window >= self.window {
            return false;
        }
        self.window = window;
        self.window_source = "overflow";
        self.last = None;
        true
    }
}

/// The integer right after `marker` in `text`, if any.
fn number_after(text: &str, marker: &str) -> Option<usize> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == ',').filter(|c| *c != ',').collect();
    digits.parse().ok()
}

/// Shrinks old, large tool results in place, as many and as far as suits
/// `window`. Returns tokens saved.
pub fn elide_old_tool_outputs(history: &mut [Value], window: usize) -> usize {
    elide_tool_outputs(history, kept_tool_results(window), elision_head(window))
}

/// Shrinks large tool results except the newest `keep_recent` to their first
/// `head` characters. Returns tokens saved.
pub fn elide_tool_outputs(history: &mut [Value], keep_recent: usize, head: usize) -> usize {
    let tool_idx: Vec<usize> =
        history.iter().enumerate().filter(|(_, m)| m["role"] == "tool").map(|(i, _)| i).collect();
    let cutoff = tool_idx.len().saturating_sub(keep_recent);
    let mut saved = 0;
    for &i in &tool_idx[..cutoff] {
        let Some(text) = history[i]["content"].as_str() else { continue };
        if text.len() <= ELIDE_OVER_CHARS || text.contains(ELIDED_MARK) {
            continue;
        }
        let head = crate::ui::truncate(text, head);
        let short =
            format!("{head}\n{ELIDED_MARK}: {} more chars; re-run the tool if you need them]", text.len() - head.len());
        saved += (text.len() - short.len()) / 4;
        history[i]["content"] = json!(short);
    }
    saved
}

/// Picks where to cut history for compaction so that roughly the newest third
/// survives. Cuts land on a user message or on the start of an assistant step,
/// never between a tool call and its result. Cutting mid-turn matters: one
/// long autonomous turn has a single user message and would otherwise never
/// be compactable.
pub fn compaction_split(history: &[Value]) -> Option<usize> {
    let total = estimate(history);
    let mut tail = 0;
    let mut split = None;
    for i in (2..history.len()).rev() {
        tail += estimate_one(&history[i]);
        if matches!(history[i]["role"].as_str(), Some("user" | "assistant")) {
            split = Some(i);
            if tail * 3 >= total {
                break;
            }
        }
    }
    split
}

const COMPACT_PROMPT: &str = "\
Summarise the conversation above so work can continue without it. Be specific and complete; \
this summary replaces the original messages. The user's own messages and a list of files and \
commands are kept separately, word for word, so spend your words on understanding, not inventory.

Write these sections:
1. Task: what the user wants and every constraint or preference they gave, including ones \
they gave early on.
2. Done: what has been completed and the decisions made (and why), with file paths.
3. State: exactly where things stand. What was in progress, open errors, failing tests, \
anything half-edited.
4. Next: the concrete next steps, in order.
5. Facts: commands, names, APIs, findings and dead ends the next steps depend on. Note \
approaches that were tried and failed so they aren't repeated.

After the summary you may add up to 3 lines of long-term memory, each as
MEMORY <kind>: <one sentence>
with <kind> one of preference, fact, decision, gotcha. Only record things that will still be true \
and useful in a fresh session next week: how to build or test, project conventions, traps, and \
preferences the user stated. Never record the current task, what happened in this session, or \
file contents. Writing no MEMORY lines is fine and often right.";

/// The summarisation prompt, optionally steered by what the user wants kept.
pub fn compact_prompt(focus: Option<&str>) -> String {
    match focus.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!(
            "{COMPACT_PROMPT}\n\nThe user asked for this summary to focus on: {f}\n\
             Give that the most detail and keep everything related to it, even if it seems minor."
        ),
        None => COMPACT_PROMPT.to_string(),
    }
}

pub const WORKING_SET_HEADER: &str = "[Working set, from the tool log]";

/// Facts pulled straight from the tool calls, so compaction can't lose them:
/// files changed, files read, recent commands with exit codes, the last error.
pub fn working_set(history: &[Value]) -> String {
    let mut edited: Vec<String> = Vec::new();
    let mut read: Vec<String> = Vec::new();
    let mut commands: Vec<String> = Vec::new();
    let mut last_error = String::new();
    let mut pending: HashMap<String, (String, Value)> = HashMap::new();
    let push = |v: &mut Vec<String>, item: String| {
        v.retain(|x| x != &item);
        v.push(item);
    };
    for m in history {
        // Earlier compactions: keep the files they recorded as changed.
        if let Some(text) = m["content"].as_str() {
            if let Some((_, ws)) = text.split_once(WORKING_SET_HEADER) {
                // Files an earlier compaction found back as they started were
                // still touched; they are checked again each time.
                for prefix in ["- changed: ", TOUCHED_UNCHANGED] {
                    if let Some(line) = ws.lines().find_map(|l| l.strip_prefix(prefix)) {
                        for f in line.split(", ") {
                            push(&mut edited, f.to_string());
                        }
                    }
                }
            }
        }
        for c in m["tool_calls"].as_array().into_iter().flatten() {
            let name = c["function"]["name"].as_str().unwrap_or("").to_string();
            let args: Value =
                serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or_default();
            pending.insert(c["id"].as_str().unwrap_or("").to_string(), (name, args));
        }
        if m["role"] == "tool" {
            let Some((name, args)) = pending.remove(m["tool_call_id"].as_str().unwrap_or("")) else { continue };
            let out = m["content"].as_str().unwrap_or("");
            let path = args["path"].as_str().unwrap_or("").to_string();
            match name.as_str() {
                "edit_file" | "write_file" if !out.starts_with("error") && !out.starts_with("denied") => {
                    push(&mut edited, path)
                }
                "read_file" if !out.starts_with("error") => push(&mut read, path),
                "bash" => {
                    let cmd = args["command"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string();
                    let code = out.lines().next().unwrap_or("").trim_start_matches("exit code: ").to_string();
                    commands.push(format!("`{}` → {}", crate::ui::truncate(&cmd, 120), crate::ui::truncate(&code, 40)));
                    if code != "0" {
                        let tail: String = out.chars().rev().take(700).collect::<Vec<_>>().into_iter().rev().collect();
                        last_error = format!("`{}` failed:\n{}", crate::ui::truncate(&cmd, 120), tail.trim());
                    } else if !last_error.is_empty() && last_error.contains(&crate::ui::truncate(&cmd, 120).to_string())
                    {
                        last_error.clear(); // the same command passes now
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = String::new();
    if !edited.is_empty() {
        out.push_str(&format!("- changed: {}\n", edited.join(", ")));
    }
    if !read.is_empty() {
        let recent: Vec<&str> = read.iter().rev().take(15).rev().map(String::as_str).collect();
        out.push_str(&format!("- read recently: {}\n", recent.join(", ")));
    }
    if !commands.is_empty() {
        out.push_str("- last commands:\n");
        for c in commands.iter().rev().take(8).rev() {
            out.push_str(&format!("  - {c}\n"));
        }
    }
    if !last_error.is_empty() {
        out.push_str(&format!("{UNRESOLVED_ERROR} {last_error}\n"));
    }
    out
}

/// For a compaction the model asked for: where to cut so its last `k`
/// steps (each assistant message with its tool results) stay verbatim. A
/// step that only calls `compact` doesn't count. None when nothing older
/// than those steps is left to summarise.
pub fn tail_split(history: &[Value], k: usize) -> Option<usize> {
    let mut steps = 0;
    for i in (0..history.len()).rev() {
        let m = &history[i];
        if m["role"] != "assistant" {
            continue;
        }
        let calls = m["tool_calls"].as_array().map(Vec::as_slice).unwrap_or_default();
        if !calls.is_empty() && calls.iter().all(|c| c["function"]["name"] == "compact") {
            continue;
        }
        steps += 1;
        if steps == k {
            return (i > 1).then_some(i);
        }
    }
    None
}

/// For a manual /compact: everything. The user's messages and the working
/// set carry over verbatim, so nothing they said or did is lost, and the
/// summary never contradicts a raw tail.
pub fn manual_split(history: &[Value]) -> Option<usize> {
    (history.len() >= 2).then_some(history.len())
}

/// The user's messages from a span of history, verbatim where they fit in
/// `max_chars`. Compaction carries these over. The first message and the
/// newest always survive whole. When the total is too long, older entries
/// are cut to their first line, oldest first; then the middle entries since
/// the last compaction are dropped, oldest first; then, if a long session's
/// carried block still doesn't fit, its middle entries go too, oldest first.
pub fn user_messages(history: &[Value], max_chars: usize) -> String {
    let mut entries: Vec<String> = Vec::new();
    // Entries before this index came from an earlier compaction.
    let mut carried = 0;
    for m in history.iter().filter(|m| m["role"] == "user") {
        let Some(text) = m["content"].as_str() else { continue };
        if text.starts_with("[Summary of the conversation so far]") {
            // Already-carried messages live inside the previous summary block.
            if let Some((_, kept)) = text.split_once(USER_MESSAGES_HEADER) {
                entries.extend(kept.trim().split(CARRY_SEPARATOR).filter(|e| !e.is_empty()).map(String::from));
                carried = entries.len();
            }
            continue;
        }
        if text.starts_with(NOTE) {
            // rusty's own nudges and reviews are not things the user said.
            continue;
        }
        // A separator inside a message would split it when carried again.
        entries.push(crate::ui::truncate(text, 2_000).replace(CARRY_SEPARATOR, "\n- - -\n"));
    }
    let total = |e: &[String]| e.iter().map(|x| x.len() + CARRY_SEPARATOR.len()).sum::<usize>();
    let last = entries.len().saturating_sub(1);
    // Cut older entries to a first line, oldest first; the first and newest stay whole.
    for i in 1..last {
        if total(&entries) <= max_chars {
            break;
        }
        let line = entries[i].lines().next().unwrap_or("");
        let short = crate::ui::truncate(line, 200);
        if short.len() < entries[i].len() {
            entries[i] = format!("{short} …");
        }
    }
    // Then drop middle entries: those since the last compaction first, then
    // carried ones; the first and the newest stay.
    while total(&entries) > max_chars && entries.len() > 2 {
        let newest = entries.len() - 1;
        let i = if carried.max(1) < newest { carried.max(1) } else { 1 };
        entries.remove(i);
        carried = carried.saturating_sub(usize::from(i < carried));
    }
    entries.join(CARRY_SEPARATOR)
}

const CARRY_SEPARATOR: &str = "\n---\n";

pub const USER_MESSAGES_HEADER: &str = "[Your messages so far, verbatim]";

/// Marks a message rusty wrote into the user role (goal nudges, careful-mode
/// reviews), so compaction never carries it over as something the user said.
pub const NOTE: &str = "[from rusty, not the user] ";

/// Splits a compaction reply into the summary and `MEMORY kind: text` lines.
pub fn parse_compaction(reply: &str) -> (String, Vec<(String, String)>) {
    let mut summary = String::new();
    let mut memories = Vec::new();
    for line in reply.lines() {
        let trimmed = line.trim().trim_start_matches(['-', '*', ' ']);
        if let Some(rest) = trimmed.strip_prefix("MEMORY ") {
            if let Some((kind, text)) = rest.split_once(':') {
                memories.push((kind.trim().to_lowercase(), text.trim().to_string()));
                continue;
            }
        }
        summary.push_str(line);
        summary.push('\n');
    }
    (summary.trim().to_string(), memories)
}

/// The memory part of the "compacted" line: `offered` lessons the summary
/// proposed, `saved` the ones the service confirmed, `kept` whether the memory
/// level keeps compaction lessons at all.
pub fn memory_note(offered: usize, saved: usize, known: usize, kept: bool) -> String {
    let plural = |n: usize, one: &str, many: &str| if n == 1 { one.to_string() } else { many.to_string() };
    let mut note = String::new();
    if saved > 0 {
        note.push_str(&format!(", {saved} {} saved", plural(saved, "memory", "memories")));
    }
    if known > 0 {
        note.push_str(&format!(", {known} already known"));
    }
    if note.is_empty() && offered > 0 && !kept {
        note.push_str(&format!(
            ", {offered} {} offered, memory level keeps none",
            plural(offered, "lesson", "lessons")
        ));
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compacted_line_counts_only_memories_the_service_kept() {
        assert_eq!(memory_note(0, 0, 0, true), "");
        assert_eq!(memory_note(3, 3, 0, true), ", 3 memories saved");
        assert_eq!(memory_note(1, 1, 0, true), ", 1 memory saved");
        // One of three refused (forgotten, bad kind): report two.
        assert_eq!(memory_note(3, 2, 0, true), ", 2 memories saved");
        // A near-duplicate only refreshed an existing lesson: not a new row.
        assert_eq!(memory_note(2, 1, 1, true), ", 1 memory saved, 1 already known");
        assert_eq!(memory_note(1, 0, 1, true), ", 1 already known");
        // Proposed at a level that keeps none: say so instead of claiming a save.
        assert_eq!(memory_note(3, 0, 0, false), ", 3 lessons offered, memory level keeps none");
        assert_eq!(memory_note(1, 0, 0, false), ", 1 lesson offered, memory level keeps none");
        assert_eq!(memory_note(3, 0, 0, true), "");
    }

    fn usage(prompt: u64) -> crate::llm::Usage {
        crate::llm::Usage { prompt, completion: 1, ..Default::default() }
    }

    #[test]
    fn the_meter_trusts_reported_prompts_and_calibrates_the_rest() {
        let history: Vec<Value> =
            (0..4).map(|i| json!({"role": "user", "content": "x".repeat(400 * (i + 1))})).collect();
        let mut m = Meter::new(100_000, "flag");
        assert_eq!(m.calibration(), None);
        // Before any report the whole context is estimated, ratio 1.
        assert_eq!(m.fill(&history, 1_000), 1_000 + estimate(&history));
        // The first report sets the ratio: 1,600 reported against 1,000 estimated.
        m.observe(Some(&usage(1_600)), 1_000, 2);
        assert!((m.calibration().unwrap() - 1.6).abs() < 1e-9);
        // Later ones move it 30% of the way: 1.6 → 1.6 * 0.7 + 1.0 * 0.3 = 1.42, then back.
        m.observe(Some(&usage(1_000)), 1_000, 2);
        assert!((m.calibration().unwrap() - 1.42).abs() < 1e-9);
        m.observe(Some(&usage(3_000)), 1_000, 2);
        assert!((m.calibration().unwrap() - (1.42 * 0.7 + 3.0 * 0.3)).abs() < 1e-9);
        let ratio = m.calibration().unwrap();
        // The report stands for the first two messages; the rest is estimated and scaled.
        assert_eq!(m.fill(&history, 1_000), 3_000 + (estimate(&history[2..]) as f64 * ratio).round() as usize);
        // A response without usage keeps that anchor.
        m.observe(None, 5_000, 3);
        assert_eq!(m.fill(&history, 1_000), 3_000 + (estimate(&history[2..]) as f64 * ratio).round() as usize);
        assert_eq!(m.requests.last().map(|r| r.source), Some("estimated"));
        // Extreme ratios are clamped.
        for _ in 0..20 {
            m.observe(Some(&usage(1_000_000)), 1_000, 2);
        }
        assert_eq!(m.calibration(), Some(2.0));
        // A rewritten history drops the report; a request without usage is counted as estimated.
        m.forget_last();
        assert_eq!(m.fill(&history, 0), (estimate(&history) as f64 * 2.0).round() as usize);
        assert_eq!(m.capacity(), 100_000 - m.reserve);
    }

    #[test]
    fn an_overflow_error_corrects_the_window() {
        let mut m = Meter::new(128_000, "assumed");
        assert!(!m.overflow("HTTP 500: upstream timed out"), "unrelated errors change nothing");
        assert!(!m.overflow("prompt is too long"), "no number and nothing accepted yet");
        assert!(
            m.overflow("HTTP 400: This model's maximum context length is 32,768 tokens. However, you requested 40000")
        );
        assert_eq!((m.window, m.window_source), (32_768, "overflow"));
        assert!(!m.overflow("maximum context length is 64000 tokens"), "never raised by an error");
        let mut a = Meter::new(1_000_000, "assumed");
        assert!(a.overflow("prompt is too long: 210000 tokens > 200000 maximum"));
        assert_eq!(a.window, 200_000);
        let mut b = Meter::new(1_000_000, "assumed");
        b.observe(Some(&usage(150_000)), 100_000, 1);
        assert!(b.overflow("model_context_window_exceeded"));
        assert_eq!(b.window, 150_000 + b.reserve, "the largest accepted prompt, plus the reply reserve");
        // A reply that stopped at the window: prompt plus output, when that is the larger bound.
        let mut c = Meter::new(1_000_000, "assumed");
        c.observe(Some(&usage(150_000)), 100_000, 1);
        assert!(c.reached(150_000 + 30_000));
        assert_eq!(c.window, 180_000);
        assert!(!c.reached(200_000), "never raised");
    }

    #[test]
    fn fixed_sizes_scale_with_the_window() {
        // At 128k everything is what rusty always used.
        assert_eq!((kept_tool_results(128_000), elision_head(128_000)), (8, 400));
        assert_eq!(Meter::new(128_000, "flag").reserve, REPLY_RESERVE);
        // Smaller and larger windows scale, within bounds.
        assert_eq!((kept_tool_results(32_000), elision_head(32_000)), (4, 200));
        assert_eq!((kept_tool_results(1_000_000), elision_head(1_000_000)), (16, 800));
        assert_eq!((kept_tool_results(200_000), elision_head(200_000)), (12, 625));
        // A tenth of the history budget, 2k to 16k tokens, as characters.
        assert_eq!(carry_chars(100_000), 40_000);
        assert_eq!(carry_chars(10_000), 8_000);
        assert_eq!(carry_chars(1_000_000), 64_000);
        // The reply reserve: the mode's reply size, at most an eighth of the window, at least 4k.
        let mut m = Meter::new(32_000, "flag");
        assert_eq!(m.reserve, 4_000);
        m.set_reply(16_384);
        assert_eq!(m.reserve, 4_000);
        let mut big = Meter::new(1_000_000, "flag");
        big.set_reply(32_768);
        assert_eq!((big.reserve, big.capacity()), (32_768, 1_000_000 - 32_768));
        big.set_reply(1_000);
        assert_eq!(big.reserve, 4_000);
        // Unchosen, the mode and history cap follow the window.
        assert_eq!(Compaction::for_window(32_000), Compaction::Aggressive);
        assert_eq!(Compaction::for_window(128_000), Compaction::Standard);
        assert_eq!(default_history_cap(200_000), None);
        assert_eq!(default_history_cap(1_000_000), Some(128_000));
    }

    #[test]
    fn compaction_modes_scale_the_trim_and_summary_pair() {
        use Compaction::*;
        assert_eq!(Compaction::default(), Standard);
        assert_eq!((Aggressive.trim_at(), Aggressive.summarize_at()), (40, 60));
        assert_eq!((Standard.trim_at(), Standard.summarize_at()), (60, 80));
        assert_eq!((Passive.trim_at(), Passive.summarize_at()), (75, 95));
        assert_eq!((Smart.trim_at(), Smart.summarize_at()), (60, 80), "standard is smart's backstop");
        for name in Compaction::NAMES.split(", ") {
            assert_eq!(Compaction::parse(name).map(Compaction::name), Some(name));
        }
        assert_eq!(Compaction::parse("eager"), None);
        // Standard fires exactly where rusty always did: budget * 6/10 and * 8/10.
        for budget in [0, 7, 1_000, 96_123, 111_999] {
            assert_eq!(budget * Standard.trim_at() / 100, budget * 6 / 10);
            assert_eq!(budget * Standard.summarize_at() / 100, budget * 8 / 10);
        }
        assert!(SMART_FLOOR < SMART_INVITE && SMART_INVITE < Aggressive.trim_at());
        assert_eq!((percent(25, 100), percent(19, 100), percent(5, 0)), (25, 19, 500));
    }

    #[test]
    fn elides_only_old_large_outputs() {
        let mut h: Vec<Value> = (0..12).map(|i| json!({"role": "tool", "content": "x".repeat(3000 + i)})).collect();
        let saved = elide_old_tool_outputs(&mut h, 128_000);
        assert!(saved > 0);
        assert!(h[0]["content"].as_str().unwrap().contains(ELIDED_MARK));
        assert!(!h[11]["content"].as_str().unwrap().contains(ELIDED_MARK));
        assert_eq!(elide_old_tool_outputs(&mut h, 128_000), 0, "second pass is a no-op");
    }

    #[test]
    fn split_lands_on_user_message() {
        let mut h = Vec::new();
        for i in 0..10 {
            h.push(json!({"role": "user", "content": format!("q{i} {}", "y".repeat(400))}));
            h.push(json!({"role": "assistant", "content": "a".repeat(400)}));
        }
        let s = compaction_split(&h).unwrap();
        assert_ne!(h[s]["role"], "tool");
        assert!(s > 0 && s < h.len());
    }

    #[test]
    fn single_long_turn_is_compactable() {
        let mut h = vec![json!({"role": "user", "content": "do the thing"})];
        for i in 0..10 {
            h.push(json!({"role": "assistant", "content": "", "tool_calls": [{"id": format!("c{i}"), "function": {"arguments": "{}"}}]}));
            h.push(json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": "z".repeat(2000)}));
        }
        let s = compaction_split(&h).unwrap();
        assert_eq!(h[s]["role"], "assistant");
        assert_eq!(h[s - 1]["role"], "tool");
    }

    #[test]
    fn rustys_own_notes_are_not_carried_as_user_messages() {
        let h = vec![
            json!({"role": "user", "content": "keep the API stable"}),
            json!({"role": "user", "content": format!("{NOTE}Second look (1 of 2): check again")}),
        ];
        let carried = user_messages(&h, 8000);
        assert!(carried.contains("keep the API stable") && !carried.contains("Second look"), "{carried}");
    }

    #[test]
    fn user_messages_survive_repeated_compaction() {
        let first =
            vec![json!({"role": "user", "content": "fix the parser"}), json!({"role": "assistant", "content": "ok"})];
        let carried = user_messages(&first, 8000);
        let second = vec![
            json!({"role": "user", "content": format!("[Summary of the conversation so far]\nstuff\n\n{USER_MESSAGES_HEADER}\n{carried}")}),
            json!({"role": "user", "content": "also add tests"}),
        ];
        let again = user_messages(&second, 8000);
        assert!(again.contains("fix the parser") && again.contains("also add tests"));
    }

    #[test]
    fn a_smart_compaction_keeps_the_last_two_steps() {
        let step = |name: &str| json!({"role": "assistant", "content": "", "tool_calls": [{"id": name, "function": {"name": name, "arguments": "{}"}}]});
        let result = |id: &str| json!({"role": "tool", "tool_call_id": id, "content": "ok"});
        let mut h = vec![json!({"role": "user", "content": "go"})];
        for name in ["read_file", "bash", "edit_file"] {
            h.push(step(name));
            h.push(result(name));
        }
        h.push(step("compact"));
        h.push(result("compact"));
        // The compact call doesn't count: bash and edit_file are kept.
        assert_eq!(tail_split(&h, 2), Some(3));
        assert_eq!(tail_split(&h[..5], 2), None, "two steps and nothing older");
        assert_eq!(tail_split(&h, 1), Some(5));
    }

    #[test]
    fn carried_messages_survive_when_new_ones_overflow_the_cap() {
        let summary = |carried: &str| json!({"role": "user", "content": format!("[Summary of the conversation so far]\nstuff\n\n{USER_MESSAGES_HEADER}\n{carried}")});
        let first = vec![
            json!({"role": "user", "content": "fix the parser\nit drops the last field"}),
            json!({"role": "user", "content": "and keep the CLI flags stable"}),
        ];
        let carried = user_messages(&first, 8_000);
        // Since then, more than the cap of new messages.
        let mut history = vec![summary(&carried)];
        for i in 0..6 {
            history.push(json!({"role": "user", "content": format!("step {i}: {}\nmore detail", "x".repeat(1_900))}));
        }
        history.push(json!({"role": "user", "content": "now ship it"}));
        let again = user_messages(&history, 8_000);
        assert!(again.starts_with("fix the parser\nit drops the last field"), "the first message stays whole: {again}");
        assert!(again.contains("and keep the CLI flags stable"), "the carried block survives");
        assert!(again.ends_with("now ship it"), "the newest stays");
        assert!(again.len() <= 8_000, "{}", again.len());
        assert!(
            again.contains("step 0:") && again.contains(" …"),
            "older entries are cut to a first line before any is dropped"
        );
        // A third compaction still carries the first message.
        let third = user_messages(&[summary(&again), json!({"role": "user", "content": "one more"})], 8_000);
        assert!(third.starts_with("fix the parser") && third.ends_with("one more"));

        // A long session: a carried block of forty long entries plus one new message
        // comes back under the cap, first message whole, carried entries dropped oldest first.
        let many: Vec<String> = (0..40).map(|i| format!("request {i}: {}", "y".repeat(400))).collect();
        let block = format!("fix the parser{CARRY_SEPARATOR}{}", many.join(CARRY_SEPARATOR));
        let long = user_messages(&[summary(&block), json!({"role": "user", "content": "and finish"})], 8_000);
        assert!(long.len() <= 8_000, "{}", long.len());
        assert!(long.starts_with("fix the parser") && long.ends_with("and finish"), "{long}");
        assert!(!long.contains("request 0:") && long.contains("request 39:"), "oldest carried entries go first");

        // A message containing the separator stays one entry.
        let hr = user_messages(&[json!({"role": "user", "content": "above\n---\nbelow"})], 8_000);
        assert_eq!(hr.split(CARRY_SEPARATOR).count(), 1, "{hr}");
    }

    #[test]
    fn reverted_files_are_told_apart_from_changed_ones() {
        let ws = "- changed: src/a.rs, src/b.rs, new.txt\n- read recently: src/a.rs\n";
        let out = collapse_reverts(ws, |f| f != "src/b.rs");
        assert_eq!(
            out,
            "- changed: src/b.rs\n- touched, now unchanged: src/a.rs, new.txt\n- read recently: src/a.rs\n"
        );
        assert_eq!(collapse_reverts(ws, |_| false), ws, "nothing reverted, nothing moved");
        // A later compaction re-reads both lines, so a file edited again moves back.
        let summary = format!("[Summary of the conversation so far]\nx\n\n{WORKING_SET_HEADER}\n{out}");
        let again = working_set(&[json!({"role": "user", "content": summary})]);
        assert!(again.contains("src/a.rs") && again.contains("new.txt") && again.starts_with("- changed: "), "{again}");
    }

    #[test]
    fn working_set_tracks_files_commands_and_errors() {
        let call = |id: &str, name: &str, args: Value| json!({"role": "assistant", "content": "", "tool_calls": [{"id": id, "function": {"name": name, "arguments": args.to_string()}}]});
        let res = |id: &str, out: &str| json!({"role": "tool", "tool_call_id": id, "content": out});
        let h = vec![
            json!({"role": "user", "content": "go"}),
            call("1", "read_file", json!({"path": "src/a.rs"})),
            res("1", "1\tfn a()"),
            call("2", "edit_file", json!({"path": "src/a.rs"})),
            res("2", "edited src/a.rs (1 replacement)"),
            call("3", "bash", json!({"command": "cargo test"})),
            res("3", "exit code: 101\nstdout:\ntest a ... FAILED"),
            call("4", "edit_file", json!({"path": "src/missing.rs"})),
            res("4", "error: cannot read src/missing.rs"),
        ];
        let ws = working_set(&h);
        assert!(ws.contains("- changed: src/a.rs\n"), "{ws}");
        assert!(!ws.contains("missing.rs"), "failed edits are not changes");
        assert!(ws.contains("read recently: src/a.rs"));
        assert!(ws.contains("`cargo test` → 101"));
        assert!(ws.contains("last unresolved error") && ws.contains("FAILED"));
        // A later compaction keeps the files an earlier one recorded.
        let again = vec![json!({"role": "user", "content": format!("[Summary]\nx\n\n{WORKING_SET_HEADER}\n{ws}")})];
        assert!(working_set(&again).contains("- changed: src/a.rs"));
    }

    #[test]
    fn manual_split_takes_everything() {
        let h = vec![
            json!({"role": "user", "content": "a"}),
            json!({"role": "assistant", "content": "b"}),
            json!({"role": "user", "content": "c"}),
            json!({"role": "assistant", "content": "d"}),
        ];
        assert_eq!(manual_split(&h), Some(4));
        assert_eq!(manual_split(&h[..1]), None);
        assert!(compact_prompt(Some("the auth bug")).contains("focus on: the auth bug"));
        assert_eq!(compact_prompt(Some("  ")), compact_prompt(None));
    }

    #[test]
    fn parses_memories_out_of_summary() {
        let (s, m) = parse_compaction("1. Task: fix it\nMEMORY gotcha: tests need DB\n- MEMORY fact: use make\n");
        assert_eq!(s, "1. Task: fix it");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].0, "gotcha");
    }
}
