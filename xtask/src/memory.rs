//! Memory service checks through the real binaries.
//!
//! `memory-smoke` runs easy create/edit tasks through the real CLI in each
//! memory mode, against an offline model provider (or NVIDIA with `--live`).
//! Verifiers and receipts stay outside the agent's workspace, and no raw model
//! logs are uploaded. A provider failure is recorded separately and still
//! fails. `memory-bench` measures socket advice latency with a full
//! 512-lesson store. `memory-summary` turns a smoke receipt into a Markdown
//! table for a CI job summary.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

const TASK_REVISION: &str = "r4-two-byte-note";
const TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "bash",
    "list_files",
    "search",
    "locate",
    "glob",
    "outline",
    "plan",
    "remember",
    "recall",
    "forget",
];
const PROVIDER_ERRORS: &[&str] = &["429", "502", "503", "504", "unauthorized", "rate limit"];

const SMOKE_USAGE: &str = "usage: cargo xtask memory-smoke [--live] [--binary PATH] [--model ID] \
[--timeout SECONDS] [--modes off|recall|learn|reflect|deep ...] [--report PATH]";
/// The memory levels the smoke drives, from none to the most aggressive.
const LEVELS: [&str; 5] = ["off", "recall", "learn", "reflect", "deep"];
const BENCH_USAGE: &str = "usage: cargo xtask memory-bench [--binary PATH] [--report PATH]";

pub struct Smoke {
    pub binary: PathBuf,
    pub live: bool,
    pub model: String,
    pub timeout: Duration,
    pub modes: Vec<String>,
    pub report: PathBuf,
}

impl Smoke {
    pub fn new(binary: PathBuf, report: PathBuf) -> Self {
        Smoke {
            binary,
            live: false,
            model: "nvidia/nemotron-3-super-120b-a12b".into(),
            timeout: Duration::from_secs(180),
            modes: LEVELS.map(String::from).to_vec(),
            report,
        }
    }
}

/// `cargo xtask memory-smoke ...`: 0 when every case passes, 1 when one
/// fails, 2 for bad arguments.
pub fn smoke_command(args: &[String]) -> Result<ExitCode> {
    let mut opts = Smoke::new("target/debug/rusty".into(), "memory-smoke.json".into());
    let mut it = args.iter().peekable();
    let parsed = (|| {
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--live" => opts.live = true,
                "--binary" => opts.binary = value(&mut it, arg)?.into(),
                "--report" => opts.report = value(&mut it, arg)?.into(),
                "--model" => opts.model = value(&mut it, arg)?,
                "--timeout" => {
                    let v = value(&mut it, arg)?;
                    opts.timeout = Duration::from_secs(v.parse().map_err(|_| anyhow!("bad --timeout {v}"))?);
                }
                "--modes" => {
                    opts.modes.clear();
                    while let Some(m) = it.next_if(|a| !a.starts_with('-')) {
                        ensure!(LEVELS.contains(&m.as_str()), "bad level {m}; choose from {}", LEVELS.join(", "));
                        opts.modes.push(m.clone());
                    }
                    ensure!(!opts.modes.is_empty(), "--modes needs at least one mode");
                }
                "-h" | "--help" => return Ok(false),
                _ => bail!("unknown argument {arg}"),
            }
        }
        Ok(true)
    })();
    if let Some(code) = usage_exit(parsed, SMOKE_USAGE) {
        return Ok(code);
    }
    Ok(if smoke(&opts)? { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// Runs create and edit in every mode and writes the receipt after each case,
/// so an interrupted job keeps its completed attempts. True when all pass.
pub fn smoke(opts: &Smoke) -> Result<bool> {
    let binary = resolve(&opts.binary)?;
    let mock = Mock::start()?;
    let endpoint = format!("http://127.0.0.1:{}/v1", mock.port);
    mkdir_parent(&opts.report)?;
    let mut runs = Vec::new();
    let result = (|| -> Result<()> {
        for mode in &opts.modes {
            for task in ["create", "edit"] {
                runs.push(run_case(opts, &binary, mode, task, &endpoint, &mock.requests)?);
                let partial = json!({
                    "version": 1, "task_revision": TASK_REVISION, "live": opts.live, "model": opts.model,
                    "runs": runs, "passed": false, "complete": false,
                });
                write_json(&opts.report, &partial)?;
            }
        }
        Ok(())
    })();
    mock.shutdown();
    result?;
    let passed = runs.iter().all(|r| r["passed"] == true);
    let receipt = json!({
        "version": 1, "task_revision": TASK_REVISION, "live": opts.live, "model": opts.model,
        "complete": true, "runs": runs, "passed": passed,
    });
    mkdir_parent(&opts.report)?;
    write_json(&opts.report, &receipt)?;
    Ok(passed)
}

fn run_case(opts: &Smoke, binary: &Path, mode: &str, task: &str, endpoint: &str, requests: &Requests) -> Result<Value> {
    let tmp = TempDir::new("rms-")?;
    let root = tmp.0.clone();
    let (project, home) = (root.join("project"), root.join("home"));
    std::fs::create_dir(&project)?;
    std::fs::create_dir(&home)?;
    if task == "edit" {
        std::fs::write(project.join("calculator.py"), "SENTINEL = \"keep me\"\ndef add(a, b):\n    return a - b\n")?;
    }
    let mut env = scrubbed_env();
    for (k, v) in [
        ("RUSTY_HOME", home.to_string_lossy().into_owned()),
        ("RUSTY_NO_DOTENV", "1".into()),
        ("RUSTY_PROJECT_ID", "memory-smoke".into()),
        ("NO_COLOR", "1".into()),
    ] {
        env.insert(k.into(), v);
    }
    if opts.live {
        let key = [std::env::var("RUSTY_API_KEY"), std::env::var("NVIDIA_API_KEY")]
            .into_iter()
            .flatten()
            .find(|k| !k.is_empty())
            .ok_or_else(|| anyhow!("live check requires NVIDIA_API_KEY or RUSTY_API_KEY"))?;
        env.insert("NVIDIA_API_KEY".into(), key);
        if let Some(url) = std::env::var("RUSTY_BASE_URL").ok().filter(|u| !u.is_empty()) {
            env.insert("RUSTY_BASE_URL".into(), url);
        }
    } else {
        env.insert("NVIDIA_API_KEY".into(), "offline-test-key".into());
        env.insert("RUSTY_BASE_URL".into(), endpoint.into());
    }
    let log = std::fs::File::create(root.join("daemon.log"))?;
    let mut daemon = Daemon(
        Command::new(binary.with_file_name("rusty-memoryd"))
            .arg("--home")
            .arg(&home)
            .env_clear()
            .envs(&env)
            .stdout(Stdio::null())
            .stderr(log)
            .process_group(0)
            .spawn()
            .context("starting rusty-memoryd")?,
    );
    let daemon = &mut daemon.0;
    let (root, project, home) = (root.as_path(), project.as_path(), home.as_path());
    let deadline = Instant::now() + Duration::from_secs(120);
    // A socket pathname can appear between bind and listen. A successful
    // status reply is the readiness signal, before any mutating RPC.
    while rpc(home, "status", Value::Null, 0).is_err() {
        if daemon.try_wait()?.is_some() || Instant::now() > deadline {
            bail!("daemon did not start");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let lesson = if task == "edit" {
        "For the add function, inspect calculator.py before editing and preserve SENTINEL."
    } else {
        "For creating note.txt, write exactly 42 and verify with Bash."
    };
    let r = rpc(home, "remember", json!({"kind": "gotcha", "text": lesson}), 0)?;
    ensure!(!truthy(&r["error"]), "remember failed: {}", r["text"]);
    let prompt = if task == "edit" {
        "Fix calculator.py so add(a,b) adds a and b. Preserve SENTINEL. Read calculator.py directly with \
read_file, then use edit_file. Finally use the bash tool to run this exact command: python3 -c 'from \
calculator import add; assert add(2,3)==5 and add(-2,3)==1' && mkdir -p artifacts && printf verified > \
artifacts/check.txt\nCreate the marker through Bash, not write_file. Report the result briefly. This task \
needs no directory discovery or extra planning."
    } else {
        "Use write_file to create note.txt containing exactly the two ASCII bytes 42, without a newline. Then \
use the bash tool to run this exact command: test \"$(cat note.txt)\" = 42 && mkdir -p artifacts && printf \
verified > artifacts/check.txt\nCreate the marker through Bash, not write_file. Report the result briefly. \
This task needs no directory discovery or extra planning."
    };
    let trajectory = root.join("trajectory.json");
    let mut command = Command::new(binary);
    if !opts.model.is_empty() {
        command.args(["--model", &opts.model]);
    }
    command
        .args(["--yolo", "--mode", "vibe", "--agents", "off", "--memory", mode, "--stats", "--trajectory"])
        .arg(&trajectory)
        .arg(prompt)
        .current_dir(project)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let before = requests.lock().unwrap().len();
    let started = Instant::now();
    let mut child = command.spawn().context("starting rusty")?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let (status, timed_out) = wait_timeout(&mut child, opts.timeout)?;
    let (status, timed_out) = if timed_out { (stop(&mut child), true) } else { (status, false) };
    let _stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let wall = started.elapsed().as_secs_f64();

    let trace: Value = match std::fs::read_to_string(&trajectory) {
        Ok(text) => serde_json::from_str(&text).context("parsing the trajectory")?,
        Err(_) => json!({}),
    };
    let has_trace = trace.as_object().is_some_and(|o| !o.is_empty());
    let calls: Vec<&Value> = trace["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
        .collect();
    let mut names: BTreeMap<String, u64> = BTreeMap::new();
    let mut shapes: BTreeMap<String, u64> = BTreeMap::new();
    for c in &calls {
        let name = c["function"]["name"].as_str().ok_or_else(|| anyhow!("tool call without a name"))?;
        *names.entry(name.into()).or_default() += 1;
        // serde_json maps are sorted, like json.dumps(sort_keys=True).
        *shapes.entry(c["function"].to_string()).or_default() += 1;
    }
    let repeated: u64 = shapes.values().filter(|&&n| n > 1).map(|n| n - 1).sum();
    let count = |n: &str| names.get(n).copied().unwrap_or(0);
    let artifact = project.join("artifacts/check.txt");
    let mut checks = Map::new();
    let mut check = |name: &str, ok: bool| {
        checks.insert(name.into(), Value::Bool(ok));
    };
    check("no_unknown_tools", names.keys().all(|n| TOOLS.contains(&n.as_str())));
    check("exit", status.success() && !timed_out);
    check("bash_artifact", std::fs::read_to_string(&artifact).is_ok_and(|t| t.trim_end_matches('\n') == "verified"));
    check("bash_used", count("bash") > 0);
    check("tool_budget", has_trace && calls.len() <= 12);
    check("repeat_budget", has_trace && repeated <= 2);
    if task == "create" {
        check("file_content", std::fs::read_to_string(project.join("note.txt")).is_ok_and(|t| t == "42"));
        check("write_used", count("write_file") > 0);
    } else {
        let code = "from calculator import add,SENTINEL; assert add(2,3)==5 and add(-2,3)==1 and SENTINEL==\"keep me\"";
        let mut verifier = Command::new("python3")
            .args(["-c", code])
            .current_dir(project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the python3 verifier")?;
        let (vstatus, vtimed_out) = wait_timeout(&mut verifier, Duration::from_secs(5))?;
        if vtimed_out {
            let _ = verifier.kill();
            let _ = verifier.wait();
            bail!("the calculator verifier timed out");
        }
        check("function_and_sentinel", vstatus.success());
        check("edit_used", count("edit_file") > 0);
    }
    let metrics = match &trace["memory"] {
        Value::Object(m) if !m.is_empty() => Value::Object(m.clone()),
        _ => json!({}),
    };
    let metric = |k: &str| metrics[k].as_f64().unwrap_or(0.0);
    if mode != "off" {
        check("advisor_enabled", trace["memory_mode"] == mode);
        check("injected", metric("injections") > 0.0);
        check("hook_deadline", metric("max_hook_us") < 150_000.0);
        check("no_hook_timeout", metric("timeouts") == 0.0);
    } else {
        check("memory_off", trace["memory_mode"] == "off" && metrics.as_object().is_some_and(Map::is_empty));
    }
    if !opts.live {
        let bodies = requests.lock().unwrap();
        let seen = bodies[before..].iter().any(|b| {
            b["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| m["role"] == "system" && m["content"].as_str().unwrap_or("").contains("From memory"))
        });
        check("prompt_injection_matches_mode", seen == (mode != "off"));
    }
    let failure = if timed_out {
        "timeout"
    } else if !status.success() && PROVIDER_ERRORS.iter().any(|s| stderr.to_lowercase().contains(s)) {
        "provider"
    } else {
        "behavior"
    };
    let service = rpc(home, "status", Value::Null, 0)?["data"].take();
    let storage = dir_bytes(&home.join("memory"))?;
    check("storage_budget", storage < 20 * 1024 * 1024);
    let passed = checks.values().all(|v| v == true);
    let mut report = json!({
        "mode": mode,
        "task": task,
        "passed": passed,
        "checks": checks,
        "wall_seconds": round3(wall),
        "trajectory_complete": has_trace,
        "tool_calls": if has_trace { json!(calls.len()) } else { Value::Null },
        "tools": names,
        "repeated_calls": if has_trace { json!(repeated) } else { Value::Null },
        "totals": trace["totals"],
        "memory": metrics,
        "storage_bytes": storage,
        "service": service,
        "artifact_bytes": std::fs::metadata(&artifact).ok().map(|m| m.len()),
    });
    if !passed {
        report["failure_class"] = json!(failure);
        if has_trace {
            let stem = opts.report.file_stem().unwrap_or_default().to_string_lossy();
            let diagnostic = opts.report.with_file_name(format!("{stem}-{mode}-{task}-failure.json"));
            mkdir_parent(&diagnostic)?;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&diagnostic)
                .with_context(|| format!("writing {}", diagnostic.display()))?;
            f.write_all(trace.to_string().as_bytes())?;
        }
        if !opts.live {
            let exit = daemon.try_wait()?.map(|s| s.to_string()).unwrap_or_else(|| "running".into());
            let log = std::fs::read_to_string(root.join("daemon.log")).unwrap_or_default();
            println!("Offline failure: {stderr} daemon exit: {exit} {log}");
        }
    }
    println!("{report}");
    Ok(report)
}

/// `cargo xtask memory-bench ...`: real socket advice latency with a full
/// 512-lesson local store. 0 when p95 < 20 ms and storage < 20 MiB.
pub fn bench_command(args: &[String]) -> Result<ExitCode> {
    let (mut binary, mut report) = (PathBuf::from("target/release/rusty-memoryd"), PathBuf::from("memory-bench.json"));
    let mut it = args.iter().peekable();
    let parsed = (|| {
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--binary" => binary = value(&mut it, arg)?.into(),
                "--report" => report = value(&mut it, arg)?.into(),
                "-h" | "--help" => return Ok(false),
                _ => bail!("unknown argument {arg}"),
            }
        }
        Ok(true)
    })();
    if let Some(code) = usage_exit(parsed, BENCH_USAGE) {
        return Ok(code);
    }
    let binary = resolve(&binary)?;
    let tmp = TempDir::new("rmb-")?;
    let home = tmp.0.clone();
    let _daemon = Daemon(
        Command::new(&binary)
            .arg("--home")
            .arg(&home)
            .env_clear()
            .envs(scrubbed_env())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .context("starting rusty-memoryd")?,
    );
    Ok(if bench(&binary, &home, &report)? { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn bench(binary: &Path, home: &Path, report: &Path) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !home.join("memory/advisor.sock").exists() {
        ensure!(Instant::now() <= deadline, "startup deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
    for i in 0..512 {
        // Distinct enough that near-duplicate merging keeps all 512.
        let text =
            format!("Workspace package {i}: inspect manifest then run Python checks for module m{i} and service s{i}.");
        let r = rpc(home, "remember", json!({"kind": "fact", "text": text}), 0)?;
        ensure!(!truthy(&r["error"]), "{}", r["text"]);
    }
    ensure!(rpc(home, "status", Value::Null, 0)?["data"]["lessons"] == 512, "the store does not hold 512 lessons");
    // Cold load then 100 fresh decisions. begin updates the bounded RAM
    // notepad; advice times include the socket, selection, JSON and response.
    let mut times = Vec::with_capacity(101);
    for seq in 1..=101 {
        let r = rpc(home, "begin", json!({"text": "inspect workspace package manifest Python checks"}), seq)?;
        ensure!(!truthy(&r["error"]), "begin failed: {}", r["text"]);
        let start = Instant::now();
        let r = rpc(home, "advice", Value::Null, seq)?;
        let elapsed = start.elapsed().as_nanos() as f64 / 1e6;
        ensure!(truthy(&r["id"]), "no advice selected from full store");
        times.push(elapsed);
    }
    let mut warm = times[1..].to_vec();
    warm.sort_by(f64::total_cmp);
    let p50 = (warm[49] + warm[50]) / 2.0;
    let p95 = warm[94];
    let max = times.iter().copied().fold(f64::MIN, f64::max);
    let size = dir_bytes(&home.join("memory"))?;
    let passed = p95 < 20.0 && size < 20 * 1024 * 1024;
    let result = json!({
        "lessons": 512,
        "cold_ms": round3(times[0]),
        "p50_ms": round3(p50),
        "p95_ms": round3(p95),
        "max_ms": round3(max),
        "storage_bytes": size,
        "binary_bytes": std::fs::metadata(binary)?.len(),
        "passed": passed,
    });
    mkdir_parent(report)?;
    write_json(report, &result)?;
    println!("{result}");
    Ok(passed)
}

/// `cargo xtask memory-summary FILE`: a smoke receipt as a Markdown table.
pub fn summary_command(args: &[String]) -> Result<ExitCode> {
    let Some(path) = args.first() else {
        eprintln!("usage: cargo xtask memory-summary RECEIPT");
        return Ok(ExitCode::from(2));
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("No behavioral receipt produced; inspect the failed step.");
        return Ok(ExitCode::SUCCESS);
    };
    let receipt: Value = serde_json::from_str(&text).with_context(|| format!("parsing {path}"))?;
    println!("| Mode | Task | Pass | Seconds | Tools | Repeats | Max hook µs |\n|---|---|---|---|---|---|---|");
    for r in receipt["runs"].as_array().into_iter().flatten() {
        let hook = r["memory"].get("max_hook_us").cloned().unwrap_or(json!(0));
        println!(
            "| {} | {} | {} | {} | {} | {} | {hook} |",
            plain(&r["mode"]),
            plain(&r["task"]),
            r["passed"],
            r["wall_seconds"],
            r["tool_calls"],
            r["repeated_calls"]
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// One request to the memory service socket, scoped like the CLI's project
/// `memory-smoke` (FNV-1a of the project id).
fn rpc(home: &Path, op: &str, data: Value, seq: u64) -> Result<Value> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in "memory-smoke".bytes() {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let request = json!({
        "op": op, "scope": format!("project:{hash:016x}"), "session": "seed", "seq": seq, "level": "learn", "data": data,
    });
    let socket = home.join("memory/advisor.sock");
    let mut s = UnixStream::connect(&socket).with_context(|| format!("connecting to {}", socket.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(format!("{request}\n").as_bytes())?;
    let mut line = Vec::new();
    BufReader::new(s).take(4 * 1024 * 1024).read_until(b'\n', &mut line)?;
    serde_json::from_slice(&line).with_context(|| format!("reading the {op} response"))
}

type Requests = Arc<Mutex<Vec<Value>>>;

/// An OpenAI-compatible provider on 127.0.0.1 that walks the CLI through a
/// fixed tool plan, and answers the advisor's background (non-streaming)
/// calls by citing the latest observation.
struct Mock {
    port: u16,
    requests: Requests,
    stopping: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Mock {
    fn start() -> Result<Mock> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let requests = Requests::default();
        let stopping = Arc::new(AtomicBool::new(false));
        let (reqs, stop) = (requests.clone(), stopping.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let reqs = reqs.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &reqs);
                });
            }
        });
        Ok(Mock { port, requests, stopping, thread })
    }

    fn shutdown(self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        let _ = self.thread.join();
    }
}

fn serve(stream: TcpStream, requests: &Requests) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    ensure!(line.starts_with("POST "), "only POST is served");
    let mut length = None;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, v)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(v.trim().parse::<usize>()?);
            }
        }
    }
    let mut body = vec![0; length.ok_or_else(|| anyhow!("no Content-Length"))?];
    reader.read_exact(&mut body)?;
    let body: Value = serde_json::from_slice(&body)?;
    let mut out = stream;
    let messages: Vec<Value> = body["messages"].as_array().cloned().unwrap_or_default();
    requests.lock().unwrap().push(body);
    let tools = messages.iter().filter(|m| m["role"] == "tool").count();
    let edit = messages.iter().filter(|m| m["role"] == "user").any(|m| match &m["content"] {
        Value::String(s) => s.contains("calculator.py"),
        Value::Null => false,
        other => other.to_string().contains("calculator.py"),
    });
    let plan = if edit {
        vec![
            ("read_file", json!({"path": "calculator.py"})),
            ("edit_file", json!({"path": "calculator.py", "old_string": "return a - b", "new_string": "return a + b"})),
            (
                "bash",
                json!({"command": "python3 -c 'from calculator import add; assert add(2,3)==5' && mkdir -p artifacts && printf verified > artifacts/check.txt"}),
            ),
        ]
    } else {
        vec![
            ("write_file", json!({"path": "note.txt", "content": "42"})),
            (
                "bash",
                json!({"command": "test \"$(cat note.txt)\" = 42 && mkdir -p artifacts && printf verified > artifacts/check.txt"}),
            ),
        ]
    };
    let (delta, finish) = match plan.get(tools) {
        Some((name, args)) => (
            json!({"tool_calls": [{"index": 0, "id": format!("call{tools}"), "type": "function",
                "function": {"name": name, "arguments": args.to_string()}}]}),
            "tool_calls",
        ),
        None => (json!({"content": "Done and verified."}), "stop"),
    };
    let reply = json!({"choices": [{"delta": delta, "finish_reason": finish}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}});
    let data = format!("data: {reply}\n\ndata: [DONE]\n\n");
    write!(out, "HTTP/1.0 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{data}", data.len())?;
    Ok(())
}

/// A directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Result<TempDir> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos();
        for _ in 0..100 {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!("{prefix}{}-{nanos:x}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("creating a temporary directory"),
            }
        }
        bail!("no free temporary directory name")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The memory service, stopped on every way out of a case.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        stop(&mut self.0);
    }
}

/// The caller's environment without RUSTY_* settings or NVIDIA keys.
fn scrubbed_env() -> BTreeMap<String, String> {
    std::env::vars().filter(|(k, _)| !k.starts_with("RUSTY_") && !k.starts_with("NVIDIA_API_KEY")).collect()
}

/// SIGTERM to the process group of a still-running child, then a bounded wait
/// (SIGKILL after five seconds).
fn stop(child: &mut Child) -> ExitStatus {
    if let Ok(None) = child.try_wait() {
        signal_group(child, libc::SIGTERM);
        if let Ok((status, false)) = wait_timeout(child, Duration::from_secs(5)) {
            return status;
        }
        signal_group(child, libc::SIGKILL);
    }
    child.wait().unwrap_or_default()
}

fn signal_group(child: &Child, signal: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: killpg only sends a signal; the group is the child's own,
        // created with process_group(0) and not yet reaped.
        unsafe { libc::killpg(pid, signal) };
    }
}

/// Waits for a child up to `limit`. Returns (status, true) on a timeout,
/// with the child still running and the status a placeholder.
fn wait_timeout(child: &mut Child, limit: Duration) -> Result<(ExitStatus, bool)> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, false));
        }
        if Instant::now() >= deadline {
            return Ok((ExitStatus::default(), true));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

fn dir_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let meta = entry?.metadata()?;
        if meta.is_file() {
            total += meta.len();
        }
    }
    Ok(total)
}

/// Whether a JSON value is set: null, false, 0, "", [] and {} are not.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn plain(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), String::from)
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// An absolute path with symlinks resolved when it exists.
fn resolve(path: &Path) -> Result<PathBuf> {
    Ok(std::fs::canonicalize(path).or_else(|_| std::path::absolute(path))?)
}

fn mkdir_parent(path: &Path) -> Result<()> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => {
            std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))
        }
        _ => Ok(()),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)? + "\n";
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

fn value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String> {
    it.next().cloned().ok_or_else(|| anyhow!("{flag} needs a value"))
}

/// The exit code when parsing stopped early: 0 after --help, 2 for bad
/// arguments (like argparse). None to go on.
fn usage_exit(parsed: Result<bool>, usage: &str) -> Option<ExitCode> {
    match parsed {
        Ok(true) => None,
        Ok(false) => {
            println!("{usage}");
            Some(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("{usage}\nerror: {e}");
            Some(ExitCode::from(2))
        }
    }
}
