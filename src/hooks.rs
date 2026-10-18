//! User hooks: shell commands that run before and after tool calls.
//!
//! They come only from `~/.config/rusty/hooks.json`, never from the project,
//! so opening a cloned repository can't run its commands on this machine.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

use crate::signal;
use crate::tools::{self, ShellCapture};
use crate::ui;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 600;
/// The most of a hook's stderr the model is shown.
const MAX_REASON: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    PreToolUse,
    PostToolUse,
}

impl Event {
    pub fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
        }
    }
}

/// Unknown keys are refused, so a misspelt event is a warning rather than a
/// hook that silently never runs.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(rename = "PreToolUse", default)]
    pre: Vec<Hook>,
    #[serde(rename = "PostToolUse", default)]
    post: Vec<Hook>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hook {
    /// Tool names separated by `|`; missing or `*` matches every tool.
    #[serde(default)]
    matcher: Option<String>,
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl Hook {
    fn matches(&self, tool: &str) -> bool {
        match self.matcher.as_deref().map(str::trim) {
            None | Some("" | "*") => true,
            Some(names) => names.split('|').any(|name| name.trim() == tool),
        }
    }
}

/// What a hook said, when it said more than "go ahead" (exit 0).
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 2, with the hook's stderr: block the call before it runs, or a
    /// note for the model after it ran.
    Exit2(String),
    /// Any other failure. The user is told and the call goes on.
    Failed(String),
}

impl Hooks {
    /// The user's hooks. A file that can't be read or parsed is reported
    /// once and runs no hooks at all.
    pub fn user() -> Hooks {
        let Some(path) = crate::config::config_dir().map(|d| d.join("hooks.json")) else {
            return Hooks::default();
        };
        Self::load(&path).unwrap_or_else(|e| {
            eprintln!("{}", ui::warn(&format!("◇ hooks off: {e:#}")));
            Hooks::default()
        })
    }

    /// A missing file means no hooks.
    pub fn load(path: &Path) -> Result<Hooks> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Hooks::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let hooks: Hooks = serde_json::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
        if hooks.pre.iter().chain(&hooks.post).any(|h| h.command.trim().is_empty()) {
            bail!("{}: every hook needs a command", path.display());
        }
        Ok(hooks)
    }

    fn of(&self, event: Event) -> &[Hook] {
        match event {
            Event::PreToolUse => &self.pre,
            Event::PostToolUse => &self.post,
        }
    }

    pub fn matches(&self, event: Event, tool: &str) -> bool {
        self.of(event).iter().any(|h| h.matches(tool))
    }

    /// Runs the event's hooks for `tool` in order, each with `input` as JSON
    /// on its stdin. Before a call, the first block ends the run.
    pub fn run(&self, event: Event, tool: &str, input: &Value, dir: Option<&Path>) -> Vec<Outcome> {
        let input = input.to_string();
        let mut outcomes = Vec::new();
        for hook in self.of(event).iter().filter(|h| h.matches(tool)) {
            let secs = hook.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, MAX_TIMEOUT_SECS);
            let outcome = match tools::run_hook(&hook.command, dir, &input, Duration::from_secs(secs)) {
                Ok(capture) => judge(&hook.command, capture, secs),
                Err(e) => Some(Outcome::Failed(format!("`{}` did not start: {e:#}", ui::truncate(&hook.command, 60)))),
            };
            let block = event == Event::PreToolUse && matches!(outcome, Some(Outcome::Exit2(_)));
            outcomes.extend(outcome);
            if block {
                break;
            }
        }
        outcomes
    }
}

fn judge(command: &str, capture: ShellCapture, secs: u64) -> Option<Outcome> {
    let stderr = capture.stderr.trim();
    let why = match capture.status {
        Some(Some(0)) => return None,
        Some(Some(2)) => {
            let reason = if stderr.is_empty() { "no reason given" } else { ui::truncate(stderr, MAX_REASON) };
            return Some(Outcome::Exit2(reason.to_string()));
        }
        Some(Some(code)) => format!("exit {code}"),
        Some(None) => "killed by a signal".to_string(),
        None if signal::interrupted() => "interrupted".to_string(),
        None => format!("timed out after {secs}s"),
    };
    let detail = stderr.lines().next().map(|l| format!(": {}", ui::truncate(l.trim(), 120))).unwrap_or_default();
    Some(Outcome::Failed(format!("`{}` failed ({why}){detail}", ui::truncate(command, 60))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rusty-hooks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn hooks(value: Value) -> Hooks {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn matchers_name_exact_tools() {
        let h = hooks(json!({
            "PreToolUse": [{"matcher": "edit_file | write_file", "command": "true"}],
            "PostToolUse": [{"command": "true"}, {"matcher": "mcp__docs__search", "command": "true"}],
        }));
        assert!(h.matches(Event::PreToolUse, "edit_file") && h.matches(Event::PreToolUse, "write_file"));
        assert!(!h.matches(Event::PreToolUse, "write") && !h.matches(Event::PreToolUse, "read_file"));
        assert!(h.matches(Event::PostToolUse, "anything"), "a missing matcher matches every tool");
        let star = hooks(json!({"PreToolUse": [{"matcher": "*", "command": "true"}]}));
        assert!(star.matches(Event::PreToolUse, "mcp__docs__search"));
        assert!(!star.matches(Event::PostToolUse, "bash"));
    }

    #[test]
    fn a_missing_file_is_no_hooks_and_a_bad_one_is_an_error() {
        let dir = temp("load");
        let path = dir.join("hooks.json");
        assert!(!Hooks::load(&path).unwrap().matches(Event::PreToolUse, "bash"));
        std::fs::write(&path, r#"{"PreToolUse": [{"matcher": "bash", "command": "check", "timeout_secs": 5}]}"#)
            .unwrap();
        assert!(Hooks::load(&path).unwrap().matches(Event::PreToolUse, "bash"));
        for bad in [
            "{not json",
            r#"{"PretoolUse": [{"command": "check"}]}"#,
            r#"{"PreToolUse": [{"command": "check", "matchers": "bash"}]}"#,
            r#"{"PostToolUse": [{"command": "  "}]}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(Hooks::load(&path).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exit_codes_decide_what_a_hook_means() {
        let dir = temp("run");
        let h = hooks(json!({
            "PreToolUse": [
                {"command": "cat > input.json; echo fine"},
                {"matcher": "bash", "command": "echo flaky >&2; exit 1"},
                {"matcher": "bash", "command": "printf '  no rm here \\n' >&2; exit 2"},
                {"matcher": "bash", "command": "touch never-ran"},
            ],
            "PostToolUse": [{"command": "sleep 5", "timeout_secs": 1}, {"command": "exit 2"}],
        }));
        let input = json!({"event": "PreToolUse", "tool": "bash", "args": {"command": "rm -rf /"}});
        let outcomes = h.run(Event::PreToolUse, "bash", &input, Some(&dir));
        assert_eq!(
            outcomes,
            [
                Outcome::Failed("`echo flaky >&2; exit 1` failed (exit 1): flaky".into()),
                Outcome::Exit2("no rm here".into())
            ]
        );
        let seen: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("input.json")).unwrap()).unwrap();
        assert_eq!(seen, input, "the hook reads the call on stdin, in the given directory");
        assert!(!dir.join("never-ran").exists(), "a block ends the run");
        assert!(h.run(Event::PreToolUse, "read_file", &input, Some(&dir)).is_empty());
        let after = h.run(Event::PostToolUse, "bash", &input, Some(&dir));
        assert_eq!(
            after,
            [Outcome::Failed("`sleep 5` failed (timed out after 1s)".into()), Outcome::Exit2("no reason given".into())]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
