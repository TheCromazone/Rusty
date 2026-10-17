//! Slash commands: one registry for `/help`, the live menu under the prompt,
//! Tab completion, and resolving a partly typed command.
//!
//! Typing `/` opens a menu of matching commands under the prompt. ↑ and ↓
//! move the highlight, Tab (or →) completes it, and Enter runs it; a command
//! that needs an argument is filled in instead, so the argument can be typed.
//! After a command and a space, the same menu offers that command's values.

use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hint, Hinter};
use rustyline::validate::Validator;
use rustyline::{Cmd, ConditionalEventHandler, Context, Event, EventContext, Helper, RepeatCount};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use crate::ui;

pub struct Command {
    pub name: &'static str,
    /// Shown after the name; `<…>` marks a required argument.
    pub args: &'static str,
    pub about: &'static str,
    pub section: &'static str,
    /// The first argument's choices, with what each does.
    pub values: &'static [(&'static str, &'static str)],
}

const fn cmd(name: &'static str, args: &'static str, about: &'static str, section: &'static str) -> Command {
    Command { name, args, about, section, values: &[] }
}

const fn with(
    name: &'static str,
    args: &'static str,
    about: &'static str,
    section: &'static str,
    values: &'static [(&'static str, &'static str)],
) -> Command {
    Command { name, args, about, section, values }
}

pub const SECTIONS: [&str; 3] = ["work", "memory & context", "setup"];

/// In menu order: the first match is what Enter runs.
pub const COMMANDS: &[Command] = &[
    cmd("/help", "", "every command, and your skills", "setup"),
    with(
        "/goal",
        "<objective>",
        "keep going until it's done and verified",
        "work",
        &[("status", "the current goal"), ("resume", "pick the goal back up"), ("clear", "drop the goal")],
    ),
    cmd("/loop", "<[5m] [x10] prompt>", "repeat on an interval, or let rusty pick the pace", "work"),
    with(
        "/compact",
        "[focus]",
        "summarise now; say what matters most to keep it in detail",
        "memory & context",
        &[("settings", "when to compact: aggressive · standard · passive · smart")],
    ),
    cmd(
        "/side",
        "new <name> | open <id or name> | list",
        "open a persistent read-only side discussion",
        "memory & context",
    ),
    cmd("/send", "main", "send selected side findings to the main chat", "memory & context"),
    cmd("/back", "", "return to the main chat", "memory & context"),
    cmd("/btw", "[question]", "ask from existing context without changing the main chat", "memory & context"),
    cmd("/context", "", "where the window is going", "memory & context"),
    cmd("/model", "[id]", "switch models, any provider", "setup"),
    with(
        "/mode",
        "[mode]",
        "how hard rusty thinks and checks; permissions stay separate",
        "work",
        &[
            ("auto", "rusty picks per request"),
            ("careful", "think longer; a checker reviews finished work"),
            ("standard", "normal effort, checks what it changed"),
            ("vibe", "quick passes, small checks"),
        ],
    ),
    with(
        "/permissions",
        "[mode]",
        "what runs without asking",
        "setup",
        &[
            ("read-only", "reads, searches and read-only commands"),
            ("ask", "asks before every change"),
            ("auto", "project work runs; the rest asks"),
            ("yolo", "everything runs except deny rules and destructive commands"),
            ("allow", "add an allow rule"),
            ("deny", "add a deny rule"),
            ("check", "see what a command would do"),
        ],
    ),
    with(
        "/memory",
        "[action]",
        "long-term memory",
        "memory & context",
        &[
            ("list", "saved lessons"),
            ("search", "find lessons"),
            ("forget", "remove one"),
            ("status", "the memory service"),
            ("helpful", "credit a lesson"),
            ("harmful", "mark a lesson wrong"),
        ],
    ),
    cmd("/remember", "<text>", "save one (prefix preference:, gotcha:, decision:)", "memory & context"),
    cmd("/forget", "<id>", "remove a saved memory", "memory & context"),
    cmd("/tokens", "", "spend by model and by tool", "memory & context"),
    cmd("/clear", "", "fresh conversation, memories kept", "memory & context"),
    cmd("/resume", "[id|words]", "list saved sessions, search them, or pick one up", "memory & context"),
    with(
        "/agents",
        "[mode]",
        "delegation · default follows /mode",
        "work",
        &[
            ("off", "no delegation"),
            ("sub", "one read-only subagent at a time"),
            ("swarm", "parallel read-only workers"),
            ("auto", "rusty picks the lightest that works"),
            ("default", "hand delegation back to the mode"),
            ("model", "set the subagent model"),
        ],
    ),
    with(
        "/swarm",
        "[setting]",
        "tune parallel workers",
        "work",
        &[("size", "workers at once (up to 16)"), ("models", "rotate models"), ("spread", "temperature spread")],
    ),
    with(
        "/tools",
        "[place]",
        "where tools run; saved for the next session",
        "work",
        &[("local", "on this machine"), ("daytona", "in a Daytona sandbox")],
    ),
    cmd("/plan", "", "the current plan", "work"),
    cmd("/target", "", "the kube context, cloud account, workspace and branch commands will hit", "setup"),
    cmd("/changes", "", "what this session changed in infrastructure", "setup"),
    cmd("/audit", "[n]", "the infrastructure audit log", "setup"),
    cmd("/models", "", "list models from every provider", "setup"),
    cmd("/doctor", "", "which providers are set up and answering", "setup"),
    with(
        "/view",
        "[view]",
        "how much you see",
        "setup",
        &[("default", "the usual"), ("verbose", "everything"), ("adhd", "the least")],
    ),
    with(
        "/theme",
        "[theme]",
        "colours",
        "setup",
        &[("rust", ""), ("neon", ""), ("matrix", ""), ("amber", ""), ("ice", ""), ("mono", "")],
    ),
    with("/font", "[font]", "banner font", "setup", &[("rust", ""), ("block", ""), ("thin", ""), ("classic", "")]),
    with("/tips", "[on|off]", "show or hide tips", "setup", &[("on", ""), ("off", "")]),
    cmd("/settings", "", "everything saved for next time", "setup"),
    cmd("/skills", "", "built-in and your own skills", "setup"),
    cmd("/yolo", "", "permissions to yolo for this session", "setup"),
    cmd("/exit", "", "save and quit", "setup"),
];

/// Commands that end the session or drop the conversation: a partial name
/// never runs them. Enter fills the name in, and piped input must spell it out.
const GUARDED: &[&str] = &["/exit", "/clear"];

/// Other spellings the command handler accepts.
const ALIASES: &[&str] = &[
    "/quit",
    "/q",
    "/?",
    "/new",
    "/providers",
    "/usage",
    "/cost",
    "/adhd",
    "/verbose",
    "/subagents",
    "/perms",
    "/mem",
];

/// Values a command's later arguments can take, after `prior` arguments.
fn nested(name: &str, prior: &[&str]) -> &'static [(&'static str, &'static str)] {
    match (name, prior) {
        ("/compact", ["settings"]) => &[
            ("aggressive", "summarise at 60% of the usable window"),
            ("standard", "summarise at 80% (the default)"),
            ("passive", "summarise at 95%"),
            ("smart", "the model compacts when the work changes"),
        ],
        _ => &[],
    }
}

/// How well `query` matches `name`, best first: a prefix, a substring, then
/// (from three letters) the letters in order. None when it doesn't match.
fn rank(query: &str, name: &str) -> Option<u8> {
    let (q, n) = (query.to_lowercase(), name.to_lowercase());
    if n.starts_with(&q) {
        return Some(0);
    }
    if n.contains(&q) {
        return Some(1);
    }
    // Letters in order catch a typo or a guess, but are noise for one or two.
    let mut letters = n.chars();
    (q.chars().count() >= 3 && q.chars().all(|c| letters.any(|l| l == c))).then_some(2)
}

/// One row of the menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// The whole line once chosen.
    pub line: String,
    pub label: String,
    pub about: String,
    /// Choosing it from a partial line fills it in rather than running it:
    /// it waits for an argument, or it is one of the `GUARDED` commands.
    pub needs_args: bool,
    /// Only the letters match, in order; a typo or a guess.
    pub loose: bool,
}

/// Skills, as (name, description).
pub type Skills = Vec<(String, String)>;

/// The menu for `line`: commands and skills while the first word is typed,
/// then the command's values while its next argument is.
pub fn items(line: &str, skills: &Skills) -> Vec<Item> {
    let Some(body) = line.strip_prefix('/') else { return Vec::new() };
    let words: Vec<&str> = body.split(' ').collect();
    if words.len() == 1 {
        let mut ranked: Vec<(u8, usize, Item)> = Vec::new();
        for (i, c) in COMMANDS.iter().enumerate() {
            if let Some(r) = rank(body, &c.name[1..]) {
                let label = if c.args.is_empty() { c.name.to_string() } else { format!("{} {}", c.name, c.args) };
                let item = Item {
                    line: c.name.to_string(),
                    label,
                    about: c.about.to_string(),
                    needs_args: c.args.starts_with('<') || GUARDED.contains(&c.name),
                    loose: r == 2,
                };
                ranked.push((r, i, item));
            }
        }
        for (i, (name, about)) in skills.iter().enumerate() {
            if COMMANDS.iter().any(|c| c.name[1..] == *name) {
                continue;
            }
            if let Some(r) = rank(body, name) {
                let item = Item {
                    line: format!("/{name}"),
                    label: format!("/{name} [args]"),
                    about: format!("skill · {about}"),
                    needs_args: false,
                    loose: r == 2,
                };
                ranked.push((r, COMMANDS.len() + i, item));
            }
        }
        ranked.sort_by_key(|(r, i, _)| (*r, *i));
        return ranked.into_iter().map(|(_, _, item)| item).collect();
    }
    let name = format!("/{}", words[0]);
    let Some(c) = COMMANDS.iter().find(|c| c.name == name) else { return Vec::new() };
    let (prior, typed) = (&words[1..words.len() - 1], words[words.len() - 1]);
    let values = if prior.is_empty() { c.values } else { nested(&name, prior) };
    let mut ranked: Vec<(u8, usize, Item)> = values
        .iter()
        .enumerate()
        .filter_map(|(i, (v, about))| {
            let r = rank(typed, v)?;
            let mut head = name.clone();
            for p in prior {
                head.push(' ');
                head.push_str(p);
            }
            let item = Item {
                line: format!("{head} {v}"),
                label: (*v).to_string(),
                about: (*about).to_string(),
                needs_args: false,
                loose: r == 2,
            };
            Some((r, i, item))
        })
        .collect();
    ranked.sort_by_key(|(r, i, _)| (*r, *i));
    ranked.into_iter().map(|(_, _, item)| item).collect()
}

/// Whether the line already names a command, alias or skill exactly, with
/// any argument either a known value or free text.
fn is_exact(line: &str, skills: &Skills) -> bool {
    let first = line.split(' ').next().unwrap_or("");
    let known = COMMANDS.iter().any(|c| c.name == first)
        || ALIASES.contains(&first)
        || skills.iter().any(|(n, _)| first.strip_prefix('/') == Some(n.as_str()));
    if !known {
        return false;
    }
    let rest = line[first.len()..].trim();
    rest.is_empty() || items(line, skills).iter().any(|i| i.line == line) || items(line, skills).is_empty()
}

/// What Enter runs for `line`. When the menu was on screen for exactly this
/// line, its highlighted row; otherwise the line itself when it is already a
/// command, else only a prefix that matches one command (piped input, or a
/// line typed faster than the menu).
pub fn resolve(line: &str, shown: Option<(&str, &[Item], Option<usize>)>, skills: &Skills) -> String {
    if !line.starts_with('/') {
        return line.to_string();
    }
    if let Some((seen, rows, selected)) = shown {
        if seen == line {
            if let Some(item) = selected.and_then(|i| rows.get(i)) {
                return item.line.clone();
            }
        }
    }
    if is_exact(line, skills) {
        return line.to_string();
    }
    let close: Vec<Item> =
        items(line, skills).into_iter().filter(|i| !i.loose && !GUARDED.contains(&i.line.as_str())).collect();
    let prefixes: Vec<&Item> = close.iter().filter(|i| i.line.starts_with(line)).collect();
    match prefixes.as_slice() {
        [only] => only.line.clone(),
        _ => line.to_string(),
    }
}

/// What to offer for a word that isn't a command: the commands it starts
/// (up to three), else the nearest spelling.
pub fn suggest(word: &str, skills: &Skills) -> Option<String> {
    let starts: Vec<String> =
        items(word, skills).into_iter().filter(|i| i.line.starts_with(word)).map(|i| i.line).take(3).collect();
    if !starts.is_empty() {
        return Some(starts.join(", "));
    }
    let word = word.trim_start_matches('/');
    let names = COMMANDS.iter().map(|c| c.name[1..].to_string()).chain(skills.iter().map(|(n, _)| n.clone()));
    names
        .map(|n| (distance(word, &n), n))
        .filter(|(d, n)| *d <= 2.max(n.len() / 4) && *d < n.len())
        .min_by_key(|(d, _)| *d)
        .map(|(_, n)| format!("/{n}"))
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == *cb { prev } else { 1 + prev.min(row[j]).min(row[j + 1]) };
            prev = cur;
        }
    }
    row[b.len()]
}

/// `/help`, from the same registry as the menu.
pub fn help(skills: &[(String, String)]) -> String {
    let mut out = String::new();
    for section in SECTIONS {
        out.push_str(&format!("  {}\n", ui::accent(section)));
        for c in COMMANDS.iter().filter(|c| c.section == section) {
            let head = if c.args.is_empty() { c.name.to_string() } else { format!("{} {}", c.name, c.args) };
            let values = c.values.iter().map(|(v, _)| *v).collect::<Vec<_>>().join(" · ");
            let about = if values.is_empty() { c.about.to_string() } else { format!("{} · {values}", c.about) };
            out.push_str(&format!("    {} {}\n", ui::info(&format!("{head:<28}")), ui::dim(&about)));
        }
    }
    out.push_str(&format!("  {}\n", ui::accent("skills")));
    for (name, about) in skills {
        out.push_str(&format!("    {} {}\n", ui::info(&format!("{:<28}", format!("/{name} [args]"))), ui::dim(about)));
    }
    out.push_str(&format!(
        "  {}\n",
        ui::dim(
            "type / for the menu: ↑↓ choose · tab completes · enter runs · <…> is a required argument, and a \
             command typed bare without it shows its status or usage · end a line with \\ for more lines"
        )
    ));
    out
}

/// What the menu showed last, shared between the hint and the key handlers.
#[derive(Default)]
pub struct Menu {
    pub line: String,
    pub rows: Vec<Item>,
    /// The highlighted row; none until ↑↓ when no value has been typed yet,
    /// since Enter then runs the bare command.
    pub selected: Option<usize>,
    /// Off while a continuation line is read.
    pub paused: bool,
}

pub type Shared = Arc<Mutex<Menu>>;

/// The line editor's helper: draws the menu as a hint under the input.
pub struct SlashHelper {
    pub menu: Shared,
    pub skills: Skills,
}

const SHOWN: usize = 8;

pub struct MenuHint {
    display: String,
    completion: Option<String>,
}

impl Hint for MenuHint {
    fn display(&self) -> &str {
        &self.display
    }
    fn completion(&self) -> Option<&str> {
        self.completion.as_deref()
    }
}

impl SlashHelper {
    /// The menu text for `line`, and the rest of the highlighted row's line
    /// that → would insert.
    fn render(&self, line: &str, menu: &mut Menu) -> Option<MenuHint> {
        if menu.line != line {
            menu.line = line.to_string();
            menu.rows = items(line, &self.skills);
            menu.selected = (!line.ends_with(' ')).then_some(0);
        }
        let first = line.split(' ').next().unwrap_or("");
        let command = COMMANDS.iter().find(|c| c.name == first);
        if menu.rows.is_empty() {
            // A command and a space: say what it takes.
            return match command {
                Some(c) if line == format!("{} ", c.name) && !c.args.is_empty() => {
                    Some(MenuHint { display: ui::dim(c.args), completion: None })
                }
                None if !line.contains(' ') && line.len() > 1 => Some(MenuHint {
                    display: format!("\n  {}", ui::dim("no command matches · /help lists them")),
                    completion: None,
                }),
                _ => None,
            };
        }
        if menu.rows.len() == 1 && menu.rows[0].line == line {
            return None;
        }
        let selected = menu.selected.and_then(|i| menu.rows.get(i));
        let rest = selected.and_then(|s| s.line.strip_prefix(line)).map(str::to_string);
        let mut display = match (&rest, command) {
            (Some(r), _) => ui::dim(r),
            // Nothing chosen yet after a command: say what it takes.
            (None, Some(c)) => ui::dim(c.args),
            (None, None) => String::new(),
        };
        let width = ui::width().saturating_sub(4).max(40);
        let start = menu.selected.unwrap_or(0).saturating_sub(SHOWN - 1);
        for (i, row) in menu.rows.iter().enumerate().skip(start).take(SHOWN) {
            let label = format!("{:<26}", row.label);
            let about = ui::truncate(&row.about, width.saturating_sub(30)).to_string();
            if Some(i) == menu.selected {
                display.push_str(&format!("\n {} {} {}", ui::primary("›"), ui::info(&label), ui::dim(&about)));
            } else {
                display.push_str(&format!("\n   {} {}", label, ui::dim(&about)));
            }
        }
        let more = menu.rows.len().saturating_sub(start + SHOWN);
        let action = match selected {
            Some(row) if row.needs_args => "enter fills it in".to_string(),
            Some(_) => "enter runs".to_string(),
            None => format!("enter runs {}", line.trim_end()),
        };
        let footer = if more > 0 {
            format!("{more} more · ↑↓ choose · tab completes · {action}")
        } else {
            format!("↑↓ choose · tab completes · {action}")
        };
        display.push_str(&format!("\n   {}", ui::dim(&footer)));
        Some(MenuHint { display, completion: rest.filter(|r| !r.is_empty()) })
    }
}

impl Hinter for SlashHelper {
    type Hint = MenuHint;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<MenuHint> {
        if pos != line.len() || !line.starts_with('/') {
            return None;
        }
        let mut menu = self.menu.lock().ok()?;
        if menu.paused {
            return None;
        }
        self.render(line, &mut menu)
    }
}

impl Highlighter for SlashHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Owned(ui::user(line))
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, _default: bool) -> Cow<'b, str> {
        Cow::Owned(ui::user(prompt))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        // Include ordinary text and continuation lines in every redraw,
        // including the final redraw on Enter. Wrap length is not a state.
        true
    }
}

/// Tab, and Enter on a command that needs an argument: the highlighted row,
/// with a space when an argument or a value can follow.
impl Completer for SlashHelper {
    type Candidate = String;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<String>)> {
        let Ok(menu) = self.menu.lock() else { return Ok((0, Vec::new())) };
        if menu.paused || menu.line != line || pos != line.len() {
            return Ok((0, Vec::new()));
        }
        let Some(row) = menu.rows.get(menu.selected.unwrap_or(0)) else { return Ok((0, Vec::new())) };
        let words: Vec<&str> = row.line.split(' ').collect();
        let takes_more = COMMANDS.iter().any(|c| c.name == row.line && (!c.args.is_empty() || !c.values.is_empty()))
            || !nested(words[0], &words[1..]).is_empty();
        let text = if takes_more { format!("{} ", row.line) } else { row.line.clone() };
        Ok((0, vec![text]))
    }
}

impl Validator for SlashHelper {}

impl Helper for SlashHelper {}

/// ↑ and ↓ move the highlight while the menu is open; otherwise history.
pub struct Move {
    pub menu: Shared,
    pub down: bool,
}

impl ConditionalEventHandler for Move {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext) -> Option<Cmd> {
        let mut menu = self.menu.lock().ok()?;
        let n = menu.rows.len();
        if menu.paused || menu.line != ctx.line() || n == 0 || (n == 1 && menu.selected.is_some()) {
            return None;
        }
        menu.selected = Some(match (menu.selected, self.down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        });
        Some(Cmd::Repaint)
    }
}

/// Enter on a command that needs an argument fills it in rather than
/// running it bare; everything else is submitted and resolved by the REPL.
pub struct Submit {
    pub menu: Shared,
    pub skills: Skills,
}

impl ConditionalEventHandler for Submit {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext) -> Option<Cmd> {
        let menu = self.menu.lock().ok()?;
        let line = ctx.line();
        if menu.paused || menu.line != line || is_exact(line, &self.skills) {
            return None;
        }
        let row = menu.rows.get(menu.selected?)?;
        row.needs_args.then_some(Cmd::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(rows: &[Item]) -> Vec<&str> {
        rows.iter().map(|i| i.line.as_str()).collect()
    }

    #[test]
    fn the_menu_ranks_prefixes_then_substrings_then_letters() {
        let none = Skills::new();
        assert_eq!(lines(&items("/co", &none))[..2], ["/compact", "/context"]);
        assert_eq!(items("/", &none).len(), COMMANDS.len(), "a bare slash lists everything");
        assert_eq!(items("/", &none)[0].line, "/help");
        // A substring, then a guess with the letters in order.
        let rows = items("/text", &none);
        assert_eq!(rows[0].line, "/context");
        assert!(!rows[0].loose);
        let rows = items("/cmpct", &none);
        assert_eq!(rows[0].line, "/compact");
        assert!(rows[0].loose);
        assert!(items("/zzz", &none).is_empty());
    }

    #[test]
    fn values_follow_the_command_and_nest() {
        let none = Skills::new();
        assert_eq!(lines(&items("/mode ", &none)), ["/mode auto", "/mode careful", "/mode standard", "/mode vibe"]);
        assert_eq!(lines(&items("/mode ca", &none)), ["/mode careful"]);
        assert_eq!(lines(&items("/compact settings sm", &none)), ["/compact settings smart"]);
        assert!(items("/goal fix the parser", &none).is_empty(), "free text gets no menu");
    }

    #[test]
    fn skills_join_the_menu_after_commands() {
        let skills = vec![("review".to_string(), "review the diff".to_string())];
        let rows = items("/re", &skills);
        assert_eq!(lines(&rows), ["/remember", "/resume", "/review"]);
        assert!(rows[2].about.starts_with("skill"));
    }

    #[test]
    fn enter_runs_what_was_highlighted_or_a_single_prefix() {
        let none = Skills::new();
        let rows = items("/co", &none);
        // The menu was on screen and the user moved to the second row.
        assert_eq!(resolve("/co", Some(("/co", &rows, Some(1))), &none), "/context");
        // Without the menu (piped input), only an unambiguous prefix resolves.
        assert_eq!(resolve("/co", None, &none), "/co");
        assert_eq!(resolve("/cont", None, &none), "/context");
        assert_eq!(resolve("/cmpct", None, &none), "/cmpct", "a guess never runs unseen");
        // Exact commands, aliases, free-text arguments and plain text pass through.
        for line in ["/compact", "/q", "/goal fix the parser", "/mode careful", "hello /co"] {
            assert_eq!(resolve(line, None, &none), line);
        }
        assert_eq!(resolve("/mode ca", None, &none), "/mode careful");
        // A partial name never quits or clears, even as the only prefix.
        assert_eq!(resolve("/ex", None, &none), "/ex");
        assert_eq!(resolve("/cl", None, &none), "/cl");
        assert!(items("/cl", &none)[0].needs_args, "Enter fills /clear in instead of running it");
        // A row picked after a command and a space wins over the bare command.
        let values = items("/compact ", &none);
        assert_eq!(resolve("/compact ", Some(("/compact ", &values, Some(0))), &none), "/compact settings");
        assert_eq!(resolve("/compact ", Some(("/compact ", &values, None)), &none), "/compact ");
        // A menu shown for another line is ignored.
        assert_eq!(resolve("/cont", Some(("/co", &rows, Some(1))), &none), "/context");
    }

    #[test]
    fn unknown_words_get_the_nearest_command() {
        let none = Skills::new();
        assert_eq!(suggest("/compcat", &none).as_deref(), Some("/compact"));
        assert_eq!(suggest("/hlep", &none).as_deref(), Some("/help"));
        assert_eq!(suggest("/frobnicate", &none), None);
        assert_eq!(suggest("/co", &none).as_deref(), Some("/compact, /context"));
    }

    #[test]
    fn the_hint_shows_the_rest_and_the_menu() {
        let helper = SlashHelper { menu: Shared::default(), skills: Skills::new() };
        let mut menu = Menu::default();
        let hint = helper.render("/comp", &mut menu).unwrap();
        assert_eq!(hint.completion(), Some("act"));
        let plain = strip(hint.display());
        assert!(plain.starts_with("act\n"), "{plain}");
        assert!(plain.contains("› /compact [focus]"), "{plain}");
        assert!(plain.contains("enter runs"), "{plain}");
        let hint = helper.render("/go", &mut menu).unwrap();
        assert!(strip(hint.display()).contains("enter fills it in"));
        // A command and a space show what it takes; an exact match shows nothing more.
        let hint = helper.render("/remember ", &mut menu).unwrap();
        assert_eq!(strip(hint.display()), "<text>");
        // After a command and a space nothing is highlighted, so Enter runs it bare.
        let plain = strip(helper.render("/compact ", &mut menu).unwrap().display());
        assert!(plain.starts_with("[focus]\n   settings"), "{plain}");
        assert!(plain.contains("enter runs /compact"), "{plain}");
        menu.selected = Some(0);
        let hint = helper.render("/compact ", &mut menu).unwrap();
        assert_eq!(hint.completion(), Some("settings"));
        assert!(helper.render("/clear", &mut menu).is_none());
        assert!(strip(helper.render("/zzz", &mut menu).unwrap().display()).contains("no command matches"));
    }

    #[test]
    fn help_lists_every_command_once() {
        let text = strip(&help(&[]));
        for c in COMMANDS {
            let rows = text.lines().filter(|l| l.trim_start().split(' ').next() == Some(c.name)).count();
            assert_eq!(rows, 1, "{}", c.name);
        }
    }

    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            match (esc, c) {
                (_, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }
}
