//! Runs eval tasks headless in fresh temp copies and scores each with its own
//! check, which the agent never sees. Results go to target/evals/<stamp>.jsonl.
//!
//! Single-turn tasks (evals/<name>/prompt.txt) run as one prompt.
//! Multi-turn tasks (evals/multi/<name>/turns.txt) are replayed line by line
//! through the interactive REPL, one line per user message:
//!   ## session      start a new process (memory must carry over)
//!   ## env K=V      set an environment variable from here on
//!   ## goal         the next line runs as an autonomous --goal
//!   ## verify CMD   the goal's fixed acceptance command (--verify CMD)
//!   ## run CMD      the harness runs CMD in the working copy before the next
//!                   session; the agent never sees it. RUSTY_HOME, RUSTY_BIN,
//!                   RUSTY_MEMORY and EVAL_DIR are set, so it can change the
//!                   code between sessions or seed memory
//!
//!     cargo xtask eval                  # everything, EVAL_JOBS at a time (default 4)
//!     cargo xtask eval multi            # only multi-turn tasks
//!     cargo xtask eval rust-slugify     # one task (or a comma-separated list)
//!
//! EVAL_MODELS="claude-opus-5-5 gpt-5 nvidia/nemotron-3-super-120b-a12b" runs
//! every task on each model (each needs its provider's key), and
//! EVAL_REPEATS=3 runs each pair three times. EVAL_TIMEOUT (default 600) caps
//! each rusty process in seconds; EVAL_TRANSCRIPTS=0 drops the miss details
//! from the log. The summary compares models.
//!
//! EVAL_MEMORY="off recall learn reflect" runs every model under each memory
//! level (any value `rusty --memory` takes). Each row records its
//! setting and the summary groups by model and memory. Unset, rusty's own
//! default applies and rows carry no memory field.
//!
//! A single-turn task may include verify.txt with a fixed acceptance command.
//! It then runs with --goal and --verify; the independent check.sh still
//! grades the actual result and stays outside the working copy.
//! Each run gets a budget ledger (RUSTY_BUDGET_LEDGER) in its home. A
//! timeout that spent at least half its wall time waiting on provider
//! retries is scored infra, since it measures the provider, not the agent.
//!
//! EVAL_BINARY selects an already qualified artifact instead of rebuilding it.
//! Keep its source revision and hash in the experiment's artifact receipt.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::report;

const DEFAULT_MODEL: &str = "nvidia/nemotron-3-super-120b-a12b";

/// The endpoint failing is an infrastructure error, not an agent failure.
/// Any line of the agent's output matching
/// `giving up after|HTTP 5[0-9][0-9]|request failed|connection to the model closed|API error: .*(overloaded|api_error|rate_limit)|is served by .* is not set`.
pub fn is_infra_line(line: &str) -> bool {
    line.contains("giving up after")
        || line.contains("request failed")
        || line.contains("connection to the model closed")
        || status_code(line, b"5")
        || line.find("API error: ").is_some_and(|i| {
            let rest = &line[i + "API error: ".len()..];
            ["overloaded", "api_error", "rate_limit"].iter().any(|w| rest.contains(w))
        })
        || line.find("is served by ").is_some_and(|i| line[i + "is served by ".len()..].contains(" is not set"))
}

/// The lines worth quoting as an infra error's reason:
/// `giving up after|HTTP [45][0-9][0-9]|request failed|connection to the model closed|API error|is not set`.
pub fn is_reason_line(line: &str) -> bool {
    line.contains("model budget exhausted")
        || line.contains("giving up after")
        || status_code(line, b"45")
        || line.contains("request failed")
        || line.contains("connection to the model closed")
        || line.contains("API error")
        || line.contains("is not set")
}

/// `HTTP ` followed by a three-digit status whose first digit is in `first`.
fn status_code(line: &str, first: &[u8]) -> bool {
    line.match_indices("HTTP ").any(|(i, _)| {
        let d = &line.as_bytes()[i + 5..];
        d.len() >= 3 && first.contains(&d[0]) && d[1].is_ascii_digit() && d[2].is_ascii_digit()
    })
}

/// Whether a task is picked by the command-line filter: empty for all,
/// `multi` for the multi-turn ones, otherwise a comma-separated list of names.
pub fn selected(filter: &str, name: &str, multi: bool) -> bool {
    match filter {
        "" => true,
        "multi" => multi,
        list => list.split(',').any(|n| n == name),
    }
}

#[derive(Clone, Debug)]
pub struct Task {
    pub name: String,
    pub dir: PathBuf,
}

/// Every runnable task under evals/ then evals/multi/, in name order.
pub fn tasks(evals: &Path, filter: &str) -> Result<Vec<Task>> {
    let mut out = Vec::new();
    for (base, multi) in [(evals.to_path_buf(), false), (evals.join("multi"), true)] {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&base)
            .with_context(|| format!("reading {}", base.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
            .collect();
        dirs.sort();
        for dir in dirs {
            if !dir.join("prompt.txt").is_file() && !dir.join("turns.txt").is_file() {
                continue;
            }
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            if selected(filter, &name, multi) {
                out.push(Task { name, dir });
            }
        }
    }
    Ok(out)
}

/// One rusty process of a multi-turn task.
#[derive(Debug, Default, PartialEq)]
pub struct Session {
    /// `K=V` pairs from the `## env` lines read so far.
    pub env: Vec<String>,
    /// Run the first line as `--goal` instead of piping every line in.
    pub goal: bool,
    /// The goal's fixed acceptance command.
    pub verify: Option<String>,
    /// Harness commands to run before this session starts.
    pub pre: Vec<String>,
    pub lines: Vec<String>,
}

/// Splits turns.txt into the processes to run. `## run` lines after the
/// last session become a final session with no lines, run before the check.
pub fn sessions(turns: &str) -> Vec<Session> {
    let mut out = Vec::new();
    let mut env = Vec::new();
    let mut next = Session::default();
    // A goal, verify or run seen before any line stays with the next session.
    let mut flush = |env: &Vec<String>, next: &mut Session, end: bool| {
        if !next.lines.is_empty() || (end && !next.pre.is_empty()) {
            next.env = env.clone();
            out.push(std::mem::take(next));
        }
    };
    for line in turns.split('\n') {
        match line {
            "## session" => flush(&env, &mut next, false),
            "## goal" => {
                flush(&env, &mut next, false);
                next.goal = true;
            }
            "" => {}
            l => {
                if let Some(kv) = l.strip_prefix("## env ") {
                    env.push(kv.to_string());
                } else if let Some(cmd) = l.strip_prefix("## verify ") {
                    next.verify = Some(cmd.to_string());
                } else if let Some(cmd) = l.strip_prefix("## run ") {
                    if !next.lines.is_empty() {
                        flush(&env, &mut next, false);
                    }
                    next.pre.push(cmd.to_string());
                } else {
                    next.lines.push(l.to_string());
                }
            }
        }
    }
    flush(&env, &mut next, true);
    out
}

struct Config {
    root: PathBuf,
    bin: PathBuf,
    limit: Duration,
    out: PathBuf,
    logs: PathBuf,
    transcripts: bool,
}

/// An environment variable, with empty counting as unset.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn number(name: &str, default: u64) -> Result<u64> {
    match var(name) {
        Some(v) => v.trim().parse().with_context(|| format!("{name}={v} is not a whole number")),
        None => Ok(default),
    }
}

/// Runs the selected tasks on every model and prints the report. True when
/// every scored run passed.
pub fn run(filter: &str) -> Result<bool> {
    let root = crate::root();
    let bin = match var("EVAL_BINARY") {
        Some(path) => std::fs::canonicalize(path).context("EVAL_BINARY must exist")?,
        None => {
            let status = Command::new("cargo").args(["build", "--release", "-q"]).current_dir(&root).status()?;
            if !status.success() {
                bail!("cargo build --release failed ({status})");
            }
            crate::target_dir().join("release/rusty")
        }
    };
    let limit = number("EVAL_TIMEOUT", 600)?;
    let models = var("EVAL_MODELS").or_else(|| var("RUSTY_MODEL")).unwrap_or_else(|| DEFAULT_MODEL.into());
    let repeats = number("EVAL_REPEATS", 1)?;
    let jobs = number("EVAL_JOBS", 4)?.max(1) as usize;
    // An empty setting leaves rusty's own default in force.
    let memories: Vec<String> = match var("EVAL_MEMORY") {
        Some(list) => list.split_whitespace().map(str::to_string).collect(),
        None => vec![String::new()],
    };
    let now = SystemTime::now();
    let stamp = format!("{}-{}-{:09}", stamp(now), std::process::id(), now.duration_since(UNIX_EPOCH)?.subsec_nanos());
    let cfg = Config {
        bin,
        limit: Duration::from_secs(limit),
        out: root.join(format!("target/evals/{stamp}.jsonl")),
        logs: root.join(format!("target/evals/{stamp}")),
        transcripts: var("EVAL_TRANSCRIPTS").is_none_or(|v| v == "1"),
        root,
    };
    std::fs::create_dir_all(&cfg.logs)?;
    OpenOptions::new().write(true).create_new(true).open(&cfg.out)?;

    let tasks = tasks(&cfg.root.join("evals"), filter)?;
    let mut queue = Vec::new();
    for model in models.split_whitespace() {
        for memory in &memories {
            for rep in 1..=repeats {
                queue.extend(tasks.iter().map(|t| (t, Run { model, memory, rep })));
            }
        }
    }
    let memory = var("EVAL_MEMORY").map(|m| format!(" · memory {m}")).unwrap_or_default();
    println!("models {models}{memory} · {repeats}x · timeout {limit}s per session");
    let next = AtomicUsize::new(0);
    let broken = AtomicUsize::new(0);
    let jsonl = Mutex::new(());
    std::thread::scope(|s| {
        for _ in 0..jobs.min(queue.len()) {
            s.spawn(|| {
                while let Some((task, run)) = queue.get(next.fetch_add(1, Ordering::SeqCst)) {
                    if let Err(e) = run_task(&cfg, task, run, &jsonl) {
                        broken.fetch_add(1, Ordering::SeqCst);
                        eprintln!("{} {} #{}: {e:#}", task.name, run.label(), run.rep);
                    }
                }
            });
        }
    });

    let report = report::run(&[cfg.out.to_string_lossy().into_owned()]);
    // Runs the harness could not carry out are missing from the report, so
    // they must not let an eval pass by omission.
    match broken.into_inner() {
        0 => report,
        n => bail!("{n} run(s) could not be carried out; see the errors above"),
    }
}

/// One cell of the eval matrix: which model, under which memory setting
/// (empty for rusty's default), and which repeat.
struct Run<'a> {
    model: &'a str,
    memory: &'a str,
    rep: u64,
}

impl Run<'_> {
    /// `model`, or `model · memory` when a memory setting is pinned.
    fn label(&self) -> String {
        match self.memory {
            "" => self.model.to_string(),
            m => format!("{} · {m}", self.model),
        }
    }

    /// The environment every process of this run gets.
    fn env(&self) -> Vec<(&'static str, &str)> {
        let mut env = vec![("RUSTY_MODEL", self.model)];
        if !self.memory.is_empty() {
            env.push(("RUSTY_MEMORY", self.memory));
        }
        env
    }
}

enum Exit {
    Done(bool),
    TimedOut,
}

fn single_args<'a>(prompt: &'a str, check: Option<&'a str>) -> Vec<&'a str> {
    match check {
        Some(command) => vec!["--goal", prompt, "--verify", command],
        None => vec![prompt],
    }
}

/// Runs one rusty process with a hard time limit, appending to the home's
/// stdout and stderr. Extra args pass through. Session `n` (from 1) writes its
/// own trajectory, so a later session can't overwrite what an earlier one did.
#[allow(clippy::too_many_arguments)]
fn agent(
    cfg: &Config,
    work: &Path,
    home: &Path,
    run: &Run,
    n: usize,
    env: &[String],
    args: &[&str],
    input: Option<String>,
) -> Result<Exit> {
    let append = |name: &str| OpenOptions::new().create(true).append(true).open(home.join(name));
    let (stdout, stderr) = (append("stdout")?, append("stderr")?);
    let mut cmd = Command::new(&cfg.bin);
    cmd.args(["--yolo", "--stats"])
        .arg("--trajectory")
        .arg(home.join(trajectory(n)))
        .args(args)
        .current_dir(work)
        .env("RUSTY_HOME", home)
        // One ledger per run: it outlives a killed process, so a timeout
        // still shows how long the provider kept the agent waiting.
        .env("RUSTY_BUDGET_LEDGER", home.join("budget.json"))
        .envs(run.env())
        .env("NO_COLOR", "1")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(stdout)
        .stderr(stderr.try_clone()?);
    for kv in env {
        if let Some((k, v)) = kv.split_once('=') {
            cmd.env(k, v);
        }
    }
    // A binary that cannot start is a broken harness, not a failed agent run.
    let mut child = cmd.spawn().with_context(|| format!("starting {}", cfg.bin.display()))?;
    if let (Some(text), Some(mut pipe)) = (input, child.stdin.take()) {
        // A thread, so a child that stops reading can't stall the time limit.
        std::thread::spawn(move || pipe.write_all(text.as_bytes()));
    }
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Exit::Done(status.success()));
        }
        if start.elapsed() >= cfg.limit {
            // Let the coordinator cancel owned activity groups before a hard
            // kill. A timeout stays a timeout even if shutdown is graceful.
            #[cfg(unix)]
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
            let grace = Instant::now();
            while grace.elapsed() < Duration::from_secs(3) && child.try_wait()?.is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Exit::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs every session; timed out if any one of them was.
fn run_multi(cfg: &Config, task: &Task, work: &Path, home: &Path, run: &Run) -> Result<Exit> {
    let turns = std::fs::read_to_string(task.dir.join("turns.txt"))?;
    let sessions = sessions(&turns);
    validate(&sessions).with_context(|| format!("{}: turns.txt", task.name))?;
    let mut timed_out = false;
    let mut success = true;
    let mut n = 0;
    for s in sessions {
        for cmd in &s.pre {
            setup(cfg, task, work, home, run, &s.env, cmd)?;
        }
        if s.lines.is_empty() {
            continue;
        }
        n += 1;
        let exit = if s.goal {
            let mut args = vec!["--goal", s.lines[0].as_str()];
            if let Some(check) = &s.verify {
                args.extend(["--verify", check.as_str()]);
            }
            agent(cfg, work, home, run, n, &s.env, &args, None)?
        } else {
            let input: String = s.lines.iter().map(|l| format!("{l}\n")).collect();
            agent(cfg, work, home, run, n, &s.env, &[], Some(input))?
        };
        timed_out |= matches!(exit, Exit::TimedOut);
        success &= matches!(exit, Exit::Done(true));
    }
    Ok(if timed_out { Exit::TimedOut } else { Exit::Done(success) })
}

/// A fixed check only means something on a goal, so a stray one is a task bug.
fn validate(sessions: &[Session]) -> Result<()> {
    match sessions.iter().position(|s| s.verify.is_some() && !s.goal) {
        Some(i) => bail!("session {} has `## verify` but is not a `## goal`", i + 1),
        None => Ok(()),
    }
}

/// A `## run` command: harness setup between sessions, hidden from the agent.
/// It failing means the task could not be set up, not that the agent failed.
fn setup(cfg: &Config, task: &Task, work: &Path, home: &Path, run: &Run, env: &[String], cmd: &str) -> Result<()> {
    let log = OpenOptions::new().create(true).append(true).open(home.join("run"))?;
    let mut c = Command::new("bash");
    c.args(["-c", cmd])
        .current_dir(work)
        .env("RUSTY_HOME", home)
        .env("RUSTY_BIN", &cfg.bin)
        .env("EVAL_DIR", &task.dir)
        .envs(run.env())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    for kv in env {
        if let Some((k, v)) = kv.split_once('=') {
            c.env(k, v);
        }
    }
    let status = c.status()?;
    if !status.success() {
        bail!("## run {cmd}: {status}");
    }
    Ok(())
}

/// Stops a memory daemon the run started, so it doesn't outlive its home.
fn stop_memory(cfg: &Config, home: &Path) {
    let daemon = cfg.bin.with_file_name("rusty-memoryd");
    if home.join("memory/advisor.sock").exists() && daemon.is_file() {
        let _ = Command::new(daemon)
            .arg("--home")
            .arg(home)
            .arg("stop")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn run_task(cfg: &Config, task: &Task, run: &Run, jsonl: &Mutex<()>) -> Result<()> {
    let (work, home) = (temp_dir()?, temp_dir()?);
    let result = score(cfg, task, run, &work, &home, jsonl);
    stop_memory(cfg, &home);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&home);
    result
}

/// The share of a timed-out run's wall time spent waiting on provider retries
/// at which the timeout counts as infrastructure rather than the agent.
const THROTTLED_SHARE: f64 = 0.5;

/// Functional correctness is independent of a healthy completed execution.
/// A passing patch cannot hide a fatal provider error or nonzero process exit.
fn verdict(exit: &Exit, functional: bool, stdout: &[u8], stderr: &[u8]) -> &'static str {
    let err = String::from_utf8_lossy(stderr);
    let budget = err.lines().any(|l| {
        l.contains("model budget exhausted") || (l.contains("HTTP 402") && l.contains("evaluation budget exhausted"))
    });
    if matches!(exit, Exit::TimedOut) {
        "timeout"
    } else if budget {
        "budget"
    } else if [stdout, stderr].iter().any(|b| String::from_utf8_lossy(b).lines().any(is_infra_line)) {
        "infra"
    } else if functional && matches!(exit, Exit::Done(true)) {
        "pass"
    } else {
        "fail"
    }
}

fn score(cfg: &Config, task: &Task, run: &Run, work: &Path, home: &Path, jsonl: &Mutex<()>) -> Result<()> {
    let (model, rep) = (run.model, run.rep);
    let _ = copy_dir(&task.dir.join("files"), work);
    let start = Instant::now();
    let exit = if task.dir.join("turns.txt").is_file() {
        run_multi(cfg, task, work, home, run)?
    } else {
        let prompt = std::fs::read_to_string(task.dir.join("prompt.txt"))?;
        let check_path = task.dir.join("verify.txt");
        let check = if check_path.is_file() { Some(std::fs::read_to_string(check_path)?) } else { None };
        if check.as_ref().is_some_and(|s| s.trim().is_empty()) {
            bail!("{}: verify.txt must contain a nonempty acceptance command", task.name);
        }
        let args = single_args(prompt.trim_end_matches('\n'), check.as_deref().map(str::trim));
        agent(cfg, work, home, run, 1, &[], &args, None)?
    };
    let secs = start.elapsed().as_secs();

    let stdout = std::fs::read(home.join("stdout")).unwrap_or_default();
    let stderr = std::fs::read(home.join("stderr")).unwrap_or_default();
    let functional = check(cfg, task, work, home, run)?;
    let verdict = verdict(&exit, functional, &stdout, &stderr);
    // Predeclared: a timeout that spent at least half its wall time waiting
    // on provider retries measures the provider, not the agent.
    let ledger: Value = std::fs::read_to_string(home.join("budget.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let waited = ledger["retry_wait_seconds"].as_f64().unwrap_or(0.0);
    let throttled = verdict == "timeout" && waited >= THROTTLED_SHARE * secs.max(1) as f64;
    // A run that read the harness's records (its own home, where earlier
    // sessions' transcripts and history live, or another run's directories)
    // measured the harness, not the agent: it is not scored, whatever the check said.
    let leak = outside_reads(home, work).or_else(|| seen_before_used(home, task));
    let verdict = match (&leak, throttled) {
        (Some(_), _) => "leak",
        (None, true) => "infra",
        (None, false) => verdict,
    };

    let cell = match run.memory {
        "" => model.replace('/', "_"),
        m => format!("{}.{}", model.replace('/', "_"), m.replace([',', '/'], "+")),
    };
    let dest = cfg.logs.join(cell).join(format!("{}.{rep}", task.name));
    std::fs::create_dir_all(&dest)?;
    for f in ["stdout", "stderr", "check", "run", "budget.json"] {
        let _ = std::fs::copy(home.join(f), dest.join(f));
    }
    for f in trajectories(home) {
        let _ = std::fs::copy(home.join(&f), dest.join(&f));
    }

    let stats: Vec<Value> = py_lines(&String::from_utf8_lossy(&stderr))
        .into_iter()
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let total = |k: &str| stats.iter().map(|r| r[k].as_i64().unwrap_or(0)).sum::<i64>();
    let (requests, prompt, completion) = (total("requests"), total("prompt"), total("completion"));
    let reason: Vec<String> = match verdict {
        "leak" => leak.iter().map(|p| format!("read outside its working copy: {p}")).collect(),
        _ if throttled => vec![format!("provider throttling: {waited:.0}s of {secs}s spent waiting to retry")],
        "fail" => {
            let check = String::from_utf8_lossy(&std::fs::read(home.join("check")).unwrap_or_default()).into_owned();
            last_line(&check).into_iter().collect()
        }
        // The endpoint's own words, so a run of these can be diagnosed from the jsonl.
        "infra" | "budget" => {
            let lines: Vec<String> = [&stdout, &stderr]
                .iter()
                .flat_map(|b| {
                    py_lines(&String::from_utf8_lossy(b)).iter().map(|l| l.trim().to_string()).collect::<Vec<_>>()
                })
                .collect();
            lines.into_iter().rfind(|l| is_reason_line(l)).into_iter().collect()
        }
        _ => Vec::new(),
    };
    let mut row: Value = serde_json::from_str(&row(
        &task.name,
        model,
        rep,
        verdict,
        secs,
        stats.len(),
        requests,
        prompt,
        completion,
        &reason,
    ))?;
    let goal = stats.last().map(|r| r["goal"].clone()).unwrap_or(Value::Null);
    row["false_completion"] = Value::Bool(goal == "done" && !functional);
    row["functional_pass"] = Value::Bool(functional);
    row["retry_wait_seconds"] = Value::from((waited * 10.0).round() / 10.0);
    row["goal"] = goal;
    row["model_budgets"] = Value::Array(stats.iter().filter_map(|r| r.get("model_budget").cloned()).collect());
    row["claims"] = Value::Array(stats.iter().filter_map(|r| r.get("claims").cloned()).collect());
    row["binary"] = Value::String(cfg.bin.display().to_string());
    if !run.memory.is_empty() {
        row["memory"] = Value::String(run.memory.to_string());
    }
    {
        let _lock = jsonl.lock().unwrap();
        OpenOptions::new().append(true).open(&cfg.out)?.write_all(format!("{row}\n").as_bytes())?;
    }
    let label = run.label();
    let short: String = label.chars().rev().take(24).collect::<Vec<_>>().into_iter().rev().collect();
    let why: String = reason.join(" ").chars().take(80).collect();
    println!(
        "{} {:<18} {short:<24} {verdict:<8} {secs:>4}s  {} session(s)  {:>5.0}k tok  {why}",
        report::mark(verdict),
        task.name,
        stats.len(),
        (prompt + completion) as f64 / 1000.0
    );

    // A miss explains itself in the log: the end of the agent's transcript, what
    // it changed, and the check's output (artifacts aren't always easy to fetch).
    if cfg.transcripts && matches!(verdict, "fail" | "timeout") {
        let mut miss =
            format!("::group::{} {label} #{rep}: {verdict}\n--- agent (last 60 lines)\n", task.name).into_bytes();
        miss.extend_from_slice(tail(&stdout, 60));
        miss.extend_from_slice(b"--- changes\n");
        let diff = Command::new("diff")
            .args(["-ruN", "--exclude=__pycache__", "--exclude=target", "--exclude=*.json"])
            .arg(task.dir.join("files"))
            .arg(work)
            .stderr(Stdio::null())
            .output();
        if let Ok(d) = diff {
            miss.extend_from_slice(head(&d.stdout, 120));
        }
        miss.extend_from_slice(b"--- check\n");
        miss.extend_from_slice(tail(&std::fs::read(home.join("check")).unwrap_or_default(), 20));
        miss.extend_from_slice(b"::endgroup::\n");
        std::io::stdout().lock().write_all(&miss)?;
    }
    Ok(())
}

/// Runs the task's hidden check in the agent's working copy. RUSTY_BIN and
/// RUSTY_MEMORY let a check ask rusty what it remembered.
fn check(cfg: &Config, task: &Task, work: &Path, home: &Path, run: &Run) -> Result<bool> {
    let log = File::create(home.join("check"))?;
    let status = Command::new("bash")
        .arg(task.dir.join("check.sh"))
        .current_dir(work)
        .env("EVAL_DIR", &task.dir)
        .env("RUSTY_HOME", home)
        .env("RUSTY_BIN", &cfg.bin)
        .envs(run.env())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()?;
    Ok(status.success())
}

/// One jsonl row, laid out exactly as Python's `json.dumps` writes it.
#[allow(clippy::too_many_arguments)]
pub fn row(
    task: &str,
    model: &str,
    rep: u64,
    verdict: &str,
    secs: u64,
    sessions: usize,
    requests: i64,
    prompt: i64,
    completion: i64,
    reason: &[String],
) -> String {
    let reason: Vec<String> = reason.iter().map(|r| py_json(r)).collect();
    format!(
        "{{\"task\": {}, \"model\": {}, \"rep\": {rep}, \"verdict\": {}, \"secs\": {secs}, \"sessions\": {sessions}, \
         \"requests\": {requests}, \"prompt\": {prompt}, \"completion\": {completion}, \"reason\": [{}]}}",
        py_json(task),
        py_json(model),
        py_json(verdict),
        reason.join(", ")
    )
}

/// A JSON string with non-ASCII escaped, as Python's `json.dumps` does.
pub fn py_json(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7f => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Lines as Python reads a text file: `\n`, `\r\n` and `\r` all end one.
fn py_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(['\r', '\n']) {
        out.push(&rest[..i]);
        let skip = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
        rest = &rest[i + skip..];
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// The last line of the stripped text (Python's `.strip().splitlines()[-1:]`).
fn last_line(text: &str) -> Option<String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
    if text.is_empty() {
        return None;
    }
    let breaks = ['\n', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}'];
    text.rsplit(breaks).next().map(str::to_string)
}

/// The last `n` lines, as `tail -n` prints them.
pub fn tail(b: &[u8], n: usize) -> &[u8] {
    if n == 0 {
        return &[];
    }
    let end = if b.ends_with(b"\n") { b.len() - 1 } else { b.len() };
    let mut seen = 0;
    for i in (0..end).rev() {
        if b[i] == b'\n' {
            seen += 1;
            if seen == n {
                return &b[i + 1..];
            }
        }
    }
    b
}

/// The first `n` lines, as `head -n` prints them.
pub fn head(b: &[u8], n: usize) -> &[u8] {
    let mut seen = 0;
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' {
            seen += 1;
            if seen == n {
                return &b[..=i];
            }
        }
    }
    b
}

/// Copies a directory tree like `cp -R src/. dst/`: modes kept, symlinks as links.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&from, &to)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Session `n`'s trajectory file; the first keeps the single-session name.
fn trajectory(n: usize) -> String {
    match n {
        1 => "trajectory.json".into(),
        n => format!("trajectory.{n}.json"),
    }
}

/// The trajectory files the sessions left in `home`, in no particular order.
/// A session that wrote none leaves a gap, not the end of the list.
fn trajectories(home: &Path) -> Vec<String> {
    let names = std::fs::read_dir(home).into_iter().flatten().flatten();
    names
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.starts_with("trajectory.") && f.ends_with(".json"))
        .collect()
}

/// The first path a tool call named outside the run's own working copy:
/// another eval directory (this run's home among them) or `$RUSTY_HOME`.
/// Only what the agent names is seen; a search of a parent directory that
/// never names the path is not.
fn outside_reads(home: &Path, work: &Path) -> Option<String> {
    let own = work.file_name()?.to_string_lossy().into_owned();
    for f in trajectories(home) {
        let Ok(text) = std::fs::read_to_string(home.join(f)) else { continue };
        let Ok(t) = serde_json::from_str::<Value>(&text) else { continue };
        let calls = t["messages"].as_array().into_iter().flatten().flat_map(|m| {
            m["tool_calls"].as_array().into_iter().flatten().filter_map(|c| c["function"]["arguments"].as_str())
        });
        for args in calls {
            if args.contains("RUSTY_HOME") {
                return Some("$RUSTY_HOME".into());
            }
            for (i, _) in args.match_indices("rusty-eval-") {
                let name: String = args[i..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
                if name != own && name.len() > "rusty-eval-".len() {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// A later session (the second on) that met one of the task's `leak.txt`
/// strings in a tool result before using it itself got it from outside its
/// memory: another run's files, `/tmp`, the harness's records. Memory reaches
/// a session through its prompt or the `recall` tool, so those don't count.
fn seen_before_used(home: &Path, task: &Task) -> Option<String> {
    let text = std::fs::read_to_string(task.dir.join("leak.txt")).ok()?;
    let secrets: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut later: Vec<String> = trajectories(home).into_iter().filter(|f| f != "trajectory.json").collect();
    later.sort();
    for f in later {
        let read = std::fs::read_to_string(home.join(&f)).ok();
        let Some(t) = read.and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
        let mut names = std::collections::HashMap::new();
        let mut used = vec![false; secrets.len()];
        for m in t["messages"].as_array().into_iter().flatten() {
            if m["role"] == "tool" {
                let name = m["tool_call_id"].as_str().and_then(|id| names.get(id)).map_or("", |n: &String| n.as_str());
                let content = m["content"].to_string();
                if name != "recall" && secrets.iter().zip(&used).any(|(s, u)| !u && content.contains(s)) {
                    return Some(format!("{f} met a memory-only string in a {name} result"));
                }
                continue;
            }
            let said = m.to_string();
            for (s, u) in secrets.iter().zip(used.iter_mut()) {
                *u |= said.contains(s);
            }
            for c in m["tool_calls"].as_array().into_iter().flatten() {
                if let (Some(id), Some(n)) = (c["id"].as_str(), c["function"]["name"].as_str()) {
                    names.insert(id.to_string(), n.to_string());
                }
            }
        }
    }
    None
}

/// A fresh, empty directory under the system temp dir (like `mktemp -d`).
fn temp_dir() -> Result<PathBuf> {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let base = std::env::temp_dir();
    loop {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
        let n = COUNT.fetch_add(1, Ordering::SeqCst);
        let dir = base.join(format!("rusty-eval-{}-{n}-{nanos:x}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
}

/// `YYYYmmdd-HHMMSS` in UTC.
pub fn stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_acceptance_uses_goal_mode_without_exposing_the_hidden_check() {
        assert_eq!(single_args("fix it", None), vec!["fix it"]);
        assert_eq!(
            single_args("fix it", Some("python3 test.py")),
            vec!["--goal", "fix it", "--verify", "python3 test.py"]
        );
    }

    #[test]
    fn task_selection() {
        assert!(selected("", "rust-slugify", false));
        assert!(selected("", "undo-one-step", true));
        assert!(selected("multi", "undo-one-step", true));
        assert!(!selected("multi", "rust-slugify", false));
        assert!(selected("rust-slugify", "rust-slugify", false));
        assert!(!selected("rust-slug", "rust-slugify", false));
        assert!(selected("cli-flag,undo-one-step", "undo-one-step", true));
        assert!(selected("cli-flag,undo-one-step", "cli-flag", false));
        assert!(!selected("cli-flag,undo-one-step", "js-dedupe", false));
        assert!(!selected("nope", "cli-flag", false));
    }

    #[test]
    fn tasks_come_from_the_repository() {
        let evals = crate::root().join("evals");
        let all = tasks(&evals, "").unwrap();
        let multi = |t: &Task| t.dir.parent() == Some(evals.join("multi").as_path());
        assert!(all.iter().any(|t| t.name == "rust-slugify" && !multi(t)));
        assert!(all.iter().any(|t| t.name == "goal-todo-cli" && multi(t)));
        assert!(!all.iter().any(|t| t.name == "multi"));
        let single: Vec<_> = all.iter().take_while(|t| !multi(t)).map(|t| &t.name).collect();
        let mut sorted = single.clone();
        sorted.sort();
        assert_eq!(single, sorted);
        assert!(tasks(&evals, "multi").unwrap().iter().all(multi));
        let two = tasks(&evals, "rust-slugify,undo-one-step").unwrap();
        assert_eq!(two.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["rust-slugify", "undo-one-step"]);
        assert!(tasks(&evals, "no-such-task").unwrap().is_empty());
    }

    #[test]
    fn independent_pass_with_fatal_endpoint_error_is_preserved_as_infra() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir().unwrap();
        let taskdir = root.join("task");
        std::fs::create_dir_all(taskdir.join("files")).unwrap();
        std::fs::write(taskdir.join("prompt.txt"), "repair the file").unwrap();
        std::fs::write(taskdir.join("check.sh"), "test \"$(cat result.txt)\" = correct").unwrap();
        let bin = root.join("fake-agent");
        std::fs::write(&bin,"#!/bin/sh\nprintf correct > result.txt\necho 'error: giving up after 3 attempts: HTTP 429 Too Many Requests' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = Config {
            root: root.clone(),
            bin,
            limit: Duration::from_secs(5),
            out: root.join("results.jsonl"),
            logs: root.join("logs"),
            transcripts: false,
        };
        let task = Task { name: "passing-patch-fatal".into(), dir: taskdir };
        let work = root.join("work");
        let home = root.join("home");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        File::create(&cfg.out).unwrap();
        score(&cfg, &task, &Run { model: "scripted", memory: "", rep: 1 }, &work, &home, &Mutex::new(())).unwrap();
        let row: Value = serde_json::from_str(std::fs::read_to_string(&cfg.out).unwrap().trim()).unwrap();
        assert_eq!(row["functional_pass"], true);
        assert_eq!(row["verdict"], "infra");
        assert!(!row["reason"].as_array().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_timeout_spent_waiting_on_the_provider_is_infra() {
        use std::os::unix::fs::PermissionsExt;
        for (waited, want) in [(30.0, "infra"), (0.0, "timeout")] {
            let root = temp_dir().unwrap();
            let taskdir = root.join("task");
            std::fs::create_dir_all(taskdir.join("files")).unwrap();
            std::fs::write(taskdir.join("prompt.txt"), "repair the file").unwrap();
            std::fs::write(taskdir.join("check.sh"), "false").unwrap();
            let bin = root.join("fake-agent");
            let script = format!(
                "#!/bin/sh\nprintf '{{\"retry_wait_seconds\": {waited}}}' > \"$RUSTY_BUDGET_LEDGER\"\nexec sleep 30\n"
            );
            std::fs::write(&bin, script).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let cfg = Config {
                root: root.clone(),
                bin,
                limit: Duration::from_secs(1),
                out: root.join("results.jsonl"),
                logs: root.join("logs"),
                transcripts: false,
            };
            let task = Task { name: "throttled".into(), dir: taskdir };
            let (work, home) = (root.join("work"), root.join("home"));
            std::fs::create_dir_all(&work).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            File::create(&cfg.out).unwrap();
            score(&cfg, &task, &Run { model: "scripted", memory: "", rep: 1 }, &work, &home, &Mutex::new(())).unwrap();
            let row: Value = serde_json::from_str(std::fs::read_to_string(&cfg.out).unwrap().trim()).unwrap();
            assert_eq!(row["verdict"], want, "{row}");
            assert_eq!(row["retry_wait_seconds"], waited, "{row}");
            if want == "infra" {
                assert!(row["reason"][0].as_str().unwrap().starts_with("provider throttling"), "{row}");
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn passing_patch_does_not_hide_unhealthy_execution() {
        let fatal = b"error: giving up after 4 attempts: HTTP 429 Too Many Requests";
        assert_eq!(verdict(&Exit::Done(false), true, b"", fatal), "infra");
        assert_eq!(verdict(&Exit::Done(false), true, b"", b"error: bad request"), "fail");
        assert_eq!(verdict(&Exit::Done(true), true, b"", b""), "pass");
        assert_eq!(verdict(&Exit::Done(false), true, b"", b"model budget exhausted: request limit"), "budget");
        assert_eq!(
            verdict(&Exit::Done(false), false, b"", b"HTTP 402 development evaluation budget exhausted"),
            "budget"
        );
        assert_eq!(verdict(&Exit::TimedOut, true, b"", fatal), "timeout");
    }

    #[test]
    fn endpoint_failures_are_infra() {
        for line in [
            "error: giving up after 5 attempts in 120s: HTTP 429 Too Many Requests",
            "HTTP 503 Service Unavailable",
            "request failed: error sending request for url (http://127.0.0.1:9/v1/chat/completions)",
            "error: the connection to the model closed unexpectedly",
            r#"API error: {"type":"overloaded_error","message":"Overloaded"}"#,
            r#"error: API error: {"type":"rate_limit_error"}"#,
            "`gpt-5` is served by OpenAI, but OPENAI_API_KEY is not set. Add it to your shell",
        ] {
            assert!(is_infra_line(line), "{line}");
            assert!(is_reason_line(line), "{line}");
        }
        for line in [
            "HTTP 404 Not Found",
            "HTTP 5xx",
            r#"API error: {"code":400,"message":"bad tool schema","type":"invalid_request_error"}"#,
            "assertion failed: the value is not set",
            "test result: FAILED. 3 passed; 1 failed",
        ] {
            assert!(!is_infra_line(line), "{line}");
        }
        // Quoted as the reason, though not infra on their own.
        assert!(is_reason_line("HTTP 401 Unauthorized"));
        assert!(is_reason_line(r#"API error: {"code":400}"#));
        assert!(!is_reason_line("HTTP 302 Found"));
    }

    #[test]
    fn turns_split_into_sessions() {
        let turns = "## env RUSTY_CONTEXT_TOKENS=26000\nfirst\nsecond\n\n## session\nthird\n## goal\nbuild it\n## env A=1\n## session\nlast";
        assert_eq!(
            sessions(turns),
            [
                Session {
                    env: vec!["RUSTY_CONTEXT_TOKENS=26000".into()],
                    lines: vec!["first".into(), "second".into()],
                    ..Session::default()
                },
                Session {
                    env: vec!["RUSTY_CONTEXT_TOKENS=26000".into()],
                    lines: vec!["third".into()],
                    ..Session::default()
                },
                Session {
                    env: vec!["RUSTY_CONTEXT_TOKENS=26000".into(), "A=1".into()],
                    goal: true,
                    lines: vec!["build it".into()],
                    ..Session::default()
                },
                Session {
                    env: vec!["RUSTY_CONTEXT_TOKENS=26000".into(), "A=1".into()],
                    lines: vec!["last".into()],
                    ..Session::default()
                },
            ]
        );
        assert!(sessions("").is_empty());
        assert!(sessions("## session\n\n## session\n").is_empty());
    }

    #[test]
    fn run_and_verify_attach_to_the_right_session() {
        let turns =
            "## run seed one\nfirst\n## run sed -i s/a/b/ f\n## goal\n## verify make check\nfix it\n## run tail";
        let s = sessions(turns);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].pre.clone(), s[0].lines.clone()), (vec!["seed one".to_string()], vec!["first".to_string()]));
        // A run between lines of one session starts the next one.
        assert_eq!(s[1].pre, ["sed -i s/a/b/ f"]);
        assert!(s[1].goal);
        assert_eq!(s[1].verify.as_deref(), Some("make check"));
        assert_eq!(s[1].lines, ["fix it"]);
        // Trailing runs still happen, before the check.
        assert_eq!(s[2], Session { pre: vec!["tail".into()], ..Session::default() });
        assert!(validate(&s).is_ok());
        assert!(validate(&sessions("## verify true\nnot a goal")).is_err());
        let split = sessions("one\n## run between\ntwo");
        assert_eq!(split.iter().map(|s| s.lines.clone()).collect::<Vec<_>>(), [["one"], ["two"]]);
    }

    #[test]
    fn memory_setting_labels_and_env() {
        let pinned = Run { model: "m/x", memory: "l1,l2", rep: 1 };
        assert_eq!(pinned.label(), "m/x · l1,l2");
        assert_eq!(pinned.env(), [("RUSTY_MODEL", "m/x"), ("RUSTY_MEMORY", "l1,l2")]);
        let default = Run { model: "m/x", memory: "", rep: 1 };
        assert_eq!(default.label(), "m/x");
        assert_eq!(default.env(), [("RUSTY_MODEL", "m/x")]);
    }

    #[test]
    fn setup_runs_hidden_commands_and_fails_loudly() {
        let root = temp_dir().unwrap();
        let (work, home) = (root.join("work"), root.join("home"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let cfg = Config {
            root: root.clone(),
            bin: root.join("rusty"),
            limit: Duration::from_secs(5),
            out: root.join("out.jsonl"),
            logs: root.join("logs"),
            transcripts: false,
        };
        let task = Task { name: "t".into(), dir: root.clone() };
        let run = Run { model: "m", memory: "v2", rep: 1 };
        let env = ["A=1".to_string()];
        setup(&cfg, &task, &work, &home, &run, &env, "printf '%s %s' \"$RUSTY_MEMORY\" \"$A\" > seen").unwrap();
        assert_eq!(std::fs::read_to_string(work.join("seen")).unwrap(), "v2 1");
        assert!(setup(&cfg, &task, &work, &home, &run, &env, "exit 3").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rows_match_python_json() {
        let r = row("t", "m/x", 2, "infra", 7, 1, 3, 100, 20, &["café \"x\"\t\u{1f600}".into()]);
        assert_eq!(
            r,
            r#"{"task": "t", "model": "m/x", "rep": 2, "verdict": "infra", "secs": 7, "sessions": 1, "requests": 3, "prompt": 100, "completion": 20, "reason": ["caf\u00e9 \"x\"\t\ud83d\ude00"]}"#
        );
        assert!(row("t", "m", 1, "pass", 0, 0, 0, 0, 0, &[]).ends_with(r#""reason": []}"#));
    }

    #[test]
    fn tail_and_head_match_coreutils() {
        assert_eq!(tail(b"a\nb\nc\n", 2), b"b\nc\n");
        assert_eq!(tail(b"a\nb\nc", 2), b"b\nc");
        assert_eq!(tail(b"a\nb\n", 5), b"a\nb\n");
        assert_eq!(tail(b"", 5), b"");
        assert_eq!(head(b"a\nb\nc\n", 2), b"a\nb\n");
        assert_eq!(head(b"a\nb", 5), b"a\nb");
    }

    #[test]
    fn check_reason_is_its_last_line() {
        assert_eq!(last_line("ok\n  FAILED: x\n\n").as_deref(), Some("  FAILED: x"));
        assert_eq!(last_line("one\r\ntwo\r\n").as_deref(), Some("two"));
        assert_eq!(last_line(" \n "), None);
        assert_eq!(py_lines("a\r\nb\rc\nd"), ["a", "b", "c", "d"]);
    }

    #[test]
    fn stamps_are_utc() {
        assert_eq!(stamp(UNIX_EPOCH), "19700101-000000");
        assert_eq!(stamp(UNIX_EPOCH + Duration::from_secs(1_791_313_200)), "20261006-190000");
        assert_eq!(stamp(UNIX_EPOCH + Duration::from_secs(951_782_400)), "20000229-000000");
    }

    #[test]
    fn reading_the_harness_records_is_a_leak() {
        let (work, home) = (temp_dir().unwrap(), temp_dir().unwrap());
        let call = |args: &str| {
            serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [
                {"function": {"name": "bash", "arguments": args}}]}]})
            .to_string()
        };
        let own = work.join("stats.py").display().to_string();
        let mine = serde_json::json!({ "command": format!("cat {own}") }).to_string();
        std::fs::write(home.join(trajectory(1)), call(&mine)).unwrap();
        assert_eq!(outside_reads(&home, &work), None, "its own working copy is fine");
        // A later session reads an earlier one's transcript from the home.
        // Session 2 wrote no trajectory; session 3 reads an earlier transcript.
        let theirs = home.join("stdout").display().to_string();
        let peek = serde_json::json!({ "command": format!("grep seed {theirs}") }).to_string();
        std::fs::write(home.join(trajectory(3)), call(&peek)).unwrap();
        let name = home.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(outside_reads(&home, &work), Some(name));
        std::fs::write(home.join(trajectory(3)), call(r#"{"command":"ls $RUSTY_HOME"}"#)).unwrap();
        assert_eq!(outside_reads(&home, &work).as_deref(), Some("$RUSTY_HOME"));
        assert_eq!(trajectories(&home).len(), 2);
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_memory_only_string_must_come_from_memory() {
        let (task_dir, home) = (temp_dir().unwrap(), temp_dir().unwrap());
        std::fs::write(task_dir.join("leak.txt"), "k7f3-seed\n").unwrap();
        let task = Task { name: "t".into(), dir: task_dir.clone() };
        let call = |id: &str, name: &str, args: &str| serde_json::json!({"role": "assistant", "tool_calls": [{"id": id, "function": {"name": name, "arguments": args}}]});
        let result = |id: &str, text: &str| serde_json::json!({"role": "tool", "tool_call_id": id, "content": text});
        let session = |n: usize, messages: Vec<Value>| {
            std::fs::write(home.join(trajectory(n)), serde_json::json!({ "messages": messages }).to_string()).unwrap()
        };
        // Session 1 may read it anywhere: that is where it is learned.
        session(1, vec![call("a", "read_file", "ONBOARDING.md"), result("a", "seed k7f3-seed")]);
        assert_eq!(seen_before_used(&home, &task), None);
        // Later, from the prompt's memory or recall, then echoed back: fine.
        session(2, vec![call("b", "bash", "STATS_SEED=k7f3-seed make"), result("b", "k7f3-seed ok")]);
        assert_eq!(seen_before_used(&home, &task), None);
        session(2, vec![call("c", "recall", "seed"), result("c", "- k7f3-seed (unverified)")]);
        assert_eq!(seen_before_used(&home, &task), None);
        // Found by searching the filesystem first: a leak.
        session(2, vec![call("d", "bash", "grep -r SEED /tmp"), result("d", "/tmp/notes.txt: k7f3-seed")]);
        assert_eq!(
            seen_before_used(&home, &task).as_deref(),
            Some("trajectory.2.json met a memory-only string in a bash result")
        );
        // Tasks without leak.txt have nothing to check.
        std::fs::remove_file(task_dir.join("leak.txt")).unwrap();
        assert_eq!(seen_before_used(&home, &task), None);
        let _ = std::fs::remove_dir_all(&task_dir);
        let _ = std::fs::remove_dir_all(&home);
    }
}
