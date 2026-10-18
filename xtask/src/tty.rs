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

/// Check the actual cells after soft wraps and an explicit Ctrl-J newline.
fn inspect_input(text: &str, cols: u16, plain: bool, indexed: bool) -> Result<()> {
    let mut screen = Screen::new(ROWS as usize, cols as usize);
    screen.feed(text, |_, _| {});
    let lines = screen.lines();
    let first = lines.iter().position(|l| l.contains("USER_PROMPT_START")).context("input start missing")?;
    let last = lines.iter().rposition(|l| l.contains("USER_PROMPT_END")).context("input end missing")?;
    if last <= first {
        bail!("input did not wrap");
    }
    let wanted = if plain {
        Color::Default
    } else if indexed {
        Color::Indexed(111)
    } else {
        Color::Rgb(138, 190, 255)
    };
    for row in first..=last {
        for col in 0..cols as usize {
            let cell = screen.cell(row, col);
            if !cell.ch.is_whitespace()
                && cell.ch != '\0'
                && (cell.pen.fg != wanted || cell.pen.bold == plain || cell.pen.dim)
            {
                bail!("input colour changed at {row}:{col}: {:?}", cell.pen);
            }
        }
    }
    Ok(())
}

fn input_capture(binary: &Path, endpoint: &str, cols: u16, dest: &Path, plain: bool, indexed: bool) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.current_dir(dest).args(["--mode", "standard", "--agents", "off", "--memory", "off"]);
    offline(&mut cmd, endpoint, &dest.join("home"));
    cmd.env("COLORTERM", if indexed { "" } else { "truecolor" }).env("RUSTY_CONTEXT_TOKENS", "128000");
    if plain {
        cmd.env("NO_COLOR", "1");
    }
    let log = File::create(dest.join("input.ansi"))?;
    let mut s = Session::spawn(cmd, cols, Box::new(log), Duration::from_secs(20))?;
    s.wait_for(&[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")], "input prompt missing")?;
    s.drain(0.2)?;
    let wrapped = format!("USER_PROMPT_START {}pong", "wrapped input ".repeat(16));
    s.send(&wrapped)?;
    s.send("\x0aUSER_PROMPT_END")?;
    s.wait_for(&[Lit("USER_PROMPT_END")], "typed input did not finish redrawing")?;
    s.drain(0.1)?;
    inspect_input(&s.text, cols, plain, indexed)?;
    std::fs::write(dest.join("input-live.ansi"), &s.text)?;
    s.send("\r")?;
    s.wait_for(&[Lit("pong"), Space], "submitted input did not run")?;
    s.wait_for(&[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")], "next prompt missing")?;
    s.drain(0.2)?;
    inspect_input(&s.text, cols, plain, indexed)?;
    s.send("/quit\r")?;
    s.finish()?;
    println!("PASS: input keeps its colour across wraps, Ctrl-J and submission at {cols} columns (NO_COLOR={plain}, indexed={indexed})");
    Ok(())
}

/// Keep selection and the exact pending input through repeated real SIGWINCHs.
fn menu_resize_capture(binary: &Path, endpoint: &str, dest: &Path) -> Result<()> {
    let home = dest.join("home");
    let mut cmd = Command::new(binary);
    cmd.current_dir(dest).args(["--mode", "standard", "--agents", "off", "--memory", "off"]);
    offline(&mut cmd, endpoint, &home);
    cmd.env("COLORTERM", "truecolor").env("RUSTY_CONTEXT_TOKENS", "128000");
    let log = File::create(dest.join("menu-resize.ansi"))?;
    let mut s = Session::spawn(cmd, 80, Box::new(log), Duration::from_secs(20))?;
    let ready = |s: &mut Session| -> Result<()> {
        s.wait_for(
            &[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")],
            "input prompt missing after resize",
        )?;
        s.drain(0.3)
    };
    ready(&mut s)?;
    s.send("/theme ")?;
    s.drain(0.3)?;
    for cols in [24, 120, 40, 80] {
        s.pty.set_size(ROWS, cols)?;
        s.drain(0.3)?;
    }
    // Value menus begin with no selection. Seven Down keys select Paper.
    s.send(&"\x1b[B".repeat(7))?;
    s.send("\r")?;
    ready(&mut s)?;
    let settings: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("settings.json"))?)?;
    if settings["theme"] != "paper" {
        bail!("resized menu did not apply the selected Paper skin: {settings}");
    }
    s.send("/theme rust\r")?;
    ready(&mut s)?;
    let pending = format!("resize sentinel 文件 {}pong", "keep this input ".repeat(10));
    s.send(&pending)?;
    s.drain(0.3)?;
    for cols in [40, 24, 120, 80] {
        s.pty.set_size(ROWS, cols)?;
        s.drain(0.3)?;
    }
    s.send("\r")?;
    s.wait_for(&[Lit("pong"), Space], "resized input did not run")?;
    s.wait_for(&[Lit("response OK")], "completed response did not update connection status")?;
    ready(&mut s)?;
    s.send("/quit\r")?;
    s.finish()?;
    if !std::fs::read_to_string(home.join("history"))?.contains(&pending) {
        bail!("resize changed the submitted input");
    }
    println!("PASS: menu selection and exact Unicode input survive 24/40/80/120-column contraction and expansion");
    Ok(())
}

/// These controls must work even when no model server is listening.
fn shell_capture(binary: &Path, dest: &Path) -> Result<()> {
    let home = dest.join("home");
    let codex = dest.join("codex");
    std::fs::create_dir_all(&codex)?;
    std::fs::write(codex.join("config.toml"), "# preserved\nmodel = \"old\"\n[profiles.work]\nmodel = \"keep\"\n")?;
    std::fs::write(
        codex.join("models_cache.json"),
        r#"{"models":[{"slug":"gpt-test","visibility":"list","priority":0}]}"#,
    )?;
    let mut cmd = Command::new(binary);
    cmd.current_dir(dest).args(["--mode", "standard", "--agents", "off", "--memory", "off", "--permissions", "yolo"]);
    cmd.args(["--max-requests", "1", "--budget-secs", "30"]);
    offline(&mut cmd, "http://127.0.0.1:1/v1", &home);
    cmd.env("RUSTY_PROVIDER", "compatible").env("RUSTY_CONTEXT_TOKENS", "128000").env("CODEX_HOME", &codex);
    std::fs::write(dest.join("editor.sh"), "printf '\\n# editor visited\\n' >> \"$1\"\n")?;
    cmd.env("VISUAL", "sh ./editor.sh");
    let log = File::create(dest.join("direct-shell.ansi"))?;
    let mut s = Session::spawn(cmd, 80, Box::new(log), Duration::from_secs(10))?;
    let ready = |s: &mut Session| -> Result<()> {
        s.wait_for(
            &[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")],
            "input did not return after direct shell",
        )?;
        s.drain(0.15)
    };
    ready(&mut s)?;
    s.send("!pwd\r")?;
    s.wait_for(&[Lit("stdout:")], "!pwd produced no stdout")?;
    ready(&mut s)?;
    if !s.text.contains(dest.to_str().context("non-UTF8 cwd")?) {
        bail!("!pwd did not use session cwd");
    }
    s.send("!printf 'direct-%s' shell; exit 7\r")?;
    s.wait_for(&[Lit("exit code: 7")], "shell exit code missing")?;
    s.wait_for(&[Lit("direct-shell")], "shell output missing")?;
    ready(&mut s)?;
    s.send("!sleep 20\r")?;
    s.drain(0.5)?;
    s.send("\x03")?;
    s.wait_for(&[Lit("interrupted by the user")], "shell was not interrupted")?;
    ready(&mut s)?;
    // An unfinished draft stays editable after work finishes and is not sent
    // to the unavailable provider. Unicode backspace edits the draft itself.
    s.send("!sleep 2\r")?;
    s.drain(0.3)?;
    s.send("draft 界\x7f!")?;
    s.drain(0.3)?;
    ready(&mut s)?;
    if !s.text.contains("draft !") || s.text.contains("failed last request") {
        bail!("unfinished Unicode draft was lost or submitted without Enter");
    }
    s.send("\x03")?;
    ready(&mut s)?;
    s.send("/permissions read-only\r")?;
    ready(&mut s)?;
    s.send("!touch forbidden\r")?;
    s.wait_for(&[Lit("denied by permissions")], "direct shell bypassed read-only")?;
    ready(&mut s)?;
    if dest.join("forbidden").exists() {
        bail!("denied command changed the workspace");
    }
    s.send("/codex model ")?;
    s.drain(0.2)?;
    s.send("\x1b[B\r")?;
    ready(&mut s)?;
    s.send("/codex config\r")?;
    ready(&mut s)?;
    let text = std::fs::read_to_string(codex.join("config.toml"))?;
    if !text.contains("# editor visited") {
        bail!("config editor did not receive the Codex path");
    }
    if !text.contains("model = \"gpt-test\"") || !text.contains("model = \"keep\"") {
        bail!("Codex config edit lost settings");
    }
    s.send("hello\r")?;
    s.wait_for(&[Lit("failed last request")], "failed request did not update connection status")?;
    ready(&mut s)?;
    s.send("/quit\r")?;
    s.finish()?;
    println!("PASS: direct shell cwd, output, exit status, interrupt, permissions and Codex config work without a model server");
    Ok(())
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
        // Startup includes durable session creation. Wait for the actual tool
        // witness within the same bounded deadline as other PTY readiness.
        let end = Instant::now() + s.timeout;
        while !dest.join("main-started.txt").exists() && Instant::now() < end {
            if !s.pump(Duration::from_millis(20))? {
                bail!("terminal closed before the main command started");
            }
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
                s.send("side-slow\rside-followup\r")?;
                s.wait_for(&[Lit("SIDE_SLOW_ANSWER")], "slow side response missing")?;
                s.wait_for(&[Lit("SIDE_FOLLOWUP_ANSWER")], "busy side dropped its queued follow-up")?;
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
/// `/ping`: off by default; once on, a finished turn sends exactly one OSC 9
/// frame made of fixed words and the folder name, and the setting is saved;
/// a turn the user stopped with ctrl-c sends nothing. `RUSTY_PING_AFTER=0`
/// removes the minimum turn length so the stand-in's quick turns count.
/// This proves the bytes, not that a terminal showed a notification.
fn ping(binary: &Path, endpoint: &str, dest: &Path) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.current_dir(dest).args(["--mode", "standard", "--agents", "off", "--memory", "off"]);
    offline(&mut cmd, endpoint, &dest.join("home"));
    cmd.env("RUSTY_PING_AFTER", "0");
    let log = File::create(dest.join("ping.ansi"))?;
    let mut s = Session::spawn(cmd, 80, Box::new(log), Duration::from_secs(30))?;
    let prompt = &[Lit("?2004h")];
    let frames = |text: &str| text.matches("\x1b]9;").count();
    s.wait_for(prompt, "no prompt")?;
    s.drain(0.3)?;
    s.send("reply with just the word pong\r")?;
    s.wait_for(&[Lit("pong"), Space], "the first turn did not run")?;
    s.wait_for(prompt, "no prompt after the first turn")?;
    s.drain(0.4)?;
    if frames(&s.text) != 0 {
        bail!("a ping went out while pings were off");
    }
    s.send("/ping on\r")?;
    s.wait_for(prompt, "no prompt after /ping on")?;
    s.drain(0.3)?;
    let saved = std::fs::read_to_string(dest.join("home/settings.json")).unwrap_or_default();
    if !saved.contains("\"ping\":\"on\"") {
        bail!("/ping on was not saved: {saved}");
    }
    s.send("reply with just the word pong\r")?;
    s.wait_for(&[Lit("pong"), Space], "the second turn did not run")?;
    s.wait_for(prompt, "no prompt after the second turn")?;
    s.drain(0.4)?;
    let want = "\x1b]9;rusty · ping: response ready\x07";
    if frames(&s.text) != 1 || !s.text.contains(want) {
        bail!("expected one {want:?} frame, saw {} frames", frames(&s.text));
    }
    // More than the spacing between pings, so only the stop can suppress it.
    s.drain(3.5)?;
    s.send("Write a 1500 word essay on the history of Unix, in plain prose. Do not use any tools.\r")?;
    s.wait_for(&[Lit("ctrl-c to stop")], "the slow turn never showed its panel")?;
    s.drain(1.0)?;
    s.send("\x03")?;
    match s.expect(&[&[Lit("stopped")], &[Lit("✓ ")]], s.timeout)? {
        Some(0) => {}
        Some(_) => bail!("the slow turn finished before ctrl-c; inconclusive"),
        None => bail!("ctrl-c did not stop the turn"),
    }
    s.wait_for(prompt, "no prompt after the stop")?;
    s.drain(0.4)?;
    if frames(&s.text) != 1 {
        bail!("a turn stopped with ctrl-c sent a ping");
    }
    s.send("/quit\r")?;
    s.finish()?;
    println!(
        "PASS: /ping is off by default, saves on, sends one fixed OSC 9 frame per finished turn and none after ctrl-c"
    );
    Ok(())
}

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
        s.wait_for(&[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")], "no prompt")?;
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

/// Exercise the visible busy draft and typeahead reader, not just rustyline.
fn typeahead_capture(binary: &Path, endpoint: &str, cols: u16, dest: &Path, plain: bool) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.current_dir(dest).args(["--mode", "standard", "--agents", "off", "--memory", "off"]);
    offline(&mut cmd, endpoint, &dest.join("home"));
    cmd.env("RUSTY_PROVIDER", "compatible");
    if plain {
        cmd.env("NO_COLOR", "1");
    }
    let log = File::create(dest.join("typeahead.ansi"))?;
    let mut s = Session::spawn(cmd, cols, Box::new(log), Duration::from_secs(30))?;
    s.wait_for(&[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")], "input prompt missing")?;
    s.drain(0.2)?;
    s.send("Unix\r")?;
    s.wait_for(&[Lit("A long streamed reply")], "stream did not start")?;
    // The emoji begins at byte 511, crossing read_keys' 512-byte buffer.
    let input = crate::provider::typeahead_input();
    s.send("\x1b[200~")?;
    s.drain(0.1)?;
    s.pty.send(&input.as_bytes()[..512])?;
    s.drain(0.15)?;
    s.pty.send(&input.as_bytes()[512..])?;
    s.send("\x1b[201~\r")?;
    match s.expect(&[&[Lit("TYPEAHEAD_EXACT_MATCH")], &[Lit("TYPEAHEAD_MISMATCH")]], s.timeout)? {
        Some(0) => {}
        Some(_) => bail!("provider did not receive exact multiline Unicode typeahead"),
        None => bail!("queued Unicode input did not reach the provider"),
    }
    s.wait_for(
        &[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")],
        "prompt did not recover after typeahead",
    )?;
    s.drain(0.3)?;
    // A pasted absolute path plus unfinished second line stays one editable
    // draft after stopping, and is never routed as a slash command.
    s.send("Unix\r")?;
    s.wait_for(&[Lit("A long streamed reply")], "second stream did not start")?;
    let path = crate::provider::typeahead_path_input();
    s.send(&format!("\x1b[200~{path}\x1b[201~"))?;
    if plain {
        // No terminal echo is enabled while busy: this text must come from
        // the visible draft renderer, even when color styling is disabled.
        s.wait_for(&[Lit("PATH_END")], "NO_COLOR busy draft was invisible")?;
        s.wait_for(&[Lit("A long streamed reply")], "stream did not continue after drafting")?;
        s.drain(0.3)?;
        let mut screen = Screen::new(ROWS as usize, cols as usize);
        screen.feed(&s.text, |_, _| {});
        if !screen.lines().iter().any(|line| line.contains("PATH_END")) {
            bail!("NO_COLOR draft disappeared after streamed output");
        }
    }
    s.drain(0.3)?;
    s.send("\x03")?;
    s.wait_for(&[Lit("stopped")], "turn did not stop with a pasted draft")?;
    s.wait_for(
        &[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")],
        "stopped draft editor did not recover",
    )?;
    let mark = s.text.len();
    s.drain(0.3)?;
    if s.text[mark..].contains("TYPEAHEAD_PATH_") {
        bail!("unfinished pasted draft was sent without Enter");
    }
    s.send(" EDITED\r")?;
    match s.expect(&[&[Lit("TYPEAHEAD_PATH_EXACT_MATCH")], &[Lit("TYPEAHEAD_PATH_MISMATCH")]], s.timeout)? {
        Some(0) => {}
        Some(_) => bail!("provider did not receive exact edited path and multiline draft"),
        None => bail!("pasted path draft was lost, split or routed as a command"),
    }
    s.wait_for(
        &[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")],
        "prompt did not recover after pasted draft",
    )?;
    s.drain(0.3)?;
    s.send("/exit\r")?;
    s.finish()?;
    println!("PASS: exact multiline Unicode typeahead at {cols} columns (NO_COLOR={plain})");
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
        // Memory lifecycle has its own tests; this terminal fixture must not
        // autostart a service that intentionally outlives the CLI process.
        cmd.args(["--memory", "off"]);
        offline(&mut cmd, endpoint, home);
    }
    let mut s = Session::spawn(cmd, 80, log, Duration::from_secs(90))?;
    let ready = |s: &mut Session| -> Result<()> {
        s.wait_for(&[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")], "no prompt")?;
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
    if let Some((endpoint, home)) = endpoint {
        side_ctrl_c(binary, cwd, endpoint, home)?;
    }
    Ok(Interrupt::Stopped(ms))
}

/// Checks focused side cancellation and prompt-level Ctrl-C on a controlling
/// PTY. A side task consumes the first Ctrl-C; the parent must remain usable,
/// and the usual prompt double-Ctrl-C quit must still work afterwards.
fn side_ctrl_c(binary: &Path, cwd: &Path, endpoint: &str, home: &Path) -> Result<()> {
    let command = |name: &str| {
        let mut cmd = Command::new(binary);
        cmd.current_dir(cwd).args(["--mode", "standard", "--agents", "off", "--memory", "off"]);
        offline(&mut cmd, endpoint, &home.join(name));
        cmd
    };
    let prompt = &[Lit("────────────────────"), Line, Space, Line, Lit("?2004h")];

    let mut idle = Session::spawn(command("side-idle-home"), 80, Box::new(std::io::sink()), Duration::from_secs(4))?;
    idle.wait_for(prompt, "idle-side check had no prompt")?;
    idle.send("/side new idle\r")?;
    idle.wait_for(&[Lit("read-only, live workspace")], "idle-side check could not select a side")?;
    idle.wait_for(prompt, "idle side prompt did not return")?;
    idle.send("\x03")?;
    idle.wait_for(
        &[Lit("again to quit")],
        "Ctrl-C with an idle selected side did not reach the normal prompt handler",
    )?;
    idle.wait_for(prompt, "idle side prompt did not recover after the first Ctrl-C")?;
    idle.drain(0.2)?;
    idle.send("\x03")?;
    idle.finish().context("double Ctrl-C with an idle selected side did not quit")?;

    let mut busy = Session::spawn(command("side-busy-home"), 80, Box::new(std::io::sink()), Duration::from_secs(12))?;
    busy.wait_for(prompt, "busy-side check had no prompt")?;
    busy.send("/side new busy\r")?;
    busy.wait_for(&[Lit("read-only, live workspace")], "busy-side check could not select a side")?;
    busy.wait_for(prompt, "busy side prompt did not return")?;
    busy.send("side-cancel\r")?;
    busy.wait_for(&[Lit("side question running")], "busy side task did not start")?;
    busy.send("\x03")?;
    busy.wait_for(&[Lit("side turn interrupted")], "first Ctrl-C did not cancel the selected child")?;
    // The side-result notice is emitted before the REPL starts its next
    // readline call. Wait for raw-mode/prompt readiness before sending /back;
    // otherwise the PTY can deliver the command during the handoff.
    busy.wait_for(prompt, "parent prompt did not return after child cancellation")?;
    busy.send("/back\r")?;
    busy.wait_for(&[Lit("main chat selected")], "parent did not survive child cancellation")?;
    busy.wait_for(prompt, "parent prompt did not return after child cancellation")?;
    busy.send("reply with just the word pong\r")?;
    busy.wait_for(&[Lit("pong")], "parent could not run a turn after cancelling its child")?;
    busy.wait_for(prompt, "parent prompt did not return after its turn")?;
    busy.send("\x03")?;
    busy.wait_for(&[Lit("again to quit")], "first prompt Ctrl-C after child cancellation did not offer to quit")?;
    busy.wait_for(prompt, "parent prompt did not recover after the first prompt Ctrl-C")?;
    busy.drain(0.2)?;
    busy.send("\x03")?;
    busy.finish().context("double Ctrl-C after child cancellation did not quit")?;
    Ok(())
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
    Input,
    Typeahead,
    TypeaheadPlain,
    Menu,
    Shell,
    Aside,
    Side,
    Summary,
    Ping,
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
    if checks.contains(&Check::Input) {
        for cols in WIDTHS {
            let dest = out.join(format!("input-{cols}"));
            std::fs::create_dir_all(&dest)?;
            input_capture(&binary, &provider.endpoint, cols, &dest, false, false)?;
        }
        let dest = out.join("input-plain");
        std::fs::create_dir_all(&dest)?;
        input_capture(&binary, &provider.endpoint, 80, &dest, true, false)?;
        let dest = out.join("input-indexed");
        std::fs::create_dir_all(&dest)?;
        input_capture(&binary, &provider.endpoint, 80, &dest, false, true)?;
    }
    if checks.contains(&Check::Shell) {
        let dest = out.join("direct-shell");
        std::fs::create_dir_all(&dest)?;
        shell_capture(&binary, &dest)?;
    }
    if checks.contains(&Check::Typeahead) {
        for cols in WIDTHS {
            let dest = out.join(format!("typeahead-{cols}"));
            std::fs::create_dir_all(&dest)?;
            typeahead_capture(&binary, &provider.endpoint, cols, &dest, false)?;
        }
    }
    if checks.contains(&Check::Typeahead) || checks.contains(&Check::TypeaheadPlain) {
        let dest = out.join("typeahead-plain");
        std::fs::create_dir_all(&dest)?;
        typeahead_capture(&binary, &provider.endpoint, 80, &dest, true)?;
    }
    if checks.contains(&Check::Input) {
        for cols in WIDTHS {
            let dest = out.join(format!("input-{cols}"));
            std::fs::create_dir_all(&dest)?;
            input_capture(&binary, &provider.endpoint, cols, &dest, false, false)?;
        }
        let dest = out.join("input-plain");
        std::fs::create_dir_all(&dest)?;
        input_capture(&binary, &provider.endpoint, 80, &dest, true, false)?;
        let dest = out.join("input-indexed");
        std::fs::create_dir_all(&dest)?;
        input_capture(&binary, &provider.endpoint, 80, &dest, false, true)?;
    }
    if checks.contains(&Check::Shell) {
        let dest = out.join("direct-shell");
        std::fs::create_dir_all(&dest)?;
        shell_capture(&binary, &dest)?;
    }
    if checks.contains(&Check::Menu) {
        let dest = out.join("menu-resize");
        std::fs::create_dir_all(&dest)?;
        menu_resize_capture(&binary, &provider.endpoint, &dest)?;
    }
    if checks.contains(&Check::Approval) {
        let dest = out.join("approval");
        std::fs::create_dir_all(&dest)?;
        approval(&binary, &provider.endpoint, &dest).map_err(fail("approval".into()))?;
    }
    if checks.contains(&Check::Ping) {
        let dest = out.join("ping");
        std::fs::create_dir_all(&dest)?;
        ping(&binary, &provider.endpoint, &dest).map_err(fail("ping".into()))?;
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
            "input" => checks.push(Check::Input),
            "typeahead" => checks.push(Check::Typeahead),
            "typeahead-plain" => checks.push(Check::TypeaheadPlain),
            "menu" => checks.push(Check::Menu),
            "shell" => checks.push(Check::Shell),
            "aside" => checks.push(Check::Aside),
            "side" => checks.push(Check::Side),
            "summary" => checks.push(Check::Summary),
            "ping" => checks.push(Check::Ping),
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
    if !run(
        &binary,
        &output,
        &[width, Check::Input, Check::Menu, Check::Typeahead, Check::Shell, Check::Approval, Check::CtrlC],
        false,
    )? {
        bail!("the ctrl-c capture was inconclusive");
    }
    if args.iter().any(|a| a == "--all") {
        for (name, check) in
            [("owned", Check::Owned), ("aside", Check::Aside), ("side", Check::Side), ("summary", Check::Summary)]
        {
            run(&binary, &output.join(name), &[check], false)?;
        }
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
            Event::Reset
                if s.line(r).starts_with("◇ ")
                    && (s.line(r).trim_end().ends_with("ctrl-c to stop") || s.line(r).trim_end().ends_with('s')) =>
            {
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
    if last.iter().any(|l| l.contains('╭') || l.contains('╯') || l.starts_with("◇ ")) {
        bail!("{cols} columns: transient panel left at exit");
    }
    if !text.contains("CAPTURE_COMPLETE") {
        bail!("{cols} columns: the capture never completed");
    }
    if frames.is_empty() {
        bail!("{cols} columns: no progress frames were captured");
    }
    {
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
    if args.iter().any(|a| a == "--all") {
        for name in ["owned", "aside", "side", "summary"] {
            inspect(&[root.join(name).display().to_string()])?;
        }
    }
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
    fn inspection_checks_compact_footer_and_cleanup() {
        let prefix: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
        let frame = "\x1b[38;5;252m◇ Thinking · 2s · ctrl-c to stop\x1b[0m";
        let clean = format!("{prefix}{frame}\r\x1b[2KCAPTURE_COMPLETE\r\n");
        assert_eq!(check("compact-clean", &clean).unwrap().frames.len(), 1);
        let dirty = format!("{prefix}{frame}\r\nCAPTURE_COMPLETE\r\n");
        assert!(check("compact-dirty", &dirty).err().unwrap().to_string().contains("left at exit"));
    }

    #[test]
    fn inspection_rejects_broken_panels() {
        let err = |name, text: String| check(name, &text).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err("wide", capture(38, true, true)).contains("misaligned"));
        assert!(err("plain", capture(37, false, true)).contains("without colour"));
        assert!(err("left", capture(37, true, false)).contains("left at exit"));
    }
}
