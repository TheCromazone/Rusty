//! Hybrid mode: reasoning and memory stay local; only filesystem and shell
//! tools move into an already started sandbox through the bridge.

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::Arc;

use super::bridge::Bridge;
use super::Remote;

#[derive(clap::Args, Clone, Debug)]
#[command(group = clap::ArgGroup::new("task").multiple(false))]
pub struct HybridArgs {
    /// ID/name of an already started sandbox (never auto-created or woken)
    #[arg(long)]
    pub sandbox: String,
    /// Absolute project directory inside the sandbox
    #[arg(long)]
    pub workspace: String,
    /// Local macOS/Linux Rusty binary
    #[arg(long, default_value = "target/debug/rusty")]
    pub local_binary: PathBuf,
    #[arg(long, group = "task")]
    pub goal: Option<String>,
    /// Omit for interactive mode
    #[arg(long, group = "task")]
    pub prompt: Option<String>,
    #[arg(long, default_value = "auto", value_parser = ["auto", "careful", "standard", "vibe"])]
    pub mode: String,
    #[arg(long, default_value = "auto", value_parser = ["off", "sub", "swarm", "auto"])]
    pub agents: String,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=8))]
    pub swarm_max: u8,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, default_value = "learn", value_parser = ["off", "recall", "learn", "reflect", "deep"])]
    pub memory_mode: String,
    #[arg(long, default_value = "auto", value_parser = ["read-only", "ask", "auto", "yolo"])]
    pub permissions: String,
    #[arg(long)]
    pub stats: bool,
    /// Local path for the session receipt
    #[arg(long)]
    pub trajectory: Option<PathBuf>,
    /// Resume the latest local session for this sandbox/workspace
    #[arg(long = "continue")]
    pub resume: bool,
    /// Stable local advisor identity across replacement sandboxes (on/deep memory)
    #[arg(long)]
    pub project_id: Option<String>,
}

fn absolute(p: &std::path::Path) -> PathBuf {
    let expanded = match p.to_str().and_then(|s| s.strip_prefix("~/")) {
        Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_else(|| p.into()),
        None => p.into(),
    };
    expanded.canonicalize().unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(expanded))
}

/// The local rusty command line; validated before any API call.
pub fn command(args: &HybridArgs, placement: &str) -> Result<Vec<String>> {
    let binary = absolute(&args.local_binary);
    if !binary.is_file() {
        bail!("local Rusty binary not found: {}; run cargo build --bins", binary.display());
    }
    let mut words: Vec<String> = vec![
        binary.display().to_string(),
        "--tools".into(),
        placement.into(),
        "--mode".into(),
        args.mode.clone(),
        "--agents".into(),
        args.agents.clone(),
        "--memory".into(),
        args.memory_mode.clone(),
        "--permissions".into(),
        args.permissions.clone(),
        "--swarm-max".into(),
        args.swarm_max.to_string(),
    ];
    if let Some(model) = &args.model {
        words.extend(["--model".into(), model.clone()]);
    }
    if args.stats {
        words.push("--stats".into());
    }
    if args.resume {
        words.push("--continue".into());
    }
    if let Some(t) = &args.trajectory {
        words.extend(["--trajectory".into(), absolute(t).display().to_string()]);
    }
    if let Some(goal) = &args.goal {
        words.extend(["--goal".into(), goal.clone()]);
    } else if let Some(prompt) = &args.prompt {
        words.extend(["--".into(), prompt.clone()]);
    }
    Ok(words)
}

extern "C" fn ignore_interrupt(_: libc::c_int) {}

/// While alive, SIGINT reaches the child only. A caught handler (not
/// SIG_IGN) resets at exec, so rusty still gets Ctrl-C and stops its turn
/// without tearing down the bridge.
struct ShieldInterrupt(libc::sigaction);

impl ShieldInterrupt {
    fn new() -> Self {
        // SAFETY: plain sigaction calls with zeroed, then filled, structs.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = ignore_interrupt as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            let mut previous: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGINT, &action, &mut previous);
            Self(previous)
        }
    }
}

impl Drop for ShieldInterrupt {
    fn drop(&mut self) {
        // SAFETY: restores the action saved in `new`.
        unsafe {
            libc::sigaction(libc::SIGINT, &self.0, std::ptr::null_mut());
        }
    }
}

/// The child's command, its extra environment, and what it returns.
pub type Spawn<'a> = dyn FnMut(&[String], &[(String, String)]) -> Result<i32> + 'a;

/// Attaches to a started sandbox, serves the bridge for the lifetime of the
/// local rusty, and returns its exit code. Never creates, wakes, stops or
/// deletes the sandbox.
pub fn attach(state: &str, remote: Arc<dyn Remote>, args: &HybridArgs, spawn: &mut Spawn) -> Result<i32> {
    attach_backend(state, remote, args, "daytona", spawn)
}

pub fn attach_backend(
    state: &str,
    remote: Arc<dyn Remote>,
    args: &HybridArgs,
    placement: &str,
    spawn: &mut Spawn,
) -> Result<i32> {
    let command = command(args, placement)?;
    if !state.eq_ignore_ascii_case("started") {
        bail!(
            "hybrid attaches only to a started sandbox; it never creates or wakes one. \
             Start it explicitly if you intend to consume compute credits."
        );
    }
    eprintln!(
        "☾ local reasoning + memory · tools in {}:{} · workers share this workspace",
        remote.id(),
        args.workspace
    );
    eprintln!("☾ attached host stays running after exit; this command never creates, stops or deletes it");
    let bridge = Bridge::start(remote, &args.workspace)?;
    let mut env = bridge.env();
    if let Some(id) = args.project_id.as_deref().filter(|p| !p.is_empty()) {
        env.push(("RUSTY_PROJECT_ID".into(), id.into()));
    }
    let _shield = ShieldInterrupt::new();
    spawn(&command, &env)
}

/// Runs the local rusty with the bridge's variables added to this
/// environment.
pub fn run_child(command: &[String], env: &[(String, String)]) -> Result<i32> {
    use std::os::unix::process::ExitStatusExt;
    let status = std::process::Command::new(&command[0]).args(&command[1..]).envs(env.iter().cloned()).status()?;
    Ok(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}
