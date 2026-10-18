mod activity;
mod agent;
mod anthropic;
mod archive;
mod aside;
mod backend;
mod budget;
mod codex;
mod config;
mod context;
mod display;
mod doctor;
mod execution;
mod footer;
mod inbox;
mod infra;
mod inspect;
mod llm;
mod markdown;
mod mcp;
mod permissions;
mod protocol;
mod session;
mod session_worker;
mod side;
mod signal;
mod skills;
mod slash;
mod snapshot;
mod sse;
mod swarm;
mod tips;
mod tools;
mod ui;
mod verification;

use anyhow::{anyhow, Result};
use clap::Parser;
use rustyline::error::ReadlineError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use agent::{Agent, GoalStatus};
use config::{AgentsMode, Settings};
use execution::ExecutionMode;
use llm::Client;
use permissions::{Mode, Policy, Verdict};

/// A coding agent for your terminal.
#[derive(Parser)]
#[command(name = "rusty", version)]
struct Cli {
    /// Tool location: local or daytona (reasoning and memory stay in this process)
    #[arg(long, env = "RUSTY_TOOLS")]
    tools: Option<String>,

    /// Private endpoint for the Daytona tool bridge; no model or memory required
    #[arg(long, hide = true)]
    tool_rpc: bool,
    /// Private native-app session engine (no terminal UI).
    #[arg(long, hide = true, conflicts_with = "inspect_worker")]
    session_worker: bool,
    /// Provider-free native-app repository inspection.
    #[arg(long, hide = true)]
    inspect_worker: bool,
    /// Carry private worker frames over stdin/stdout (for SSH).
    #[arg(long, hide = true)]
    protocol_stdio: bool,
    /// Run this prompt once and exit. Omit for interactive mode.
    prompt: Vec<String>,

    /// Any model id your endpoint serves (see --list-models)
    #[arg(short, long, env = "RUSTY_MODEL")]
    model: Option<String>,

    /// Execution mode: careful, standard or vibe (independent of permissions)
    #[arg(long, env = "RUSTY_MODE")]
    mode: Option<String>,

    /// Memory: off, recall (use saved lessons), learn (also tool context, file
    /// checks, credit from --verify), reflect (also a model review after checked
    /// goals) or deep (most aggressive). Benchmark runs should pass off
    #[arg(long, env = "RUSTY_MEMORY", default_value = "learn")]
    memory: String,

    /// Context window in tokens; otherwise asked of the endpoint, then the saved
    /// table, else 128000 is assumed
    #[arg(long, env = "RUSTY_CONTEXT_TOKENS", value_parser = clap::value_parser!(u64).range(1000..=context::MAX_WINDOW as u64))]
    context_tokens: Option<u64>,

    /// Compaction never waits past this many context tokens, whatever the
    /// window (overrides saved settings)
    #[arg(long, env = "RUSTY_HISTORY_CAP_TOKENS", value_parser = clap::value_parser!(u64).range(1000..=context::MAX_WINDOW as u64))]
    history_cap_tokens: Option<u64>,

    /// Compaction: aggressive, standard, passive or smart (overrides saved settings)
    #[arg(long, env = "RUSTY_COMPACTION")]
    compaction: Option<String>,

    /// Model that writes compaction summaries; the session's model when unset,
    /// and as the fallback if it fails (overrides saved settings)
    #[arg(long, env = "RUSTY_COMPACTION_MODEL")]
    compaction_model: Option<String>,

    /// Permission mode: read-only, ask, auto or yolo
    #[arg(short, long, env = "RUSTY_PERMISSIONS", default_value = "auto")]
    permissions: String,

    /// Shortcut for --permissions yolo
    #[arg(long)]
    yolo: bool,

    /// Delegation: off, sub, swarm or auto (overrides saved settings)
    #[arg(short, long, env = "RUSTY_AGENTS")]
    agents: Option<String>,

    /// Workers running at once (1..16); overrides saved swarm size
    #[arg(long, env = "RUSTY_SWARM_MAX", value_parser = clap::value_parser!(u8).range(1..=16))]
    swarm_max: Option<u8>,

    /// View: default, verbose or adhd (overrides saved settings)
    #[arg(long)]
    view: Option<String>,

    /// Resume a session here: the most recent one, or the one with this id
    #[arg(short = 'c', long = "continue", value_name = "ID", num_args = 0..=1, default_missing_value = "latest")]
    resume: Option<String>,

    /// List the sessions saved here, or those matching a search, then exit
    #[arg(long, value_name = "QUERY", num_args = 0..=1, default_missing_value = "")]
    sessions: Option<String>,

    /// Work on this goal until it's done, then exit
    #[arg(long)]
    goal: Option<String>,

    /// Fixed local acceptance command, run before the goal can close
    #[arg(long, requires = "goal", value_name = "COMMAND")]
    verify: Option<String>,

    /// Pin an acceptance file before work starts; repeat for each workspace-relative file
    #[arg(long, requires = "verify", value_name = "PATH")]
    verify_file: Vec<PathBuf>,

    /// Deadline for the fixed acceptance command (1..600 seconds)
    #[arg(long, requires = "verify", default_value = "120", value_parser = clap::value_parser!(u64).range(1..=600))]
    verify_timeout: u64,

    /// Maximum model HTTP attempts across this process; enables a bounded run (default 256)
    #[arg(long, env = "RUSTY_MAX_REQUESTS", value_parser = clap::value_parser!(u64).range(1..))]
    max_requests: Option<u64>,

    /// Conservative token admission cap; enables a bounded run (default 4000000), not a billing cap
    #[arg(long, env = "RUSTY_MAX_BUDGET_TOKENS", value_parser = clap::value_parser!(u64).range(1..))]
    max_budget_tokens: Option<u64>,

    /// Provider deadline; enables a bounded run (default 3600 seconds); tools keep their own deadlines
    #[arg(long, env = "RUSTY_BUDGET_SECS", value_parser = clap::value_parser!(u64).range(1..=86400))]
    budget_secs: Option<u64>,

    /// Print a JSON line with token and timing stats to stderr on exit
    #[arg(long)]
    stats: bool,

    /// Write the conversation (OpenAI message format) and totals to this JSON file on exit
    #[arg(long, value_name = "PATH")]
    trajectory: Option<PathBuf>,

    /// List the models your endpoints serve and exit
    #[arg(long)]
    list_models: bool,

    /// Check which providers are set up and reachable, then exit
    #[arg(long)]
    doctor: bool,

    /// Tools offered to the model: full, or shell (bash and the goal and
    /// loop controls only), for interface experiments
    #[arg(long, env = "RUSTY_TOOLSET", default_value = "full")]
    toolset: String,

    /// Print the values this build accepts, as JSON, and exit
    #[arg(long)]
    capabilities: bool,

    /// Start the project's MCP servers, print a JSON report and exit nonzero
    /// if a required server or tool is missing
    #[arg(long)]
    mcp_check: bool,

    /// Project directory (defaults to the current directory)
    #[arg(short = 'C', long)]
    dir: Option<PathBuf>,
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{} {e:#}", ui::err("error:"));
            std::process::exit(1);
        }
    }
}

struct WorkerStartupTrace {
    enabled: bool,
    started: Instant,
    previous: Instant,
}
impl WorkerStartupTrace {
    fn new(worker: bool) -> Self {
        let now = Instant::now();
        Self {
            enabled: worker && std::env::var("RUSTY_WORKER_TRACE").as_deref() == Ok("1"),
            started: now,
            previous: now,
        }
    }
    fn phase(&mut self, name: &str) {
        if self.enabled {
            let now = Instant::now();
            eprintln!(
                "worker startup {name}: phase_ms={:.3} total_ms={:.3}",
                now.duration_since(self.previous).as_secs_f64() * 1000.0,
                now.duration_since(self.started).as_secs_f64() * 1000.0
            );
            self.previous = now;
        }
    }
}

fn run() -> Result<i32> {
    let private_mode = std::env::args()
        .take_while(|arg| arg != "--")
        .any(|arg| matches!(arg.split('=').next().unwrap_or(""), "--session-worker" | "--inspect-worker"));
    let mut worker_transport = None;
    let mut trace = WorkerStartupTrace::new(private_mode);
    let result = run_inner(private_mode, &mut worker_transport, &mut trace);
    if let Err(error) = &result {
        if let Some(transport) = &mut worker_transport {
            let message = format!("{error:#}");
            let code =
                if message.contains("session is already open for writing") { "lease_busy" } else { "startup_failed" };
            // The app receives a typed failure before the ordinary stderr diagnostic.
            if let Err(report) = transport.startup_failed(code, &message) {
                eprintln!("worker startup failure could not be reported: {report:#}");
            }
        }
        trace.phase("failed");
    }
    result
}

fn run_inner(
    private_mode: bool,
    worker_transport: &mut Option<session_worker::Transport>,
    trace: &mut WorkerStartupTrace,
) -> Result<i32> {
    if private_mode {
        let cli = Cli::parse();
        *worker_transport = Some(session_worker::Transport::open(cli.protocol_stdio)?);
        trace.phase("transport_hello");
        if let Some(dir) = &cli.dir {
            std::env::set_current_dir(dir)?;
        }
        if cli.inspect_worker {
            let root = std::env::current_dir()?.canonicalize()?;
            trace.phase("inspect_root");
            return session_worker::inspect(worker_transport.take().expect("opened transport"), &root);
        }
        // This process is never an unattended harness. Scrub before dotenv and
        // again afterwards; worker authorization also rejects the bypass.
        session_worker::activate();
        std::env::remove_var("RUSTY_ALLOW_DESTRUCTIVE");
    }
    // The sandbox's tool endpoint must never load model keys from dotenv.
    if std::env::args().take_while(|arg| arg != "--").any(|arg| arg == "--tool-rpc") {
        let cli = Cli::parse();
        if let Some(dir) = &cli.dir {
            std::env::set_current_dir(dir)?;
        }
        signal::install();
        return backend::serve_rpc();
    }
    config::load_env();
    if session_worker::active() {
        std::env::remove_var("RUSTY_ALLOW_DESTRUCTIVE");
    }
    let mut cli = Cli::parse();
    if let Some(d) = cli.dir.as_ref().filter(|_| worker_transport.is_none()) {
        std::env::set_current_dir(d).map_err(|e| anyhow!("cannot enter {}: {e}", d.display()))?;
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    if cli.capabilities {
        println!("{}", capabilities());
        return Ok(0);
    }
    if cli.mcp_check {
        let (_, reports) = mcp::Mcp::connect(&cwd)?;
        let ok = !reports.iter().any(mcp::Report::blocks);
        println!("{}", serde_json::json!({ "ok": ok, "servers": reports }));
        return Ok(if ok { 0 } else { 1 });
    }

    trace.phase("environment_and_directory");
    let mut settings = Settings::load();
    trace.phase("settings");
    let backend = backend::Backend::connect(cli.tools.as_deref().unwrap_or(&settings.tools))?;
    trace.phase("backend");
    if let Some(query) = &cli.sessions {
        // Unquoted words all belong to the search: `rusty --sessions fix parser`.
        let query = std::iter::once(query).chain(&cli.prompt).map(String::as_str).collect::<Vec<_>>().join(" ");
        let project = project_dir(&PathBuf::from(backend.identity(&cwd)));
        let sessions = project.join("sessions");
        let found =
            if query.trim().is_empty() { session::list(&sessions)? } else { session::search(&sessions, &query)? };
        if found.is_empty() && !query.trim().is_empty() {
            println!("  {}", ui::dim(&format!("no session matches `{}`", query.trim())));
        } else {
            print!("{}", session::render(&found, rusty::unix_now()));
        }
        return Ok(0);
    }
    ui::set_theme(&settings.theme);
    ui::set_font(&settings.font);
    display::set_view(&settings.view);
    if let Some(v) = &cli.view {
        if !display::set_view(v) {
            return Err(anyhow!("unknown view `{v}` (default, verbose, adhd)"));
        }
    }
    if let Some(a) = &cli.agents {
        settings.agents.mode = AgentsMode::parse(a).ok_or_else(|| anyhow!("unknown agents mode `{a}`"))?;
    }
    if let Some(max) = cli.swarm_max {
        settings.agents.swarm_max = max as usize;
    }

    // An explicit mode always wins; without one rusty picks per request.
    let chosen = match cli.mode.as_deref() {
        Some("auto") => None,
        Some(mode) => Some(
            ExecutionMode::parse(mode)
                .ok_or_else(|| anyhow!("unknown execution mode `{mode}` (auto, careful, standard, vibe)"))?,
        ),
        None => settings.execution_mode,
    };
    let model =
        cli.model.clone().or_else(|| chosen.and_then(ExecutionMode::model)).unwrap_or_else(config::default_model);
    if cli.doctor {
        let client = Client::from_env().ok();
        let rows = doctor::rows(client.as_ref(), &model, true);
        let (report, ok) = doctor::render(&rows, client.as_ref(), &model);
        println!("{report}");
        return Ok(if ok { 0 } else { 1 });
    }
    let mut client = Client::from_env()?;
    let bounded = cli.max_requests.is_some() || cli.max_budget_tokens.is_some() || cli.budget_secs.is_some();
    let ledger = std::env::var_os("RUSTY_BUDGET_LEDGER").filter(|p| !p.is_empty()).map(PathBuf::from);
    if bounded || ledger.is_some() {
        let defaults = budget::Limits::default();
        let limits = bounded.then(|| budget::Limits {
            requests: cli.max_requests.unwrap_or(defaults.requests),
            tokens: cli.max_budget_tokens.unwrap_or(defaults.tokens),
            seconds: cli.budget_secs.unwrap_or(defaults.seconds),
        });
        client = client.with_budget(limits, ledger.as_deref())?;
    }
    let client = Arc::new(client);
    trace.phase("client_configuration");
    if cli.list_models {
        for m in client.list_models()? {
            println!("{m}");
        }
        return Ok(0);
    }

    // Native launchers own terminal signal disposition; cancellation travels
    // through the private protocol instead of the CLI's double-Ctrl-C handler.
    if !session_worker::active() {
        signal::install();
        signal::save_terminal();
    }
    let mode = if cli.yolo {
        Mode::Yolo
    } else {
        Mode::parse(&cli.permissions).ok_or_else(|| anyhow!("unknown permission mode `{}`", cli.permissions))?
    };
    let identity = PathBuf::from(backend.identity(&cwd));
    let pdir = project_dir(&identity);
    let global = config::config_dir().unwrap_or_default();
    let policy = Policy::load(pdir.join("permissions.json"), mode);

    // Each startup probe waits on something outside rusty (git, kubectl,
    // the memory daemon, the endpoint's model list) and none needs another,
    // so they run side by side; each result is still used where it was.
    use rusty::advisor::Level;
    let memory_level = Level::parse(&cli.memory);
    let advisor_probe = memory_level
        .filter(|level| level.advises() && !session_worker::active())
        .map(|level| std::thread::spawn(move || rusty::advisor::Hooks::connect(&global, &identity, level)));
    let window_probe = (cli.context_tokens.is_none() && !session_worker::active()).then(|| {
        let (client, model) = (client.clone(), model.clone());
        std::thread::spawn(move || client.context_window(&model))
    });
    let local = backend.name() == "local";
    let target_probe = local.then(|| probe(&cwd, infra::Target::detect));
    let snapshot_probe = local.then(|| probe(&cwd, snapshot::take));
    let blobs_probe = local.then(|| probe(&cwd, snapshot::blobs));

    let mut agent = Agent::new(client.clone(), model, cwd.clone(), policy);
    if let Some(target) = target_probe {
        agent.infra.target = joined(target);
    }
    agent.set_backend(backend)?;
    trace.phase("agent_and_backend");
    if agent.backend.name() == "local" {
        agent.snapshot = snapshot_probe.map(joined).unwrap_or_default();
        agent.start_blobs = blobs_probe.and_then(joined);
        trace.phase("workspace_snapshot");
        agent.mcp = mcp::Mcp::load(&cwd)?;
        trace.phase("mcp");
        if !agent.mcp.is_empty() {
            println!("{} {}", ui::dim("◇ mcp"), ui::dim(&agent.mcp.summary()));
        }
    }
    agent.memory_level =
        memory_level.ok_or_else(|| anyhow!("unknown memory level `{}` ({})", cli.memory, Level::NAMES))?;
    // Session workers connect the advisor lazily when the first run starts.
    if let Some(advisor) = advisor_probe {
        match joined(advisor) {
            Ok(h) => agent.advisor = Some(h),
            Err(e) => {
                eprintln!("memory advisor unavailable ({e:#}); continuing with memory off");
                agent.memory_level = Level::Off;
            }
        }
    }
    trace.phase("memory_configuration");
    agent.meter = match cli.context_tokens {
        Some(n) => context::Meter::new(n as usize, "flag"),
        // An app can pin a context size; otherwise use the local table/default.
        // A slow /models endpoint must never delay a worker's ready handshake.
        None if session_worker::active() => match settings.context_windows.get(&agent.model) {
            Some(&n) => context::Meter::new(n, "table"),
            None => context::Meter::default(),
        },
        None => match window_probe.and_then(joined) {
            Some(n) => context::Meter::new(n, "endpoint"),
            None => match settings.context_windows.get(&agent.model) {
                Some(&n) => context::Meter::new(n, "table"),
                None => {
                    println!(
                        "{}",
                        ui::dim(&format!(
                            "◇ context window assumed {} tokens; pass --context-tokens to set it",
                            context::ASSUMED_WINDOW
                        ))
                    );
                    context::Meter::default()
                }
            },
        },
    };
    trace.phase("context_configuration");
    // Unchosen, compaction follows the window (aggressive below 48k), and so
    // does the history cap, whatever the mode (128k above a 200k window).
    let chosen_compaction = cli.compaction.clone().or_else(|| settings.compaction.clone());
    agent.compaction = match &chosen_compaction {
        Some(c) => context::Compaction::parse(c)
            .ok_or_else(|| anyhow!("unknown compaction `{c}` ({})", context::Compaction::NAMES))?,
        None => context::Compaction::for_window(agent.meter.window),
    };
    agent.history_cap = cli
        .history_cap_tokens
        .map(|n| n as usize)
        .or(settings.history_cap_tokens.map(|n| n.min(context::MAX_WINDOW)))
        .or_else(|| context::default_history_cap(agent.meter.window));
    agent.compaction_model = cli
        .compaction_model
        .clone()
        .or_else(|| settings.compaction_model.clone())
        .filter(|m| !m.trim().is_empty() && *m != agent.model);
    agent.shell_only = match cli.toolset.as_str() {
        "full" => false,
        "shell" => true,
        other => return Err(anyhow!("unknown toolset `{other}` (full, shell)")),
    };
    agent.agents = settings.agents.clone();
    agent.tips = settings.tips;
    agent.model_override = cli.model.clone();
    agent.delegation_override =
        cli.agents.is_some() || settings.delegation_override || settings.agents.mode != AgentsMode::Off;
    agent.set_execution_mode(chosen.unwrap_or(ExecutionMode::Standard));
    agent.mode_fixed = chosen.is_some();
    agent.infra.dir = Some(pdir.clone());
    // A production target makes auto start careful and keep picking it; an
    // explicit mode still wins. Nothing is saved.
    if agent.infra.target.production && !agent.mode_fixed {
        agent.set_execution_mode(ExecutionMode::Careful);
        agent.mode_reason = "the target looks like production".into();
        println!(
            "{} {}",
            ui::warn("◇ careful"),
            ui::dim(&format!("target looks like production: {}", agent.infra.target.summary()))
        );
    }

    // Checked once the mode is known, since the reply reserve depends on it.
    if let Some(m) = agent.compaction_model.clone() {
        client.endpoint_for(&m)?;
        agent.compaction_window = if session_worker::active() {
            settings.context_windows.get(&m).copied()
        } else {
            client.context_window(&m).or_else(|| settings.context_windows.get(&m).copied())
        };
        let need = agent.largest_summary_request();
        match agent.compaction_window {
            Some(w) if w < need => {
                return Err(anyhow!(
                    "`{m}` has a {w}-token context window, but a summary request here can reach {need} tokens \
                     (the summarise threshold plus the reply reserve); pick a larger summariser or lower \
                     --history-cap-tokens"
                ));
            }
            Some(_) => {}
            None => println!(
                "{}",
                ui::dim(&format!(
                    "◇ {m}'s context window is unknown; a summary it can't fit is written by {}",
                    agent.model
                ))
            ),
        }
    }

    trace.phase("execution_configuration");
    let sessions = pdir.join("sessions");
    if let Some(key) = cli.resume.clone() {
        // The app already owns the exact ID returned by session.ready. An idle
        // transcript deliberately stays out of CLI search, but must still be
        // recoverable after a crash before the first turn was recorded.
        let exact_worker = session_worker::active()
            && key.len() == 32
            && key.bytes().all(|b| b.is_ascii_hexdigit())
            && session::path(&sessions, &key).is_file();
        if exact_worker {
            agent.load_session(&session::path(&sessions, &key))?;
        } else {
            // Try IDs before treating an optional -c value as prompt text. This
            // keeps legacy timestamp-hyphen IDs resumable while retaining `-c prompt`.
            let found = if key == "latest" { Vec::new() } else { session::find(&sessions, &key)? };
            let (entry, key) = if key == "latest" {
                (session::latest(&sessions)?, key)
            } else if let [entry] = found.as_slice() {
                (Some(entry.clone()), key)
            } else if found.len() > 1 {
                let ids: Vec<&str> = found.iter().map(|e| e.id.as_str()).collect();
                return Err(anyhow!("`{key}` could be {}; give more of the id", ids.join(", ")));
            } else {
                let hex = key.bytes().all(|byte| byte.is_ascii_hexdigit());
                let legacy = key.len() >= 8
                    && key.as_bytes()[0].is_ascii_digit()
                    && key.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
                if hex || legacy {
                    return Err(anyhow!("no session {key} here; `rusty --sessions` lists them"));
                }
                cli.prompt.insert(0, key);
                (session::latest(&sessions)?, "latest".to_string())
            };
            match entry {
                Some(entry) => resume_session(&mut agent, &sessions, &entry)?,
                None if key == "latest" => println!("{}", ui::dim("no previous session here; starting fresh")),
                None => return Err(anyhow!("no session {key} here; `rusty --sessions` lists them")),
            }
        }
    }
    if agent.session_path.is_none() {
        agent.new_session(&sessions)?;
    }
    agent.trajectory_path = cli.trajectory.clone();

    trace.phase("session_lease_and_history");
    if let Some(transport) = worker_transport.take() {
        trace.phase("ready_to_serve");
        return session_worker::serve(transport, &mut agent);
    }

    // Headless modes.
    let started = Instant::now();
    let headless = cli.goal.is_some() || !cli.prompt.is_empty();
    if headless {
        let result = match &cli.goal {
            Some(goal) => {
                if let Some(command) = &cli.verify {
                    if agent.backend.name() != "local" {
                        return Err(anyhow!(
                            "--verify currently requires local tools; remote acceptance is not qualified"
                        ));
                    }
                    agent.goal_check = Some(
                        verification::CheckSpec::new(command, cli.verify_timeout)?.pin_files(&cwd, &cli.verify_file)?,
                    );
                }
                agent.run_goal(Some(goal))
            }
            None => {
                let input = cli.prompt.join(" ");
                let input = expand_skill(&cwd, &input).unwrap_or(input);
                agent.run_turn(&input).map(|_| ())
            }
        };
        agent.save_session()?;
        if let Some(path) = &agent.session_path {
            println!("{}", ui::dim(&format!("resume this session with `rusty -c {}`", session::id_of(path))));
        }
        print_changes(&agent);
        if cli.stats {
            print_stats(&agent, started);
        }
        agent.write_trajectory()?;
        result?;
        return Ok(if signal::interrupted() {
            130
        } else if agent.goal_end == Some("stalled")
            || cli.goal.is_some() && !agent.goal.as_ref().is_some_and(|g| matches!(g.status, GoalStatus::Done(_)))
        {
            2
        } else {
            0
        });
    }

    repl(&mut agent, &client, &cwd, &mut settings)?;
    if cli.stats {
        print_stats(&agent, started);
    }
    agent.write_trajectory()?;
    Ok(0)
}

/// Runs a startup probe of the working directory on its own thread.
fn probe<T: Send + 'static>(cwd: &Path, run: fn(&Path) -> T) -> std::thread::JoinHandle<T> {
    let cwd = cwd.to_owned();
    std::thread::spawn(move || run(&cwd))
}

/// A probe's result; a probe that panicked panics here, as it did inline.
fn joined<T>(probe: std::thread::JoinHandle<T>) -> T {
    probe.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Bumped when a harness-facing flag, exit code or `--stats` key changes
/// meaning (docs/VERIFICATION.md, "Harness contract").
const CONTRACT: u32 = 3;

/// What this build accepts, so a harness can refuse an unsupported setting
/// before any model call. Each list is filtered through the parser that
/// reads the flag, so it can't claim a value the flag would reject.
fn capabilities() -> serde_json::Value {
    let accepted = |values: &[&'static str], ok: &dyn Fn(&str) -> bool| -> Vec<&'static str> {
        values.iter().copied().filter(|v| ok(v)).collect()
    };
    let memory: Vec<&str> = rusty::advisor::Level::NAMES.split(", ").collect();
    let compaction: Vec<&str> = context::Compaction::NAMES.split(", ").collect();
    let mut modes = vec!["auto"];
    modes.extend(accepted(&["careful", "standard", "vibe"], &|v| ExecutionMode::parse(v).is_some()));
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "contract": CONTRACT,
        "memory": accepted(&memory, &|v| rusty::advisor::Level::parse(v).is_some()),
        "compaction": accepted(&compaction, &|v| context::Compaction::parse(v).is_some()),
        "context": {
            "tokens_flag": true,
            "history_cap_flag": true,
            "compaction_model_flag": true,
            "window_sources": ["flag", "endpoint", "table", "assumed", "overflow"]
        },
        "mode": modes,
        "agents": accepted(&["off", "sub", "swarm", "auto"], &|v| AgentsMode::parse(v).is_some()),
        "permissions": accepted(&["read-only", "ask", "auto", "yolo"], &|v| Mode::parse(v).is_some()),
        "tools": ["local", "daytona"],
        "toolset": ["full", "shell"],
        "verify": {"supported": true, "timeout_secs": [1, 600], "files_flag": true},
        "mcp": {"transports": ["stdio"], "features": ["tools"], "check": true},
        "budget": ["max-requests", "max-budget-tokens", "budget-secs", "ledger"],
        // Records a harness may read from --stats and the trajectory.
        "stats": ["model_budget", "claims", "safety", "goal", "goal_end", "goal_outcome", "context", "compactions"],
    })
}

/// One JSON line on stderr for scripts and evals.
fn print_stats(agent: &Agent, started: Instant) {
    let t = &agent.totals;
    let goal = agent.goal.as_ref().map(|g| match &g.status {
        GoalStatus::Done(_) => "done",
        GoalStatus::Blocked(_) => "blocked",
        GoalStatus::Active => "open",
    });
    eprintln!(
        "{}",
        serde_json::json!({
            "execution_mode": agent.execution_mode.name(),
            "mode_reason": if agent.mode_fixed { "chosen by the user" } else { agent.mode_reason.as_str() },
            "requests": t.requests, "prompt": t.prompt, "completion": t.completion,
            "model_budget": agent.budget_snapshot(),
            "secs": started.elapsed().as_secs_f32(), "interrupted": signal::interrupted(), "goal": goal,
            "goal_end": agent.goal_end,
            "goal_outcome": agent.goal_outcome(),
            "claims": agent.completion,
            "safety": agent.safety,
            "memory_mode": agent.memory_level.name(),
            "compaction": agent.compaction.name(),
            "context": agent.context_record(),
            "compactions": agent.compactions,
            "tools_location": agent.backend.summary(),
            "memory": agent.advisor.as_ref().map(|h| &h.metrics),
        })
    );
}

/// `/name args` → the skill's prompt, if `name` is a skill.
fn expand_skill(cwd: &Path, input: &str) -> Option<String> {
    let rest = input.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    skills::find(cwd, name).map(|s| skills::render(&s, args))
}

fn repl(agent: &mut Agent, client: &Arc<Client>, cwd: &Path, settings: &mut Settings) -> Result<()> {
    banner(agent, client, cwd);
    // Typing / opens the command menu under the prompt (see slash.rs).
    let menu = slash::Shared::default();
    let skill_list: slash::Skills = skills::all(cwd).into_iter().map(|s| (s.name, s.description)).collect();
    // List completion fills in the one row Tab offers, cursor at the end.
    let config = rustyline::Config::builder().completion_type(rustyline::CompletionType::List).build();
    let mut rl: rustyline::Editor<slash::SlashHelper, rustyline::history::DefaultHistory> =
        rustyline::Editor::with_config(config)?;
    rl.set_helper(Some(slash::SlashHelper { menu: menu.clone(), skills: skill_list.clone() }));
    // Keep the advertised newline key distinct from Enter's submit binding.
    rl.bind_sequence(rustyline::KeyEvent::ctrl('J'), rustyline::Cmd::Newline);
    {
        use rustyline::{EventHandler, KeyCode, KeyEvent, Modifiers};
        let on = |h: Box<dyn rustyline::ConditionalEventHandler>| EventHandler::Conditional(h);
        rl.bind_sequence(
            KeyEvent(KeyCode::Up, Modifiers::NONE),
            on(Box::new(slash::Move { menu: menu.clone(), down: false })),
        );
        rl.bind_sequence(
            KeyEvent(KeyCode::Down, Modifiers::NONE),
            on(Box::new(slash::Move { menu: menu.clone(), down: true })),
        );
        rl.bind_sequence(
            KeyEvent(KeyCode::Enter, Modifiers::NONE),
            on(Box::new(slash::Submit { menu: menu.clone(), skills: skill_list.clone() })),
        );
    }
    let history_file = config::config_dir().map(|d| d.join("history"));
    if let Some(h) = &history_file {
        let _ = rl.load_history(h);
    }
    let prompt = "› ";
    let mut armed_quit = false;

    loop {
        let parent = agent.side_parent()?;
        side::poll(&parent);
        if side::running(&agent.side_book) {
            side::wait(&parent);
        }
        agent.accept_side_summaries()?;
        // Anything typed since the turn stopped reading keys, including an
        // Enter that would otherwise reach the line editor as a bare newline
        // and sit there unsent, joins the queue first.
        signal::poll_keys();
        // Queued and freshly typed input go through the same slash router.
        let mut input = if let Some(queued) = signal::take_queued() {
            println!("{} {}", ui::user("›"), ui::user(&queued));
            println!("{}", ui::dim("  (typed while rusty was working)"));
            queued
        } else {
            match rl.readline(prompt) {
                Ok(l) => {
                    armed_quit = false;
                    l
                }
                Err(ReadlineError::Interrupted) => {
                    if armed_quit {
                        break;
                    }
                    armed_quit = true;
                    println!("{}", ui::dim("  (ctrl-c again to quit)"));
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e.into()),
            }
        };
        // A trailing backslash continues the message on the next line.
        while input.ends_with('\\') {
            input.pop();
            input.push('\n');
            menu.lock().map(|mut m| m.paused = true).ok();
            let more = rl.readline("  ");
            menu.lock().map(|mut m| m.paused = false).ok();
            match more {
                Ok(l) => input.push_str(&l),
                Err(_) => break,
            }
        }
        // A partly typed command runs what the menu had highlighted, matched
        // against the line as typed (a trailing space opens a value menu).
        if input.starts_with('/') {
            let typed = input.trim_end().to_string();
            let resolved = match menu.lock() {
                Ok(mut m) => {
                    let r = slash::resolve(&input, Some((&m.line, &m.rows, m.selected)), &skill_list);
                    m.line.clear();
                    r
                }
                Err(_) => slash::resolve(&input, None, &skill_list),
            };
            if resolved.trim_end() != typed {
                println!("{}", ui::dim(&format!("  → {}", resolved.trim_end())));
            }
            input = resolved;
        }
        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(rusty::privacy::redact(&input).0);
        signal::restore_terminal();
        signal::reset();

        let result = if input.starts_with('/') {
            match command(agent, client, cwd, settings, &input) {
                Ok(true) => break,
                Ok(false) => Ok(()),
                Err(e) => Err(e),
            }
        } else if side::focused(&agent.side_book) {
            (|| {
                let parent = agent.side_parent()?;
                side::submit(&parent, &input)?;
                side::wait(&parent);
                agent.collect_side_usage();
                Ok(())
            })()
        } else {
            agent.run_turn(&input).map(|_| ())
        };
        if let Err(e) = result {
            println!("{} {e:#}\n", ui::err("error:"));
        }
        agent.save_session()?;
    }
    side::shutdown(&agent.side_book);
    if let Some(h) = &history_file {
        if let Some(dir) = h.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if rusty::privacy::private_file(h, true).is_ok() {
            let _ = rl.save_history(h);
        }
    }
    let kept = agent.end_session()?;
    print_changes(agent);
    let id = agent.session_path.as_deref().filter(|_| kept).map(session::id_of);
    println!("{}", ui::dim(&goodbye(id.as_deref(), ui::width())));
    Ok(())
}

/// The REPL's last line, with how to pick the conversation up again when
/// there is one: on the same line if it fits, else on a line of its own.
fn goodbye(id: Option<&str>, width: usize) -> String {
    let bye = "  ◌ link closed. go ship something.";
    let Some(id) = id else { return bye.to_string() };
    let hint = format!("resume with `rusty -c {id}`");
    if bye.chars().count() + 1 + hint.chars().count() <= width {
        format!("{bye} {hint}")
    } else {
        format!("{bye}\n  {hint}")
    }
}

/// What this session changed in infrastructure, if anything, with the
/// rollback paths and verification results. Printed at the end so the
/// person who reviews the session doesn't have to reconstruct it.
fn print_changes(agent: &Agent) {
    let rec = agent.infra.change_record();
    if !rec.is_empty() {
        println!("{} {}", ui::accent("▤"), rec.trim_end());
        println!();
    }
}

/// Handles a slash command. Returns true to quit.
fn command(agent: &mut Agent, client: &Arc<Client>, cwd: &Path, settings: &mut Settings, input: &str) -> Result<bool> {
    let (cmd, rest) = match input.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (input, ""),
    };
    if side::focused(&agent.side_book)
        && !matches!(cmd, "/side" | "/back" | "/send" | "/help" | "/?" | "/exit" | "/quit" | "/q")
    {
        anyhow::bail!("use /back before changing main-chat settings or asking /btw");
    }
    match cmd {
        "/side" | "/back" | "/send" => {
            let parent = agent.side_parent()?;
            side::command(&parent, input)?;
            if cmd == "/send" {
                side::wait(&parent);
                agent.collect_side_usage();
                agent.accept_side_summaries()?;
            }
        }
        "/exit" | "/quit" | "/q" => return Ok(true),
        "/help" | "/?" => print_help(cwd),
        "/clear" | "/new" => {
            agent.end_session()?;
            agent.clear()?;
            if let Some(p) = &agent.session_path {
                if let Some(dir) = p.parent().map(Path::to_path_buf) {
                    agent.new_session(&dir)?;
                }
            }
            println!("{}", ui::dim("  fresh context. memories kept."));
        }
        "/resume" | "/sessions" => {
            let Some(dir) = agent.session_path.as_ref().and_then(|path| path.parent().map(Path::to_path_buf)) else {
                return Ok(false);
            };
            let now = rusty::unix_now();
            if rest.is_empty() {
                let entries = session::list(&dir)?;
                print!("{}", session::render(&entries, now));
                println!("{}", ui::dim("  /resume <id> picks one up · /resume <words> searches"));
                return Ok(false);
            }
            match session::find(&dir, rest)?.as_slice() {
                [entry] => {
                    agent.end_session()?;
                    resume_session(agent, &dir, entry)?;
                }
                [] => {
                    let hits = session::search(&dir, rest)?;
                    if hits.is_empty() {
                        println!("{}", ui::dim(&format!("  no session matches `{rest}`")));
                    } else {
                        print!("{}", session::render(&hits, now));
                    }
                }
                shared => {
                    print!("{}", session::render(shared, now));
                    println!("{}", ui::dim(&format!("  `{rest}` could be any of these; give more of the id")));
                }
            }
        }
        "/btw" => agent.ask_aside(rest)?,
        "/model" => {
            if !rest.is_empty() {
                agent.model = rest.to_string();
                agent.model_override = Some(rest.to_string());
            }
            match client.endpoint_for(&agent.model) {
                Ok(ep) => {
                    println!("  model {} {}", ui::info(&agent.model), ui::dim(&format!("· {}", ep.provider.name())))
                }
                Err(_) => {
                    let p = config::Provider::for_model(&agent.model);
                    let hint = match p.key_vars().into_iter().find(|v| !v.starts_with("RUSTY_")) {
                        Some(var) => format!("needs {var}"),
                        None => p.setup_hint(),
                    };
                    println!("  model {} {}", ui::info(&agent.model), ui::warn(&format!("· {} {hint}", p.name())))
                }
            }
        }
        "/mode" => {
            if rest == "auto" {
                agent.mode_fixed = false;
                settings.execution_mode = None;
                settings.save();
            } else if !rest.is_empty() {
                let mode = ExecutionMode::parse(rest)
                    .ok_or_else(|| anyhow!("unknown execution mode `{rest}` (auto, careful, standard, vibe)"))?;
                agent.set_execution_mode(mode);
                agent.mode_fixed = true;
                settings.execution_mode = Some(mode);
                settings.save();
            }
            print_mode(agent);
        }
        "/tools" => {
            if !rest.is_empty() {
                if !matches!(rest, "local" | "daytona") {
                    return Err(anyhow!("tool placement must be local or daytona"));
                }
                settings.tools = rest.into();
                settings.save();
                println!("  default tools {rest} · takes effect next session; --tools overrides it");
            }
            println!("  current tools {} · reasoning and memory in this process", agent.backend.summary());
        }
        "/doctor" | "/providers" => {
            let rows = doctor::rows(Some(client), &agent.model, true);
            println!("{}", doctor::render(&rows, Some(client), &agent.model).0);
        }
        "/models" => {
            for m in client.list_models()?.into_iter().filter(|m| m.contains(rest)) {
                println!("  {m}");
            }
        }
        "/compact" if rest == "settings" || rest.starts_with("settings ") => match rest["settings".len()..].trim() {
            "" => {
                println!("  compaction · {}", agent.compaction.describe());
                println!("  {}", ui::dim(&format!("/compact settings <mode> · {}", context::Compaction::NAMES)));
            }
            name => match context::Compaction::parse(name) {
                Some(mode) => {
                    agent.compaction = mode;
                    settings.compaction = Some(mode.name().into());
                    settings.save();
                    println!("  compaction · {} (saved)", mode.describe());
                }
                None => {
                    println!("  {}", ui::err(&format!("unknown compaction `{name}` ({})", context::Compaction::NAMES)))
                }
            },
        },
        "/compact" => agent.compact(Some(rest).filter(|r| !r.is_empty()))?,
        "/context" => println!("{}", agent.context_report()),
        "/tokens" | "/usage" | "/cost" => print_tokens(agent),
        "/view" | "/adhd" | "/verbose" => {
            let target = match cmd {
                "/adhd" => "adhd",
                "/verbose" => "verbose",
                _ => rest,
            };
            if target.is_empty() {
                println!("  view {} {}", ui::bold(display::view_name()), ui::dim("(default · verbose · adhd)"));
            } else if display::set_view(target) {
                settings.view = display::view_name().to_string();
                settings.save();
                println!("  view {}", ui::bold(display::view_name()));
            } else {
                println!("  unknown view. default · verbose · adhd");
            }
        }
        "/theme" => {
            if rest.is_empty() || !ui::set_theme(rest) {
                let names: Vec<String> = ui::THEMES
                    .iter()
                    .map(|t| if t.name == ui::theme().name { ui::bold(t.name) } else { ui::dim(t.name) })
                    .collect();
                println!("  theme {}", names.join(" · "));
            } else {
                settings.theme = rest.to_string();
                settings.save();
                println!("  {} {}", ui::gradient("▓▒░ theme"), ui::bold(rest));
            }
        }
        "/font" => {
            if rest.is_empty() || !ui::set_font(rest) {
                println!("  font {} {}", ui::bold(ui::font()), ui::dim(&format!("({})", ui::FONTS.join(" · "))));
            } else {
                settings.font = rest.to_string();
                settings.save();
                banner(agent, client, cwd);
            }
        }
        "/agents" | "/subagents" if rest == "default" => {
            // Hand delegation back to the execution mode.
            agent.delegation_override = false;
            settings.delegation_override = false;
            agent.set_execution_mode(agent.execution_mode);
            settings.agents.mode = AgentsMode::Off;
            settings.save();
            agents_cmd(agent, "");
        }
        "/agents" | "/subagents" | "/swarm" => {
            if cmd == "/swarm" {
                swarm_cmd(agent, rest);
            } else {
                agents_cmd(agent, rest);
            }
            // Any change makes delegation the user's choice, kept across sessions.
            if !rest.is_empty() {
                agent.delegation_override = true;
                settings.delegation_override = true;
                settings.agents = agent.agents.clone();
                settings.save();
            }
        }
        "/tips" => {
            agent.tips = match rest {
                "off" => false,
                "on" => true,
                _ => !agent.tips,
            };
            settings.tips = agent.tips;
            settings.save();
            println!("  tips {}", on_off(agent.tips));
        }
        "/target" => {
            agent.infra.target = if agent.backend.name() == "local" {
                infra::Target::detect_with_identity(cwd)
            } else {
                serde_json::from_value(agent.backend.describe(true)?["target"].clone())?
            };
            let line = if agent.infra.target.is_empty() { ui::dim("none detected") } else { target_line(agent) };
            println!("  target {line}");
        }
        "/changes" => {
            let rec = agent.infra.change_record();
            let none = ui::dim("  nothing changed in infrastructure this session");
            println!("{}", if rec.is_empty() { none } else { rec });
        }
        "/audit" => {
            let n = rest.parse().unwrap_or(20);
            let tail = agent.infra.audit_tail(n);
            if tail.is_empty() {
                println!("{}", ui::dim("  no infrastructure commands logged yet"));
            } else {
                print!("{tail}");
                if let Some(p) = agent.infra.audit_path() {
                    println!("  {}", ui::dim(&p.display().to_string()));
                }
            }
        }
        "/settings" => print_settings(agent),
        "/permissions" | "/perms" => permissions_cmd(agent, rest),
        "/yolo" => {
            agent.policy.mode = Mode::Yolo;
            println!("  permissions {}", ui::warn("yolo · everything runs except deny rules and destructive commands"));
        }
        "/plan" => {
            if agent.plan.is_empty() {
                println!("{}", ui::dim("  no plan yet"));
            } else {
                let items = serde_json::to_value(&agent.plan).unwrap_or_default();
                println!("{}", display::render_plan(&items));
            }
        }
        "/goal" => match rest {
            "verify off" => {
                agent.goal_check = None;
                println!("  future goals have no fixed check; the current goal is unchanged");
            }
            value if value.starts_with("verify ") => {
                let check = verification::CheckSpec::new(&value[7..], 120)?;
                agent.goal_check = Some(check);
                println!("  fixed check set for future goals; /goal pin <path> pins an acceptance file");
            }
            value if value.starts_with("pin ") => {
                let check = agent.goal_check.clone().ok_or_else(|| anyhow!("set /goal verify <command> first"))?;
                agent.goal_check = Some(check.pin_files(cwd, &[PathBuf::from(&value[4..])])?);
                println!("  acceptance file pinned for future goals");
            }
            "" | "status" => match &agent.goal {
                Some(g) => {
                    let status = match &g.status {
                        GoalStatus::Active => "active".to_string(),
                        GoalStatus::Done(e) => {
                            format!("{}: {e}", agent.goal_outcome().unwrap_or("completed_unverified"))
                        }
                        GoalStatus::Blocked(e) => format!("blocked: {e}"),
                    };
                    println!(
                        "  goal {}\n  {}",
                        ui::bold(&g.objective),
                        ui::dim(&format!("{status} · {} turns", g.turns))
                    );
                }
                None => println!("{}", ui::dim("  no goal. /goal <objective> to start one")),
            },
            "clear" | "off" => {
                agent.goal = None;
                println!("{}", ui::dim("  goal cleared"));
            }
            "resume" | "continue" => agent.run_goal(None)?,
            objective => agent.run_goal(Some(objective))?,
        },
        "/loop" => match parse_loop(rest) {
            Some((interval, max, prompt)) => agent.run_loop(&prompt, interval, max)?,
            None => println!("  usage: /loop [30s|5m|1h] [x10] <prompt>   (no interval: rusty picks the pace)"),
        },
        "/memory" | "/mem" => memory_cmd(agent, rest),
        "/remember" => {
            if rest.is_empty() {
                println!("  usage: /remember [preference|fact|decision|gotcha:] <text>");
            } else {
                let (kind, text) = match rest.split_once(':') {
                    Some((k, t)) if rusty::advisor::KINDS.contains(&k.trim()) => (k.trim(), t.trim()),
                    _ => ("fact", rest),
                };
                println!("  {}", agent.remember(kind, text, kind == "preference"));
            }
        }
        "/forget" => {
            println!("  {}", agent.forget_memory(rest));
        }
        "/skills" => {
            for s in skills::all(cwd) {
                println!(
                    "  {} {} {}",
                    ui::info(&format!("{:<14}", format!("/{}", s.name))),
                    s.description,
                    ui::dim(&format!("· {}", s.source))
                );
            }
            println!("{}", ui::dim("  add your own as .md files in ~/.config/rusty/skills or .rusty/skills"));
        }
        other => match expand_skill(cwd, input) {
            Some(prompt) => {
                agent.run_turn(&prompt)?;
            }
            None => {
                let skill_list: slash::Skills = skills::all(cwd).into_iter().map(|s| (s.name, s.description)).collect();
                match slash::suggest(other, &skill_list) {
                    Some(near) => println!("  unknown command {other}. did you mean {near}? /help lists them"),
                    None => println!("  unknown command {other}. /help lists them"),
                }
            }
        },
    }
    Ok(false)
}

/// The live target for the banner and `/target`, with production called out.
fn target_line(agent: &Agent) -> String {
    let t = &agent.infra.target;
    if t.production {
        format!("{} {}", t.summary(), ui::warn("· production"))
    } else {
        t.summary()
    }
}

fn on_off(b: bool) -> String {
    if b {
        ui::ok("on")
    } else {
        ui::dim("off")
    }
}

/// The startup banner; `/font` shows it again in the new font.
fn banner(agent: &Agent, client: &Client, cwd: &Path) {
    ui::banner(&ui::BannerInfo {
        model: &agent.model,
        cwd: &tilde(cwd),
        exec: if agent.mode_fixed { agent.execution_mode.name() } else { "auto" },
        perms: agent.policy.mode.name(),
        agents: agent.agents.mode.name(),
        view: display::view_name(),
        provider: client.endpoint_for(&agent.model).map(|e| e.provider.name()).unwrap_or("no key"),
        keys: client.key_count(),
        target: &target_line(agent),
    });
}

fn agents_cmd(agent: &mut Agent, rest: &str) {
    let (sub, arg) = rest.split_once(char::is_whitespace).map(|(s, a)| (s, a.trim())).unwrap_or((rest, ""));
    match sub {
        "" => {}
        "model" => {
            agent.agents.sub_model = if arg.is_empty() || arg == "default" { None } else { Some(arg.to_string()) };
        }
        m => match AgentsMode::parse(m) {
            Some(mode) => agent.agents.mode = mode,
            None => {
                println!("  /agents off · sub · swarm · auto · default · model <id>");
                return;
            }
        },
    }
    let a = &agent.agents;
    let desc = match a.mode {
        AgentsMode::Off => "no delegation",
        AgentsMode::Sub => "one read-only subagent at a time",
        AgentsMode::Swarm => "parallel read-only workers",
        AgentsMode::Auto => "rusty picks: itself, a subagent, or a swarm",
    };
    println!("  agents {} {}", ui::bold(a.mode.name()), ui::dim(&format!("· {desc}")));
    println!("  {}", ui::dim(&format!("subagent model {}", a.sub_model.as_deref().unwrap_or("(same as main)"))));
}

fn swarm_cmd(agent: &mut Agent, rest: &str) {
    let (sub, arg) = rest.split_once(char::is_whitespace).map(|(s, a)| (s, a.trim())).unwrap_or((rest, ""));
    let a = &mut agent.agents;
    match sub {
        "" => {}
        "on" => a.mode = AgentsMode::Swarm,
        "off" => a.mode = AgentsMode::Off,
        "size" | "max" => match arg.parse::<usize>() {
            Ok(n) if (1..=16).contains(&n) => a.swarm_max = n,
            _ => println!("  size must be 1-16"),
        },
        "models" => {
            a.swarm_models = arg
                .split([',', ' '])
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "default")
                .map(String::from)
                .collect()
        }
        "spread" => match arg.parse::<f32>() {
            Ok(x) if (0.0..=1.0).contains(&x) => a.spread = x,
            _ => println!("  spread must be between 0 and 1"),
        },
        _ => {
            println!("  /swarm on · off · size <n> · models <a,b,…> · spread <0-1>");
            return;
        }
    }
    let models = if a.swarm_models.is_empty() { "(subagent model)".to_string() } else { a.swarm_models.join(", ") };
    println!(
        "  swarm {} {}",
        if a.mode == AgentsMode::Swarm || a.mode == AgentsMode::Auto { ui::ok("armed") } else { ui::dim("off") },
        ui::dim(&format!("· mode {}", a.mode.name()))
    );
    println!(
        "  {}",
        ui::dim(&format!("size ≤{} · models {models} · temperature 0.2→{:.1}", a.swarm_max, 0.2 + a.spread))
    );
}

fn print_mode(agent: &Agent) {
    if !agent.mode_fixed {
        let last = if agent.mode_reason.is_empty() {
            String::new()
        } else {
            format!(" · last pick {} ({})", agent.execution_mode.name(), agent.mode_reason)
        };
        println!(
            "  execution {} {}",
            ui::bold("auto"),
            ui::dim(&format!("· picks careful, standard or vibe per request{last}"))
        );
        return;
    }
    let m = agent.execution_mode;
    let what = match m {
        ExecutionMode::Careful => "thinks longer, and a read-only checker reviews finished work",
        ExecutionMode::Standard => "normal effort, checks what it changed",
        ExecutionMode::Vibe => "quick passes, small checks, may send out a few read-only workers",
    };
    println!("  execution {} {}", ui::bold(m.name()), ui::dim(&format!("· {what}")));
    println!(
        "  {}",
        ui::dim(&format!(
            "model {} · checker {} · workers {} (≤{})",
            agent.model,
            m.checker(),
            agent.agents.mode.name(),
            agent.swarm_cap()
        ))
    );
}

fn print_settings(agent: &Agent) {
    let a = &agent.agents;
    let rows = [
        ("model", agent.model.clone()),
        ("tools", agent.backend.summary()),
        ("memory location", "this process (controller)".into()),
        ("execution", if agent.mode_fixed { agent.execution_mode.name().to_string() } else { "auto".to_string() }),
        ("checker", agent.execution_mode.checker().to_string()),
        ("permissions", agent.policy.mode.name().to_string()),
        ("view", display::view_name().to_string()),
        ("theme", ui::theme().name.to_string()),
        ("font", ui::font().to_string()),
        ("agents", a.mode.name().to_string()),
        ("subagent model", a.sub_model.clone().unwrap_or_else(|| "(same as main)".into())),
        ("swarm size", a.swarm_max.to_string()),
        ("swarm models", if a.swarm_models.is_empty() { "(subagent model)".into() } else { a.swarm_models.join(", ") }),
        ("swarm spread", format!("{:.1}", a.spread)),
        ("tips", if agent.tips { "on".into() } else { "off".into() }),
        ("target", if agent.infra.target.is_empty() { "-".into() } else { agent.infra.target.summary() }),
        ("context window", format!("{} ({})", agent.meter.window, agent.meter.window_source)),
    ];
    for (k, v) in rows {
        println!("  {} {}", ui::dim(&format!("{k:<15}")), v);
    }
}

fn print_tokens(agent: &Agent) {
    let t = &agent.totals;
    println!(
        "  {} requests · ↑{} prompt · ↓{} completion · context {}%",
        t.requests,
        ui::human(t.prompt),
        ui::human(t.completion),
        agent.context_pct()
    );
    if let Some(avg) = t.prompt.checked_div(t.requests) {
        println!("  {}", ui::dim(&format!("avg prompt {} per request", ui::human(avg))));
    }
    for (m, (r, p, c)) in &t.by_model {
        println!(
            "  {} {}",
            ui::info(&format!("{m:<44}")),
            ui::dim(&format!("{r} req · ↑{} ↓{}", ui::human(*p), ui::human(*c)))
        );
    }
    let mut tools: Vec<_> = t.by_tool.iter().collect();
    tools.sort_by(|a, b| b.1.cmp(a.1));
    if !tools.is_empty() {
        let parts: Vec<String> = tools.iter().take(6).map(|(k, v)| format!("{k} ~{}", ui::human(**v))).collect();
        println!("  {} {}", ui::dim("tool output →"), ui::dim(&parts.join(" · ")));
    }
}

fn permissions_cmd(agent: &mut Agent, rest: &str) {
    let (sub, arg) = rest.split_once(char::is_whitespace).map(|(s, a)| (s, a.trim())).unwrap_or((rest, ""));
    let p = &mut agent.policy;
    match sub {
        "" => {
            println!("  mode {}", ui::bold(p.mode.name()));
            println!("    read-only  reads, searches and read-only commands (cloud reads too)");
            println!("    ask        asks before every edit or command that changes something");
            println!("    auto       project work, everyday git and cloud reads run; the rest asks");
            println!("    yolo       everything runs except deny rules and destructive commands");
            println!(
                "  {}",
                ui::dim("destructive (force push, cloud or database deletes, rm -rf outside) asks every time")
            );
            println!("  allow {}", if p.allow.is_empty() { "-".into() } else { p.allow.join(", ") });
            println!("  deny  {}", if p.deny.is_empty() { "-".into() } else { p.deny.join(", ") });
            println!(
                "{}",
                ui::dim("  /permissions <mode> · allow <rule> · deny <rule> · remove <rule> · reset · check <command>")
            );
        }
        "allow" | "deny" if !arg.is_empty() => {
            if sub == "allow" {
                p.allow.push(arg.into())
            } else {
                p.deny.push(arg.into())
            }
            p.save();
            println!("  {sub} `{arg}`");
        }
        "remove" if !arg.is_empty() => {
            p.allow.retain(|r| r != arg);
            p.deny.retain(|r| r != arg);
            p.save();
            println!("  removed `{arg}`");
        }
        "reset" => {
            p.allow.clear();
            p.deny.clear();
            p.save();
            println!("  rules cleared");
        }
        "check" if !arg.is_empty() => {
            let args = serde_json::json!({"command": arg});
            let decision = match agent.backend.decide("bash", &args, p, &agent.cwd) {
                Ok(verdict) => verdict,
                Err(e) => {
                    println!("  cannot check remote permissions: {e:#}");
                    return;
                }
            };
            let verdict = match decision {
                Verdict::Allow => ui::ok("✓ runs"),
                Verdict::Ask(_) => ui::warn("? asks first"),
                Verdict::Confirm(_) => ui::err("‼ needs you every time"),
                Verdict::Deny(why) => ui::err(&format!("✗ refused · {why}")),
            };
            let class = if agent.backend.name() == "local" {
                permissions::classify("bash", &args, &agent.cwd).describe()
            } else {
                "checked in the sandbox".into()
            };
            println!("  {verdict} {}", ui::dim(&format!("· {class} · {} mode", p.mode.name())));
        }
        m => match Mode::parse(m) {
            Some(mode) => {
                p.mode = mode;
                println!("  permissions {}", ui::bold(mode.name()));
            }
            None => println!("  unknown option `{m}`. /permissions for help"),
        },
    }
}

fn memory_cmd(agent: &mut Agent, rest: &str) {
    let Some(h) = &mut agent.advisor else {
        println!("  memory is off");
        return;
    };
    let (sub, arg) = rest.split_once(char::is_whitespace).map(|(s, a)| (s, a.trim())).unwrap_or((rest, ""));
    let (op, data) = match sub {
        "" | "list" => ("list", serde_json::Value::Null),
        "search" => ("recall", serde_json::json!({"query":arg})),
        "forget" => ("forget", serde_json::json!({"id":arg})),
        "status" => ("status", serde_json::Value::Null),
        "helpful" | "harmful" => ("feedback", serde_json::json!({"helpful":sub=="helpful"})),
        _ => {
            println!("  /memory [list | search <query> | forget <id> | status | helpful | harmful]");
            return;
        }
    };
    let text = h.control(op, data).text;
    if text.is_empty() {
        println!("{}", ui::dim("  no memories yet. rusty saves them as it learns, or use /remember"));
    } else {
        println!("  {text}");
    }
}

/// Parses `[30s|5m|1h] [x10] <prompt>`.
fn parse_loop(rest: &str) -> Option<(Option<u64>, Option<u32>, String)> {
    let mut words: Vec<&str> = rest.split_whitespace().collect();
    let mut interval = None;
    let mut max = None;
    if let Some(secs) = words.first().and_then(|w| parse_duration(w)) {
        interval = Some(secs);
        words.remove(0);
    }
    if let Some(n) = words.first().and_then(|w| w.strip_prefix('x')).and_then(|n| n.parse().ok()) {
        max = Some(n);
        words.remove(0);
    }
    let prompt = words.join(" ");
    (!prompt.is_empty()).then_some((interval, max, prompt))
}

fn parse_duration(s: &str) -> Option<u64> {
    let (num, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: u64 = num.parse().ok()?;
    match unit {
        "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        _ => None,
    }
}

fn print_help(cwd: &Path) {
    let skill_list: slash::Skills = skills::all(cwd).into_iter().map(|s| (s.name, s.description)).collect();
    print!("{}", slash::help(&skill_list));
}

/// `/Users/me/code/x` → `~/code/x`.
fn tilde(p: &Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

fn project_dir(cwd: &Path) -> PathBuf {
    let name = cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "root".into());
    let id = format!("{name}-{:08x}", rusty::fnv(&cwd.display().to_string()) as u32);
    config::config_dir().unwrap_or_else(|| PathBuf::from(".rusty")).join("projects").join(id)
}

fn resume_session(agent: &mut Agent, dir: &Path, entry: &session::Entry) -> Result<()> {
    let path = session::path(dir, &entry.id);
    if agent.session_path.as_deref() != Some(path.as_path()) {
        agent.load_session(&path)?;
    }
    let title = if entry.title.is_empty() { String::new() } else { format!(" · {}", entry.title) };
    println!("{}", ui::dim(&format!("  resumed {}{title} · {} messages", entry.id, agent.history.len())));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_goodbye_says_how_to_resume_and_fits_the_terminal() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(goodbye(None, 80), "  ◌ link closed. go ship something.");
        assert_eq!(goodbye(Some(id), 120), format!("  ◌ link closed. go ship something. resume with `rusty -c {id}`"));
        for width in [40, 59, 80] {
            let lines: Vec<String> = goodbye(Some(id), width).lines().map(String::from).collect();
            assert_eq!(
                lines,
                ["  ◌ link closed. go ship something.".to_string(), format!("  resume with `rusty -c {id}`")]
            );
        }
    }

    #[test]
    fn loop_args() {
        assert_eq!(parse_loop("5m x3 check ci"), Some((Some(300), Some(3), "check ci".into())));
        assert_eq!(parse_loop("check ci"), Some((None, None, "check ci".into())));
        assert_eq!(parse_loop("30s"), None);
    }
}
