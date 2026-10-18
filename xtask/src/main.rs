//! Repository tasks: `cargo xtask <command>`.
//!
//!     cargo xtask qa [--live|--perf|--cloud] [--list]   unified quality gate
//!     cargo xtask check-message FILE   one commit message (`-` reads stdin)
//!     cargo xtask check-body           a pull request description, on stdin
//!     cargo xtask install-hooks        check every commit message locally
//!     cargo xtask eval [TASKS]         run the evals against a live model (see eval.rs)
//!     cargo xtask eval-completion     offline completion contracts and grader counterexamples
//!     cargo xtask eval-report FILE...  summarise eval runs from target/evals/*.jsonl
//!     cargo xtask memory-smoke [--live] [--binary PATH] [--report PATH]
//!                                      memory hooks through the real CLI
//!     cargo xtask memory-bench [--binary PATH] [--report PATH]
//!                                      memory advice latency, 512 lessons
//!     cargo xtask memory-summary FILE  a smoke receipt as a Markdown table
//!     cargo xtask tty [CHECK]...       real-terminal checks (see tty.rs)
//!     cargo xtask tty-capture | tty-inspect
//!     cargo xtask record SCENE OUT     a demo session as an asciinema cast
//!     cargo xtask bench [--binary PATH] [--json PATH] [--runs N]
//!                                      startup, sessions, redaction, recall, turn and stream costs

mod bench;
mod eval;
mod ignores;
mod json;
mod memory;
mod message;
mod native;
mod pattern;
mod provider;
mod pty;
mod qa;
mod record;
mod report;
mod screen;
mod testing;
mod tty;
mod worktree;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("protocol-fixtures") => native::protocol().map(|()| ExitCode::SUCCESS),
        Some("cli-gate") => native::cli_gate(rest).map(|()| ExitCode::SUCCESS),
        Some("fixture-provider") => native::fixture_provider(rest).map(|()| ExitCode::SUCCESS),
        Some("qa") => qa::command(rest),
        Some("flaky") => testing::flaky(rest),
        Some("worktree") => worktree::command(rest),
        Some("test-suite") => {
            if !matches!(rest, [] | [_]) || rest.first().is_some_and(|a| a != "--changed") {
                bail!("usage: cargo xtask test-suite [--changed]");
            }
            ignores::check()?;
            testing::tests(!rest.is_empty()).map(|_| ExitCode::SUCCESS)
        }
        Some("check-message") => {
            let text = read_arg(rest.first().map_or("-", String::as_str))?;
            Ok(verdict(message::commit_problem(&text)))
        }
        Some("check-body") => {
            let text = read_arg("-")?;
            let problem = message::has_attribution(&text)
                .then(|| "Write the description as yourself; remove tool attribution.".to_string());
            Ok(verdict(problem))
        }
        Some("install-hooks") => install_hooks().map(|()| ExitCode::SUCCESS),
        Some("eval") => eval::run(rest.first().map_or("", String::as_str)).map(passed),
        Some("eval-completion") => {
            sh("cargo", &["test", "--workspace", "completion_contract", "--", "--nocapture"])?;
            Ok(ExitCode::SUCCESS)
        }
        Some("eval-report") => report::run(rest).map(passed),
        Some("memory-smoke") => memory::smoke_command(rest),
        Some("memory-bench") => memory::bench_command(rest),
        Some("memory-summary") => memory::summary_command(rest),
        Some("tty") => tty::command(rest),
        Some("tty-capture") => tty::capture(rest),
        Some("tty-inspect") => tty::inspect(rest),
        Some("record") => record::command(rest),
        Some("bench") => bench::command(rest),
        _ => {
            eprintln!(
                "usage: cargo xtask <qa [--live|--perf|--cloud] [--list] | check-message FILE|- | check-body | install-hooks\n    \
                 | eval [TASKS] | eval-completion | eval-report FILE.jsonl...\n    \
                 | memory-smoke [...] | memory-bench [...] | memory-summary FILE\n    \
                 | tty [ctrl-c|approval|steady]... [--binary PATH] [--live]\n    \
                 | tty-capture --binary PATH --output DIR | tty-inspect DIR\n    \
                 | record SCENE OUT.cast [-- RUSTY ARGS]
    \
                 | bench [--binary PATH] [--json PATH] [--runs N]>"
            );
            Ok(ExitCode::from(2))
        }
    }
}

fn read_arg(path: &str) -> Result<String> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
    }
}

fn passed(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn verdict(problem: Option<String>) -> ExitCode {
    match problem {
        Some(p) => {
            eprintln!("{p}");
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask lives in the repository").to_path_buf()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root().join("target"), PathBuf::from)
}

/// Builds rusty and returns the debug binary.
pub fn debug_rusty() -> Result<PathBuf> {
    sh("cargo", &["build", "--bin", "rusty"])?;
    Ok(target_dir().join("debug/rusty"))
}

/// Runs a command from the repository root and fails if it does.
fn sh(program: &str, args: &[&str]) -> Result<()> {
    let status =
        Command::new(program).args(args).current_dir(root()).status().with_context(|| format!("starting {program}"))?;
    if !status.success() {
        bail!("{program} {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Points git at a commit-msg hook that runs `cargo xtask check-message`.
/// The hook is written into this clone's .git, so nothing but Rust is
/// committed.
fn install_hooks() -> Result<()> {
    let out = Command::new("git").args(["rev-parse", "--git-path", "hooks"]).current_dir(root()).output()?;
    if !out.status.success() {
        bail!("not a git checkout");
    }
    let dir = root().join(String::from_utf8_lossy(&out.stdout).trim());
    std::fs::create_dir_all(&dir)?;
    let hook = dir.join("commit-msg");
    std::fs::write(&hook, "#!/bin/sh\nexec cargo xtask check-message \"$1\"\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    // An older clone may still point at the removed .githooks directory.
    let _ = Command::new("git").args(["config", "--unset", "core.hooksPath"]).current_dir(root()).status();
    println!("installed {}", hook.display());
    Ok(())
}
