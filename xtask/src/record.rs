//! Records a real rusty session as an asciinema v2 cast.
//!
//!     cargo xtask record <scene> <out.cast> [-- rusty args]
//!
//! A scene is a list of steps run against rusty on a pseudo-terminal: wait
//! for output, type like a person and press Enter, or pause so the viewer can
//! read. `RUSTY_BIN` picks the binary (default: `rusty` on PATH). Render with
//! agg: `agg --theme monokai out.cast out.gif`.

use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};

use crate::json::quote;
use crate::pattern::{find, strip_ansi, Piece, Piece::*};
use crate::pty::{Chunk, Pty, Utf8};

const COLS: u16 = 104;
const ROWS: u16 = 34;

enum Step {
    /// Wait until the output since the last Enter (or match) matches.
    Wait(&'static [Piece]),
    /// Type like a person, then press Enter.
    Type(&'static str),
    /// Let the viewer read.
    Pause(f64),
}
use Step::*;

const PROMPT: Step = Wait(&[Lit("?2004h")]);
const TURN_DONE: Step = Wait(&[Lit("✓ "), Line, Lit("ctx")]);

const SCENES: &[(&str, &[Step])] = &[
    (
        "mode-auto",
        &[
            PROMPT,
            Type("/mode auto"),
            PROMPT,
            Pause(1.0),
            Type("the discount tests are failing. find out why, fix it, and prove it. keep the explanation short."),
            TURN_DONE,
            PROMPT,
            Pause(2.0),
            Type("quick prototype: add a pure format_cents function in shop/formatting.py and one test. keep it tiny; run the test."),
            TURN_DONE,
            PROMPT,
            Pause(2.0),
            Type("review this local shop for production deployment risks. read the code, make no changes, and give three concrete risks. do not deploy anything."),
            TURN_DONE,
            PROMPT,
            Pause(4.0),
        ],
    ),
    (
        "mode-careful",
        &[
            PROMPT,
            Type("/mode careful"),
            PROMPT,
            Pause(1.0),
            Type("the discount tests are failing. find out why, fix it, and prove it. keep the explanation short."),
            TURN_DONE,
            PROMPT,
            Pause(4.0),
        ],
    ),
    (
        "mode-standard",
        &[
            PROMPT,
            Type("/mode standard"),
            PROMPT,
            Pause(1.0),
            Type("the discount tests are failing. find out why, fix it, and prove it. keep the explanation short."),
            TURN_DONE,
            PROMPT,
            Pause(4.0),
        ],
    ),
    (
        "mode-vibe",
        &[
            PROMPT,
            Type("/mode vibe"),
            PROMPT,
            Pause(1.0),
            Type("the discount tests are failing. find out why, fix it, and prove it. keep the explanation short."),
            TURN_DONE,
            PROMPT,
            Pause(4.0),
        ],
    ),
    (
        "views",
        &[
            PROMPT,
            Type("/view default"),
            PROMPT,
            Type("read shop/tax.py and run the cart tests. explain the tax rounding in two bullets."),
            TURN_DONE,
            PROMPT,
            Pause(2.0),
            Type("/view verbose"),
            PROMPT,
            Type("read shop/catalog.py and run the cart tests. explain the catalog in two bullets."),
            TURN_DONE,
            PROMPT,
            Pause(2.0),
            Type("/view adhd"),
            PROMPT,
            Type("read shop/cart.py and run the cart tests. explain the cart in three bullets."),
            PROMPT,
            Pause(4.0),
        ],
    ),
    (
        "verified-goal",
        &[
            Wait(&[Lit("goal done")]),
            Pause(4.0),
        ],
    ),
    (
        "delegation",
        &[
            PROMPT,
            Type("/agents sub"),
            PROMPT,
            Type("use one subagent to read shop/tax.py and explain its rounding rule in two bullets. do not change files."),
            TURN_DONE,
            PROMPT,
            Pause(3.0),
            Type("/agents auto"),
            PROMPT,
            Type("read shop/catalog.py and tell me the SKU names in one line. no changes."),
            TURN_DONE,
            PROMPT,
            Pause(3.0),
            Type("/agents off"),
            PROMPT,
            Type("read shop/cart.py yourself and name its entry point. no changes."),
            TURN_DONE,
            PROMPT,
            Pause(3.0),
        ],
    ),
    (
        "permissions",
        &[
            PROMPT,
            Type("/permissions read-only"),
            PROMPT,
            Type("/permissions check python3 -m unittest -q"),
            PROMPT,
            Pause(1.5),
            Type("/permissions ask"),
            PROMPT,
            Type("/permissions check python3 -m unittest -q"),
            PROMPT,
            Pause(1.5),
            Type("/permissions auto"),
            PROMPT,
            Type("/permissions check cargo test"),
            PROMPT,
            Type("/permissions check git push origin main"),
            PROMPT,
            Pause(1.5),
            Type("/permissions yolo"),
            PROMPT,
            Type("/permissions check git push --force"),
            PROMPT,
            Pause(4.0),
        ],
    ),
    (
        "hero",
        &[
            PROMPT,
            Pause(2.0),
            Type("the discount tests are failing. find out why, fix it, and prove it."),
            TURN_DONE,
            Pause(4.0),
        ],
    ),
    (
        "swarm",
        &[
            PROMPT,
            Pause(0.8),
            Type("/agents swarm"),
            PROMPT,
            Type("/swarm size 2"),
            PROMPT,
            Type("use a swarm of two workers: one inspect shop/discounts.py, one inspect shop/tax.py. report one concrete issue per module. do not edit files."),
            TURN_DONE,
            Pause(4.0),
        ],
    ),
    (
        "goal",
        &[
            PROMPT,
            Pause(0.8),
            Type("/goal add a `python3 -m shop.cli total <sku> <qty>` command that prints the price, with a test"),
            Wait(&[Lit("goal "), Either(&["done", "blocked", "paused"])]),
            Pause(4.0),
        ],
    ),
    (
        "compact",
        &[
            PROMPT,
            Pause(0.8),
            Type("read src/context.rs and src/agent.rs, then explain in five bullets how compaction works"),
            TURN_DONE,
            PROMPT,
            Type("/context"),
            PROMPT,
            Type("/compact keep how the working set is built"),
            Wait(&[Lit("compacted")]),
            PROMPT,
            Type("without reading anything again: which function builds the working set, and what four things does it track?"),
            TURN_DONE,
            Pause(4.0),
        ],
    ),
    (
        "focus",
        &[
            PROMPT,
            Pause(0.8),
            Type("/view adhd"),
            PROMPT,
            Type("read every module in shop/ and tell me how a cart total is computed"),
            Wait(&[Lit("✓ "), Digits, Lit("s")]),
            PROMPT,
            Type("/remember gotcha: money is in cents; never use floats for prices"),
            PROMPT,
            Type("/memory"),
            PROMPT,
            Type("/permissions check rm -rf build && git push --force"),
            PROMPT,
            Type("/tokens"),
            PROMPT,
            Pause(3.5),
            Type("/view default"),
            PROMPT,
            Pause(1.0),
        ],
    ),
    // Run from a copy of demo/shop with an NVIDIA key: one model fixes, a bigger one reviews.
    (
        "models",
        &[
            PROMPT,
            Pause(1.2),
            Type("/doctor"),
            Wait(&[Lit("model "), Line, Lit("→")]),
            PROMPT,
            Pause(3.0),
            Type("the discount tests are failing. find out why, fix it, and prove it."),
            TURN_DONE,
            PROMPT,
            Pause(2.0),
            Type("/model nvidia/nemotron-3-ultra-550b-a55b"),
            PROMPT,
            Type("you're a second pair of eyes now. review the fix that was just made: is there any case it still gets wrong? three bullets."),
            TURN_DONE,
            Pause(4.0),
        ],
    ),
];

struct Recorder {
    pty: Pty,
    utf8: Utf8,
    start: Instant,
    events: Vec<(f64, String)>,
    screen: String,
    rng: u64,
}

impl Recorder {
    /// Records output for `secs`, or until the terminal closes.
    fn pump(&mut self, secs: f64) -> Result<()> {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Chunk::Data(bytes) = self.pty.read(left)? else { return Ok(()) };
            let text = self.utf8.decode(&bytes);
            if text.is_empty() {
                continue;
            }
            self.events.push((self.start.elapsed().as_secs_f64(), text.clone()));
            self.screen.push_str(&text);
        }
        Ok(())
    }

    /// A typing delay between 25 and 70 ms.
    fn keystroke(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        0.025 + 0.045 * (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub fn command(args: &[String]) -> Result<ExitCode> {
    let (Some(scene), Some(out)) = (args.first(), args.get(1)) else {
        bail!("usage: cargo xtask record <scene> <out.cast> [-- rusty args]");
    };
    let extra = args.iter().position(|a| a == "--").map_or(&[][..], |i| &args[i + 1..]);
    let Some((_, steps)) = SCENES.iter().find(|(name, _)| name == scene) else {
        let names: Vec<_> = SCENES.iter().map(|(name, _)| *name).collect();
        bail!("unknown scene {scene:?}; one of {}", names.join(", "));
    };
    let mut cmd = Command::new(std::env::var_os("RUSTY_BIN").unwrap_or_else(|| "rusty".into()));
    cmd.arg0("rusty").args(extra);
    cmd.env("COLORTERM", "truecolor").env("TERM", "xterm-256color").env("COLUMNS", COLS.to_string());
    let wall = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let seed = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64 | 1;
    let mut r = Recorder {
        pty: Pty::spawn(cmd, ROWS, COLS)?,
        utf8: Utf8::default(),
        start: Instant::now(),
        events: Vec::new(),
        screen: String::new(),
        rng: seed,
    };

    let mut since = 0; // waits match output produced after the last Enter
    for step in *steps {
        match *step {
            Wait(pat) => {
                // Match output produced since the last Enter or the last match.
                let deadline = Instant::now() + Duration::from_secs(300);
                let raw = loop {
                    let raw = find(pat, &r.screen[since..]);
                    if raw.is_some() || find(pat, &strip_ansi(&r.screen[since..])).is_some() {
                        break raw;
                    }
                    if Instant::now() > deadline {
                        r.pty.kill();
                        bail!("scene {scene:?} timed out waiting for {pat:?}; no completed cast written");
                    }
                    r.pump(0.2)?;
                };
                if let Some((_, end)) = raw {
                    since += end;
                }
                r.pump(0.3)?;
            }
            Type(text) => {
                for ch in text.chars() {
                    r.pty.send(ch.encode_utf8(&mut [0; 4]).as_bytes())?;
                    let delay = r.keystroke();
                    r.pump(delay)?;
                }
                r.pump(0.35)?;
                since = r.screen.len();
                r.pty.send(b"\r")?;
                r.pump(0.2)?;
            }
            Pause(secs) => r.pump(secs)?,
        }
    }
    // A one-shot goal may already have closed its terminal after completion.
    let _ = r.pty.send(b"\x04");
    r.pump(1.5)?;
    r.pty.kill();

    let mut cast = format!(
        "{{\"version\": 2, \"width\": {COLS}, \"height\": {ROWS}, \"timestamp\": {wall}, \
         \"env\": {{\"TERM\": \"xterm-256color\"}}}}\n"
    );
    for (t, text) in &r.events {
        cast.push_str(&format!("[{t:.4}, \"o\", {}]\n", quote(text)));
    }
    std::fs::write(out, cast)?;
    match r.events.last() {
        Some((t, _)) => eprintln!("{out}: {} events, {t:.0}s", r.events.len()),
        None => eprintln!("{out}: empty"),
    }
    Ok(ExitCode::SUCCESS)
}
