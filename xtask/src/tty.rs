//! Real-terminal checks: rusty runs on a pseudo-terminal while we type at it
//! and watch what it draws.
//!
//!     cargo xtask tty [ctrl-c|approval|steady]... [--binary PATH] [--live]
//!     cargo xtask tty-capture --binary PATH --output DIR
//!     cargo xtask tty-inspect DIR
//!
//! `approval` and `steady` always talk to a local stand-in provider. `ctrl-c`
//! does too unless `--live`, which uses the configured model instead.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::json::quote;
use crate::pattern::{find, Piece, Piece::*};
use crate::provider::Provider;
use crate::pty::{Chunk, Pty, Utf8};
use crate::screen::{Color, Event, Pen, Screen};

pub const WIDTHS: [u16; 4] = [24, 40, 80, 120];
const ROWS: u16 = 24;

/// rusty on a terminal, with output kept the way `expect` keeps it: each
/// match consumes the output up to its end.
struct Session {
    pty: Pty,
    text: String,
    from: usize,
    utf8: Utf8,
    log: Box<dyn Write>,
    timeout: Duration,
}

impl Session {
    fn spawn(cmd: Command, cols: u16, log: Box<dyn Write>, timeout: Duration) -> Result<Session> {
        let pty = Pty::spawn(cmd, ROWS, cols)?;
        Ok(Session { pty, text: String::new(), from: 0, utf8: Utf8::default(), log, timeout })
    }

    /// Reads for up to `wait`; false once the terminal is closed.
    fn pump(&mut self, wait: Duration) -> Result<bool> {
        match self.pty.read(wait)? {
            Chunk::Data(bytes) => {
                self.log.write_all(&bytes)?;
                self.log.flush()?;
                self.text.push_str(&self.utf8.decode(&bytes));
                Ok(true)
            }
            Chunk::Timeout => Ok(true),
            Chunk::Eof => Ok(false),
        }
    }

    /// Waits for the first of `pats` (tried in order) to appear; its index,
    /// or None on timeout or end of output.
    fn expect(&mut self, pats: &[&[Piece]], timeout: Duration) -> Result<Option<usize>> {
        let end = Instant::now() + timeout;
        loop {
            for (i, pat) in pats.iter().enumerate() {
                if let Some((_, stop)) = find(pat, &self.text[self.from..]) {
                    self.from += stop;
                    return Ok(Some(i));
                }
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.pump(left.min(Duration::from_millis(100)))? {
                return Ok(None);
            }
        }
    }

    fn wait_for(&mut self, pat: &[Piece], fail: &str) -> Result<()> {
        match self.expect(&[pat], self.timeout)? {
            Some(_) => Ok(()),
            None => bail!("{fail}"),
        }
    }

    /// Keeps reading for `secs` without matching anything.
    fn drain(&mut self, secs: f64) -> Result<()> {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if !self.pump(left)? {
                break;
            }
        }
        Ok(())
    }

    fn send(&mut self, text: &str) -> Result<()> {
        self.pty.send(text.as_bytes())
    }

    /// Waits for the end of output and a zero exit status.
    fn finish(&mut self) -> Result<()> {
        let end = Instant::now() + self.timeout;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if !self.pump(left)? {
                break;
            }
        }
        let status = self.pty.wait(Duration::from_secs(5)).context("rusty kept running after its output ended")?;
        if !status.success() {
            bail!("rusty exited {status}");
        }
        Ok(())
    }
}

/// The environment for talking to the stand-in provider from `home`.
fn offline(cmd: &mut Command, endpoint: &str, home: &Path) {
    cmd.env("RUSTY_BASE_URL", endpoint)
        .env("NVIDIA_API_KEY", "offline-test-key")
        .env("RUSTY_NO_DOTENV", "1")
        .env("RUSTY_HOME", home)
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR");
}

/// One run at `cols` columns: twelve real tool calls, then a streamed reply.
fn tools_capture(binary: &Path, endpoint: &str, cols: u16, dest: &Path, prompt: &str) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.current_dir(dest).args(["--yolo", "--mode", "standard", "--agents", "off", "--memory", "off", "--trajectory"]);
    cmd.arg(dest.join(format!("trajectory-{cols}.json"))).arg(prompt);
    offline(&mut cmd, endpoint, &dest.join("home"));
    let log = File::create(dest.join(format!("terminal-{cols}.ansi")))?;
    let mut s = Session::spawn(cmd, cols, Box::new(log), Duration::from_secs(30))?;
    if matches!(prompt, "aside-capture" | "side-capture" | "summary-capture") {
        let end = Instant::now() + Duration::from_secs(5);
        while !dest.join("main-started.txt").exists() && Instant::now() < end {
            s.pump(Duration::from_millis(20))?;
        }
        if !dest.join("main-started.txt").exists() {
            bail!("main command did not start");
        }
        if prompt == "side-capture" || prompt == "summary-capture" {
            std::fs::write(dest.join("side-fact.txt"), "SIDE_FACT")?;
            s.send("/side new terminal\r")?;
            s.wait_for(&[Lit("read-only, live workspace")], "no persistent side selected")?;
            s.send("side-sentinel\r")?;
            s.wait_for(&[Lit("SIDE_ANSWER")], "no persistent side answer")?;
            if prompt == "summary-capture" {
                s.send("/send main\r")?;
                s.wait_for(&[Lit("summary queued for main")], "summary was not queued while main ran")?;
                s.send("/send main\r")?;
                s.wait_for(&[Lit("already sent")], "summary revision was not deduplicated")?;
            } else {
                s.send("side-cancel\r")?;
                s.wait_for(&[Lit("side question running")], "second side did not start")?;
                s.send("\x03")?;
                s.wait_for(&[Lit("side turn interrupted")], "Ctrl-C did not stop only the selected side")?;
            }
            s.send("/back\r")?;
            s.wait_for(&[Lit("main chat selected")], "could not return to main")?;
        } else {
            s.send("/btw aside-sentinel\r")?;
            s.wait_for(&[Lit("ASIDE_ANSWER")], "no side answer during the command")?;
        }
        if dest.join("main-finished.txt").exists() {
            bail!("side question waited for the main command to finish");
        }
    }
    s.wait_for(&[Lit("CAPTURE_COMPLETE")], "capture timed out")?;
    s.finish()?;
    if prompt == "summary-capture" {
        let trace = std::fs::read_to_string(dest.join(format!("trajectory-{cols}.json")))?;
        if trace.matches("PTY_SIDE_SUMMARY").count() != 1 {
            bail!("main did not receive the side summary exactly once");
        }
    }
    if prompt == "owned-capture" && std::fs::read_to_string(dest.join("owned-result.txt"))? != "ready" {
        bail!("owned command result missing");
    }
    println!("PASS: captured real PTY at {cols} columns");
    Ok(())
}

/// A blank answer declines an approval. Then, in the interactive REPL, an
/// approved write lets the turn resume, and the terminal can still be resized
/// and used afterwards: a second line editor at the prompt used to replace the
/// REPL's resize handler, so the next resize aborted rusty.
pub fn approval(binary: &Path, endpoint: &str, dest: &Path) -> Result<()> {
    let approved = dest.join("approved.txt");
    let mut cmd = Command::new(binary);
    cmd.current_dir(dest);
    cmd.args(["--permissions", "ask", "--mode", "standard", "--agents", "off", "--memory", "off", "approval"]);
    offline(&mut cmd, endpoint, &dest.join("home"));
    let log = File::create(dest.join("terminal-declined.ansi"))?;
    let mut s = Session::spawn(cmd, 80, Box::new(log), Duration::from_secs(15))?;
    s.wait_for(&[Lit("[y]es")], "approval prompt missing")?;
    s.send("\r")?;
    s.wait_for(&[Lit("CAPTURE_COMPLETE")], "a declined approval did not resume the turn")?;
    s.finish()?;
    if approved.exists() {
        bail!("a blank answer approved the write");
    }
    println!("PASS: a blank answer declines an approval");

    let mut cmd = Command::new(binary);
    cmd.current_dir(dest);
    cmd.args(["--permissions", "ask", "--mode", "standard", "--agents", "off", "--memory", "off"]);
    offline(&mut cmd, endpoint, &dest.join("home"));
    let log = File::create(dest.join("terminal.ansi"))?;
    let mut s = Session::spawn(cmd, 80, Box::new(log), Duration::from_secs(15))?;
    let ready = |s: &mut Session| -> Result<()> {
        s.wait_for(&[Lit("?2004h")], "no prompt")?;
        s.drain(0.4)
    };
    ready(&mut s)?;
    s.send("approval\r")?;
    s.wait_for(&[Lit("[y]es")], "approval prompt missing")?;
    s.send("yes\r")?;
    s.wait_for(&[Lit("CAPTURE_COMPLETE")], "approval did not resume turn")?;
    ready(&mut s)?;
    s.pty.set_size(ROWS, 100)?;
    s.drain(0.3)?;
    s.pty.set_size(ROWS, 80)?;
    s.drain(0.3)?;
    s.send("reply with just the word pong\r")?;
    s.wait_for(&[Lit("pong"), Space], "the turn after a resize did not run")?;
    ready(&mut s)?;
    s.send("/exit\r")?;
    s.finish().context("rusty did not exit cleanly after a resize")?;
    let written = std::fs::read_to_string(&approved).unwrap_or_default();
    if written != "approved" {
        bail!("the approved write did not happen: approved.txt holds {written:?}");
    }
    println!("PASS: panel paused for approval, turn resumed, and the terminal survived a resize");
    Ok(())
}

pub enum Interrupt {
    Stopped(u128),
    /// The model finished before the keypress.
    Inconclusive,
}

/// In the interactive REPL: Ctrl-C during a streaming turn stops it within
/// two seconds, the next turn still works, and Ctrl-C twice at an empty
/// prompt quits. With `endpoint` the stand-in provider answers, otherwise
/// whatever model the environment configures.
pub fn ctrl_c(binary: &Path, cwd: &Path, endpoint: Option<(&str, &Path)>, log: Box<dyn Write>) -> Result<Interrupt> {
    let mut cmd = Command::new(binary);
    cmd.current_dir(cwd).arg("--yolo");
    if let Some((endpoint, home)) = endpoint {
        offline(&mut cmd, endpoint, home);
    }
    let mut s = Session::spawn(cmd, 80, log, Duration::from_secs(90))?;
    let ready = |s: &mut Session| -> Result<()> {
        s.wait_for(&[Lit("?2004h")], "no prompt")?;
        s.drain(0.4)
    };
    ready(&mut s)?;
    s.send("Write a 1500 word essay on the history of Unix, in plain prose. Do not use any tools.\r")?;
    // Wait until the steady panel is visible before sending the interrupt.
    s.wait_for(&[Lit("ctrl-c to stop")], "the panel never showed")?;
    s.drain(2.0)?;
    let sent = Instant::now();
    s.send("\x03")?;
    let ms = match s.expect(&[&[Lit("stopped")], &[Lit("✓ ")]], s.timeout)? {
        Some(0) => sent.elapsed().as_millis(),
        Some(_) => return Ok(Interrupt::Inconclusive),
        None => bail!("ctrl-c did not stop the turn"),
    };
    if ms > 2000 {
        bail!("took {ms}ms to stop");
    }
    ready(&mut s)?;
    s.send("reply with just the word pong\r")?;
    s.wait_for(&[Lit("pong"), Space], "turn after interrupt did not work")?;
    ready(&mut s)?;
    s.send("\x03")?;
    s.wait_for(&[Lit("again to quit")], "a single ctrl-c at the prompt did not offer to quit")?;
    // A key sent before the prompt is back reaches a cooked terminal, not the
    // line editor.
    ready(&mut s)?;
    s.send("\x03")?;
    s.finish().context("a second ctrl-c did not quit")?;
    Ok(Interrupt::Stopped(ms))
}

/// The line the expect script printed for an outcome.
pub fn verdict(result: &Result<Interrupt>) -> String {
    match result {
        Ok(Interrupt::Stopped(ms)) => format!("PASS: stopped in {ms}ms, recovered, quit on double ctrl-c"),
        Ok(Interrupt::Inconclusive) => "INCONCLUSIVE: the turn finished before ctrl-c".into(),
        Err(e) => format!("FAIL: {e:#}"),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Check {
    CtrlC,
    Approval,
    Steady,
    Owned,
    Aside,
    Side,
    Summary,
}

/// Runs the checks into `out`, keeping every capture there. False when the
/// ctrl-c check was inconclusive.
fn run(binary: &Path, out: &Path, checks: &[Check], live: bool) -> Result<bool> {
    let binary = std::fs::canonicalize(binary).with_context(|| format!("no rusty at {}", binary.display()))?;
    std::fs::create_dir_all(out)?;
    let out = std::fs::canonicalize(out)?;
    let provider = Provider::start()?;
    let fail = |what: String| move |e: anyhow::Error| e.context(format!("FAIL: {what}"));
    if checks.contains(&Check::Steady)
        || checks.contains(&Check::Owned)
        || checks.contains(&Check::Aside)
        || checks.contains(&Check::Side)
        || checks.contains(&Check::Summary)
    {
        for cols in WIDTHS {
            let dest = out.join(format!("width-{cols}"));
            std::fs::create_dir_all(&dest)?;
            let prompt = if checks.contains(&Check::Summary) {
                "summary-capture"
            } else if checks.contains(&Check::Side) {
                "side-capture"
            } else if checks.contains(&Check::Aside) {
                "aside-capture"
            } else if checks.contains(&Check::Owned) {
                "owned-capture"
            } else {
                "capture"
            };
            tools_capture(&binary, &provider.endpoint, cols, &dest, prompt).map_err(fail(format!("{cols} columns")))?;
        }
    }
    if checks.contains(&Check::Approval) {
        let dest = out.join("approval");
        std::fs::create_dir_all(&dest)?;
        approval(&binary, &provider.endpoint, &dest).map_err(fail("approval".into()))?;
    }
    if checks.contains(&Check::CtrlC) {
        let home = out.join("interrupt-home");
        let endpoint = (!live).then_some((provider.endpoint.as_str(), home.as_path()));
        let log = File::create(out.join("ctrl-c.txt"))?;
        let mut copy = log.try_clone()?;
        let result = ctrl_c(&binary, &out, endpoint, Box::new(log));
        let line = verdict(&result);
        writeln!(copy, "\n{line}")?;
        println!("{line}");
        if let Interrupt::Inconclusive = result? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn flag(args: &[String], name: &str) -> Option<PathBuf> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(PathBuf::from)
}

/// `cargo xtask tty [ctrl-c|approval|steady]... [--binary PATH] [--live]`
pub fn command(args: &[String]) -> Result<ExitCode> {
    let mut checks = Vec::new();
    let mut skip = false;
    for a in args {
        match a.as_str() {
            _ if skip => skip = false,
            "--binary" => skip = true,
            "--live" => {}
            "ctrl-c" => checks.push(Check::CtrlC),
            "approval" => checks.push(Check::Approval),
            "steady" => checks.push(Check::Steady),
            "owned" => checks.push(Check::Owned),
            "aside" => checks.push(Check::Aside),
            "side" => checks.push(Check::Side),
            "summary" => checks.push(Check::Summary),
            other => bail!("unknown tty check {other:?}; expected ctrl-c, approval or steady"),
        }
    }
    if checks.is_empty() {
        checks = vec![Check::Steady, Check::Approval, Check::CtrlC];
    }
    let binary = match flag(args, "--binary") {
        Some(b) => b,
        None => crate::debug_rusty()?,
    };
    let work = std::env::temp_dir().join(format!("rusty-tty-{}", std::process::id()));
    match run(&binary, &work, &checks, args.iter().any(|a| a == "--live")) {
        Ok(conclusive) => {
            let _ = std::fs::remove_dir_all(&work);
            Ok(if conclusive { ExitCode::SUCCESS } else { ExitCode::from(2) })
        }
        Err(e) => {
            eprintln!("captures kept in {}", work.display());
            Err(e)
        }
    }
}

/// `cargo xtask tty-capture --binary PATH --output DIR`
pub fn capture(args: &[String]) -> Result<ExitCode> {
    let (Some(binary), Some(output)) = (flag(args, "--binary"), flag(args, "--output")) else {
        bail!("usage: cargo xtask tty-capture --binary PATH --output DIR");
    };
    let width = if args.iter().any(|a| a == "--summary") {
        Check::Summary
    } else if args.iter().any(|a| a == "--side") {
        Check::Side
    } else if args.iter().any(|a| a == "--aside") {
        Check::Aside
    } else if args.iter().any(|a| a == "--owned") {
        Check::Owned
    } else {
        Check::Steady
    };
    if !run(&binary, &output, &[width, Check::Approval, Check::CtrlC], false)? {
        bail!("the ctrl-c capture was inconclusive");
    }
    Ok(ExitCode::SUCCESS)
}

/// What one width's capture showed.
struct Inspection {
    cols: usize,
    frames: Vec<Vec<String>>,
    scrolls: usize,
    last: Vec<String>,
}

/// Replays a capture on a screen model and checks every panel frame: complete
/// borders, nothing in the wrap column, coloured, pinned to the bottom row
/// once it gets there, and gone at exit.
fn inspect_capture(path: &Path, cols: usize) -> Result<Inspection> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8(raw).with_context(|| format!("{} is not UTF-8", path.display()))?;
    let rows = ROWS as usize;
    let mut screen = Screen::new(rows, cols);
    let mut frames = Vec::new();
    let mut anchors = Vec::new();
    let mut problem: Option<String> = None;
    let coloured = |pen: Pen| pen.fg != Color::Default || pen.bold || pen.dim;
    screen.feed(&text, |s, event| {
        if problem.is_some() {
            return;
        }
        let r = s.row();
        match event {
            // The compact one-line footer of a narrow terminal.
            Event::Reset if cols < 31 && s.line(r).starts_with("◇ rusty") => {
                if s.cell(r, cols - 1).ch != ' ' {
                    problem = Some("compact footer used wrap column".into());
                } else if !coloured(s.cell(r, 0).pen) {
                    problem = Some("compact footer drawn without colour".into());
                }
                frames.push(s.lines());
                anchors.push(r);
            }
            // The bottom-right corner completes a panel.
            Event::Printed('╯') => {
                let complete = r >= 2 && s.cell(r - 2, 0).ch == '╭' && s.cell(r - 1, 0).ch == '│';
                if !complete {
                    problem = Some(format!("incomplete panel at row {r}:\n{}", s.lines().join("\n")));
                } else if !(r - 2..=r).all(|j| "╮│╯".contains(s.cell(j, cols - 2).ch)) {
                    problem = Some(format!("panel's right edge is misaligned:\n{}", s.lines()[r - 2..=r].join("\n")));
                } else if !(r - 2..=r).all(|j| s.cell(j, cols - 1).ch == ' ') {
                    problem = Some("panel used wrap column".into());
                } else if !coloured(s.cell(r, 0).pen) {
                    problem = Some("panel drawn without colour".into());
                }
                anchors.push(r);
                frames.push(s.lines());
            }
            _ => {}
        }
    });
    if let Some(p) = problem {
        bail!("{cols} columns: {p}");
    }
    let last = screen.lines();
    if last.iter().any(|l| l.contains('╭') || l.contains('╯')) {
        bail!("{cols} columns: transient panel left at exit");
    }
    if !text.contains("CAPTURE_COMPLETE") {
        bail!("{cols} columns: the capture never completed");
    }
    if !frames.is_empty() {
        if screen.scrolled == 0 {
            bail!("{cols} columns: output never scrolled");
        }
        let Some(first) = anchors.iter().position(|&a| a == rows - 1) else {
            bail!("{cols} columns: the panel never reached the bottom row");
        };
        if anchors[first..].iter().any(|&a| a != rows - 1) {
            bail!("{cols} columns: panel shifted after reaching bottom");
        }
    }
    Ok(Inspection { cols, frames, scrolls: screen.scrolled, last })
}

fn lines_json(lines: &[String]) -> String {
    format!("[{}]", lines.iter().map(|l| quote(l)).collect::<Vec<_>>().join(", "))
}

/// `cargo xtask tty-inspect DIR`: checks each width's capture and writes
/// `alignment.json`, `replay.json` (every other frame) and each width's
/// `last-screen.txt`.
pub fn inspect(args: &[String]) -> Result<ExitCode> {
    let Some(root) = args.first().map(PathBuf::from) else { bail!("usage: cargo xtask tty-inspect DIR") };
    let mut reports = Vec::new();
    let mut replays = Vec::new();
    for cols in WIDTHS {
        let dir = root.join(format!("width-{cols}"));
        let i = inspect_capture(&dir.join(format!("terminal-{cols}.ansi")), cols as usize)?;
        reports.push(format!(
            "{{\"width\": {}, \"passed\": true, \"complete_frames\": {}, \"scrolls\": {}, \
             \"footer_stays_at_bottom\": {}, \"clean_exit\": true}}",
            i.cols,
            i.frames.len(),
            i.scrolls,
            !i.frames.is_empty()
        ));
        let shown: Vec<String> = if i.frames.is_empty() {
            vec![lines_json(&i.last)]
        } else {
            i.frames.iter().step_by(2).map(|f| lines_json(f)).collect()
        };
        replays.push(format!("\"{cols}\": [{}]", shown.join(", ")));
        std::fs::write(dir.join("last-screen.txt"), i.last.join("\n") + "\n")?;
    }
    let checks = reports.iter().map(|r| format!("    {r}")).collect::<Vec<_>>().join(",\n");
    std::fs::write(
        root.join("alignment.json"),
        format!("{{\n  \"passed\": true,\n  \"checks\": [\n{checks}\n  ]\n}}\n"),
    )?;
    std::fs::write(root.join("replay.json"), format!("{{{}}}", replays.join(", ")))?;
    println!("[{}]", reports.join(", "));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thirty lines of output, then a three-row panel `inner` cells wide,
    /// optionally coloured and cleared, then the completion marker.
    fn capture(inner: usize, colour: bool, clear: bool) -> String {
        let mut s: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
        let (on, off) = if colour { ("\x1b[38;5;102m", "\x1b[0m") } else { ("", "") };
        let rule = "─".repeat(inner);
        s += &format!("{on}╭{rule}╮{off}\r\n{on}│{}│{off}\r\n{on}╰{rule}╯{off}", " ".repeat(inner));
        if clear {
            s += "\r\x1b[2A\x1b[2K\x1b[1B\r\x1b[2K\x1b[1B\r\x1b[2K\x1b[2A";
        }
        s + "CAPTURE_COMPLETE\r\n"
    }

    fn check(name: &str, text: &str) -> Result<Inspection> {
        let path = std::env::temp_dir().join(format!("xtask-inspect-{}-{name}", std::process::id()));
        std::fs::write(&path, text)?;
        let result = inspect_capture(&path, 40);
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn inspection_accepts_a_clean_panel() {
        let i = check("clean", &capture(37, true, true)).unwrap();
        assert_eq!((i.frames.len(), i.scrolls), (1, 9));
        assert!(i.last.iter().any(|l| l.starts_with("CAPTURE_COMPLETE")));
    }

    #[test]
    fn inspection_rejects_broken_panels() {
        let err = |name, text: String| check(name, &text).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err("wide", capture(38, true, true)).contains("misaligned"));
        assert!(err("plain", capture(37, false, true)).contains("without colour"));
        assert!(err("left", capture(37, true, false)).contains("left at exit"));
    }
}
