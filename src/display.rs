//! How a turn looks on screen.
//!
//! Reads, searches and listings collapse into one "explored" line, so you
//! see the shape of the work instead of a wall of tool calls. Edits and
//! commands get their own lines. While the model works, a compact status row
//! sits beneath the transcript. Every turn ends with a one-line recap.
//!
//! Views: `default` (as above), `verbose` (every call, every result, the
//! reasoning stream) and `adhd` (one status line, short answers, nothing else).

use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU8, Ordering};
thread_local! { static ACTIVE_FOOTER: RefCell<Weak<RefCell<crate::footer::Footer>>> = const { RefCell::new(Weak::new()) }; }

pub fn refresh_input() {
    if !crate::ui::interactive() {
        return;
    }
    ACTIVE_FOOTER.with(|f| {
        if let Some(footer) = f.borrow().upgrade() {
            let mut out = std::io::stdout().lock();
            footer.borrow_mut().refresh_input(&mut out);
            let _ = out.flush();
        }
    });
}

/// Runtime side results use the terminal owner's footer rather than painting
/// a second progress panel over the active turn.
pub fn notice(text: &str) {
    ACTIVE_FOOTER.with(|f| {
        if let Some(footer) = f.borrow().upgrade() {
            footer.borrow_mut().clear(&mut std::io::stdout());
        }
    });
    println!("\n{}", crate::ui::info(text));
}

use std::time::{Duration, Instant};

use crate::ui;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Default,
    Verbose,
    Adhd,
}

pub const VIEWS: &[&str] = &["default", "verbose", "adhd"];
static VIEW: AtomicU8 = AtomicU8::new(0);

pub fn view() -> View {
    match VIEW.load(Ordering::Relaxed) {
        1 => View::Verbose,
        2 => View::Adhd,
        _ => View::Default,
    }
}

pub fn view_name() -> &'static str {
    VIEWS[VIEW.load(Ordering::Relaxed) as usize % VIEWS.len()]
}

pub fn set_view(name: &str) -> bool {
    let name = if name == "focus" { "adhd" } else { name };
    match VIEWS.iter().position(|v| *v == name) {
        Some(i) => {
            VIEW.store(i as u8, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

pub const READ_TOOLS: &[&str] = &["read_file", "list_files", "search", "locate", "glob", "outline", "recall"];

/// What happened in a turn; feeds the recap and the token tips.
#[derive(Default, Clone, Debug)]
pub struct TurnStats {
    pub reads: usize,
    pub searches: usize,
    pub edits: usize,
    pub commands: usize,
    pub delegated: usize,
    pub visible_chars: usize,
    /// Largest single tool result: (tool, approx tokens).
    pub biggest: (String, usize),
    pub file_reads: HashMap<String, u32>,
    pub tool_tokens: HashMap<String, u64>,
}

pub struct Display {
    quiet: bool,
    verb: &'static str,
    started: Instant,
    last_draw: Instant,
    frame: usize,
    footer: Rc<RefCell<crate::footer::Footer>>,
    in_content: bool,
    in_reasoning: bool,
    /// Something was printed this turn, so the answer gets a blank line above it.
    printed: bool,
    /// The model wrote an answer this turn.
    answered: bool,
    /// Text streamed so far in the current reply (kept for interrupts).
    pub content: String,
    /// The current, not yet complete, line of the reply.
    line_buf: String,
    in_code: bool,
    status: String,
    group: Vec<(String, String)>,
    pub stats: TurnStats,
}

impl Drop for Display {
    fn drop(&mut self) {
        self.clear_spinner();
    }
}

impl Display {
    pub fn new(quiet: bool) -> Self {
        let now = Instant::now();
        let footer = Rc::new(RefCell::new(crate::footer::Footer::default()));
        if !quiet {
            ACTIVE_FOOTER.with(|f| *f.borrow_mut() = Rc::downgrade(&footer));
        }
        Self {
            quiet,
            verb: "Thinking",
            started: now,
            last_draw: now - Duration::from_secs(1),
            frame: 0,
            footer,
            in_content: false,
            in_reasoning: false,
            printed: false,
            answered: false,
            content: String::new(),
            line_buf: String::new(),
            in_code: false,
            status: String::new(),
            group: Vec::new(),
            stats: TurnStats::default(),
        }
    }

    /// Refreshes elapsed time; completed output stays in scrollback.
    pub fn tick(&mut self) {
        if self.quiet || !ui::interactive() || self.in_reasoning {
            return;
        }
        let cadence = 1000;
        if self.last_draw.elapsed() < Duration::from_millis(cadence) {
            return;
        }
        self.last_draw = Instant::now();
        if ui::animate() {
            self.frame += 1;
        }
        self.paint();
    }

    fn paint(&mut self) {
        if self.quiet || !ui::interactive() || self.in_reasoning {
            return;
        }
        let extra = if !self.status.is_empty() {
            self.status.clone()
        } else if !self.group.is_empty() {
            self.group_summary(false)
        } else {
            String::new()
        };
        let verb = if self.in_content { "Responding" } else { self.verb };
        let mut buf = Vec::new();
        self.footer.borrow_mut().draw(&mut buf, self.frame, verb, &extra, self.started.elapsed().as_secs());
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&buf);
        let _ = out.flush();
    }

    fn clear_spinner(&mut self) {
        let mut out = std::io::stdout().lock();
        self.footer.borrow_mut().clear(&mut out);
        let _ = out.flush();
    }

    /// Clear, append and repaint in one write, so logs flow past a steady panel.
    fn append(&mut self, s: &str) {
        let mut buf = Vec::new();
        self.footer.borrow_mut().clear(&mut buf);
        let _ = writeln!(buf, "{s}");
        if ui::interactive() && !self.in_reasoning {
            let extra = if self.status.is_empty() { String::new() } else { self.status.clone() };
            let verb = if self.in_content { "Responding" } else { self.verb };
            self.footer.borrow_mut().draw(&mut buf, self.frame, verb, &extra, self.started.elapsed().as_secs());
        }
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&buf);
        let _ = out.flush();
    }

    pub fn set_status(&mut self, s: String) {
        self.status = s;
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    /// Clears transient output so something else can print cleanly.
    pub fn pause(&mut self) {
        if self.quiet {
            return;
        }
        self.clear_spinner();
        self.flush_group();
    }

    pub fn line(&mut self, s: &str) {
        if self.quiet {
            return;
        }
        self.pause();
        self.append(s);
        self.printed = true;
    }

    pub fn reasoning(&mut self, s: &str) {
        if self.quiet {
            return;
        }
        if view() == View::Verbose {
            self.pause();
            if !self.in_reasoning {
                print!("{} ", ui::dim("┊"));
                self.in_reasoning = true;
            }
            print!("{}", ui::dim(&s.replace('\n', &format!("\n{} ", ui::dim("┊")))));
            let _ = std::io::stdout().flush();
        } else {
            self.tick();
        }
    }

    pub fn content(&mut self, s: &str) {
        self.content.push_str(s);
        self.stats.visible_chars += s.len();
        if self.quiet {
            return;
        }
        if !self.in_content {
            self.pause();
            if self.in_reasoning {
                println!();
                self.in_reasoning = false;
            }
            if self.printed {
                println!();
            }
            self.in_content = true;
            self.printed = true;
            self.answered = true;
        }
        // Render whole lines so markdown styling works mid-stream.
        // Only the new text can hold a line end, and the rendered lines are
        // dropped from the front once: a batch of many lines stays linear.
        let scanned = self.line_buf.len();
        self.line_buf.push_str(s);
        let mut start = 0;
        while let Some(pos) = self.line_buf[start.max(scanned)..].find('\n') {
            let end = start.max(scanned) + pos;
            let rendered = crate::markdown::line(&self.line_buf[start..end], &mut self.in_code);
            self.append(&rendered);
            start = end + 1;
        }
        self.line_buf.drain(..start);
        let _ = std::io::stdout().flush();
    }

    pub fn end_stream(&mut self) {
        if !self.quiet && !self.line_buf.is_empty() {
            let rendered = crate::markdown::line(&self.line_buf, &mut self.in_code);
            self.append(&rendered);
        } else if !self.quiet && self.in_reasoning {
            println!();
        }
        self.line_buf.clear();
        self.in_code = false;
        self.in_content = false;
        self.in_reasoning = false;
        self.content.clear();
        self.status.clear();
    }

    /// Called before a tool runs.
    pub fn tool(&mut self, name: &str, args: &Value) {
        let label = label(name, args);
        match name {
            "read_file" => {
                self.stats.reads += 1;
                *self.stats.file_reads.entry(label.clone()).or_default() += 1;
            }
            "search" | "locate" | "glob" | "outline" | "list_files" | "recall" => self.stats.searches += 1,
            "edit_file" | "write_file" => self.stats.edits += 1,
            "bash" | "bash_start" => self.stats.commands += 1,
            "task" | "swarm" => self.stats.delegated += 1,
            _ => {}
        }
        if self.quiet {
            return;
        }
        let v = view();
        if v == View::Adhd {
            self.status = activity(name, &label);
            return;
        }
        if READ_TOOLS.contains(&name) && v == View::Default {
            self.group.push((name.to_string(), label));
            self.tick();
            return;
        }
        let head = match name {
            "bash" => format!("{} {}", ui::info("▸"), ui::bold(&format!("$ {label}"))),
            _ => format!("{} {} {}", ui::info("▸"), ui::bold(verb_for(name)), ui::dim(&label)),
        };
        self.line(&head);
    }

    /// Called after a tool returns.
    pub fn tool_result(&mut self, name: &str, args: &Value, text: &str, ok: bool) {
        let tokens = text.len() / 4;
        *self.stats.tool_tokens.entry(name.to_string()).or_default() += tokens as u64;
        if tokens > self.stats.biggest.1 {
            self.stats.biggest = (format!("{name} {}", label(name, args)), tokens);
        }
        if self.quiet {
            return;
        }
        let first = text.lines().next().unwrap_or("");
        if !ok || first.starts_with("denied") || first.starts_with("the user declined") || first.starts_with("blocked")
        {
            self.line(&format!("  {} {}", ui::err("└"), ui::err(ui::truncate(first, 160))));
            return;
        }
        let v = view();
        if name == "plan" {
            let plan = render_plan(&args["items"]);
            if !plan.is_empty() {
                self.line(&plan);
            }
            return;
        }
        if v == View::Adhd || (v == View::Default && READ_TOOLS.contains(&name)) {
            return;
        }
        let n = text.lines().count();
        let note = match name {
            "task" | "swarm" => {
                let reports = text.lines().filter(|l| l.starts_with("## ")).count().max(1);
                ui::dim(&format!("{reports} report{} · {} lines back", if reports == 1 { "" } else { "s" }, n))
            }
            "bash" => {
                let code = first.trim_start_matches("exit code: ");
                let mark = if code == "0" { ui::ok("exit 0") } else { ui::err(&format!("exit {code}")) };
                format!("{mark}{}", ui::dim(&format!(" · {} lines", n.saturating_sub(1))))
            }
            "bash_start" | "bash_wait" | "bash_cancel" => {
                let record: Value = serde_json::from_str(text).unwrap_or(Value::Null);
                let status = record["status"].as_str().unwrap_or("unknown");
                if status == "Exited" {
                    let code = record["exit_code"].as_i64().map_or("signal".into(), |n| n.to_string());
                    let lines = ["stdout", "stderr"]
                        .iter()
                        .map(|k| record[k].as_str().unwrap_or("").lines().count())
                        .sum::<usize>();
                    let mark = if code == "0" { ui::ok("exit 0") } else { ui::err(&format!("exit {code}")) };
                    format!("{mark}{}", ui::dim(&format!(" · {lines} output lines")))
                } else {
                    ui::dim(&format!("{} · deadline {}s", status.to_lowercase(), record["timeout_secs"]))
                }
            }
            _ if n > 1 && v == View::Default => ui::dim(&format!("{} (+{} lines)", ui::truncate(first, 100), n - 1)),
            _ => ui::dim(ui::truncate(first, 140)),
        };
        self.line(&format!("  {} {note}", ui::dim("└")));
        if v == View::Verbose {
            for l in text.lines().skip(1).take(6) {
                self.line(&format!("    {}", ui::dim(ui::truncate(l, 140))));
            }
        }
    }

    pub fn flush_group(&mut self) {
        if self.group.is_empty() || self.quiet {
            self.group.clear();
            return;
        }
        self.clear_spinner();
        println!("{} {}", ui::primary("◆"), ui::dim(&self.group_summary(true)));
        self.printed = true;
        self.group.clear();
    }

    fn group_summary(&self, done: bool) -> String {
        if !done {
            let (name, label) = self.group.last().unwrap();
            return activity(name, label);
        }
        let mut parts = Vec::new();
        for (tool, verb) in [
            ("read_file", "read"),
            ("search", "searched"),
            ("locate", "located"),
            ("glob", "found"),
            ("outline", "outlined"),
            ("list_files", "listed"),
            ("recall", "recalled"),
        ] {
            let labels: Vec<&str> = self.group.iter().filter(|(n, _)| n == tool).map(|(_, l)| l.as_str()).collect();
            if labels.is_empty() {
                continue;
            }
            let shown: Vec<String> = labels
                .iter()
                .take(3)
                .map(|l| match tool {
                    // "/pattern/ in path" → "pattern"
                    "search" => {
                        ui::truncate(l.split("/ in ").next().unwrap_or(l).trim_start_matches('/'), 30).to_string()
                    }
                    "glob" | "locate" | "recall" => ui::truncate(l, 30).to_string(),
                    _ => short(l),
                })
                .collect();
            let more = if labels.len() > 3 { format!(" +{}", labels.len() - 3) } else { String::new() };
            parts.push(format!("{verb} {}{more}", shown.join(", ")));
        }
        let line = format!("explored · {}", parts.join(" · "));
        ui::truncate(&line, ui::width().saturating_sub(4)).to_string()
    }

    /// The one-line summary printed when a turn ends.
    pub fn recap(&mut self, prompt: u64, completion: u64, context_pct: usize, interrupted: bool) {
        if self.quiet {
            return;
        }
        self.pause();
        let secs = self.started.elapsed().as_secs_f32();
        let time =
            if secs < 60.0 { format!("{secs:.0}s") } else { format!("{}m{:02}s", secs as u64 / 60, secs as u64 % 60) };
        let mark = if interrupted { ui::err("■ stopped") } else { ui::ok("✓") };
        if self.answered {
            println!();
        }
        if view() == View::Adhd {
            println!("{} {}", mark, ui::dim(&time));
            println!();
            return;
        }
        let s = &self.stats;
        let mut parts = Vec::new();
        for (n, one, many) in [
            (s.reads, "read", "reads"),
            (s.searches, "lookup", "lookups"),
            (s.edits, "edit", "edits"),
            (s.commands, "command", "commands"),
            (s.delegated, "delegation", "delegations"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        }
        parts.push(time);
        if prompt + completion > 0 {
            parts.push(format!("↑{} ↓{}", ui::human(prompt), ui::human(completion)));
            parts.push(format!("ctx {context_pct}%"));
        }
        println!("{} {}", mark, ui::dim(&parts.join(" · ")));
        println!();
    }
}

fn label(name: &str, args: &Value) -> String {
    crate::tools::summary(name, args)
}

fn short(label: &str) -> String {
    let base = label.rsplit('/').next().unwrap_or(label);
    let base = if base.is_empty() { label } else { base };
    ui::truncate(base, 40).to_string()
}

fn verb_for(name: &str) -> &str {
    match name {
        "edit_file" => "edit",
        "write_file" => "write",
        "read_file" => "read",
        "list_files" => "list",
        "task" => "subagent",
        "goal_done" => "goal",
        "bash_start" => "start",
        "bash_wait" => "wait",
        "bash_cancel" => "cancel",
        "loop_next" => "loop",
        other => other,
    }
}

fn activity(name: &str, label: &str) -> String {
    let what = short(label);
    match name {
        "read_file" => format!("reading {what}"),
        "search" => format!("searching {}", ui::truncate(label, 40)),
        "glob" => format!("finding {}", ui::truncate(label, 40)),
        "locate" => format!("locating {}", ui::truncate(label, 40)),
        "outline" => format!("outlining {what}"),
        "list_files" => format!("listing {what}"),
        "recall" => "remembering".into(),
        "edit_file" => format!("editing {what}"),
        "write_file" => format!("writing {what}"),
        "bash" => format!("running {}", ui::truncate(label, 40)),
        "bash_start" => format!("starting {}", ui::truncate(label, 40)),
        "bash_wait" => "waiting for owned command".into(),
        "bash_cancel" => "cancelling owned command".into(),
        "task" => format!("delegating: {}", ui::truncate(label, 40)),
        "swarm" => "dispatching the swarm".into(),
        "plan" => "planning".into(),
        _ => name.to_string(),
    }
}

/// Renders plan items from the `plan` tool's arguments.
pub fn render_plan(items: &Value) -> String {
    let Some(items) = items.as_array() else { return String::new() };
    let total = items.len();
    let done = items.iter().filter(|i| i["status"] == "done").count();
    let current = items.iter().find(|i| i["status"] == "in_progress").and_then(|i| i["text"].as_str());
    if view() == View::Adhd {
        let bar: String = (0..total).map(|i| if i < done { "▰" } else { "▱" }).collect();
        let now = current.map(|c| format!(" · now: {c}")).unwrap_or_default();
        return format!("{} {}", ui::primary(&bar), ui::dim(&format!("{done}/{total}{now}")));
    }
    let mut s = format!("{} {}\n", ui::info("▸"), ui::bold(&format!("plan {done}/{total}")));
    for i in items {
        let text = i["text"].as_str().unwrap_or("");
        let line = match i["status"].as_str() {
            Some("done") => format!("  {} {}", ui::ok("■"), ui::dim(text)),
            Some("in_progress") => format!("  {} {}", ui::warn("▣"), ui::bold(text)),
            _ => format!("  {} {}", ui::dim("□"), text),
        };
        s.push_str(&line);
        s.push('\n');
    }
    s.trim_end().to_string()
}
