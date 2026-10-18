//! Hybrid mode, offline: the real Rust CLI and tool endpoint, the localhost
//! bridge, and the real Daytona REST transport against a fake toolbox API.
//!
//! No Daytona resource or real model is used. Different controller and
//! sandbox directories expose accidental local execution. Scripted swarms
//! exercise real threads, HTTP, permissions, editing, Bash and optional local
//! memory hooks.

mod support;

use rusty::cloud::api::{Config, Daytona};
use rusty::cloud::bridge::{remote_rpc, Bridge};
use rusty::cloud::hybrid::{self, HybridArgs};
use rusty::cloud::Remote;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use support::*;

struct Hybrid {
    host: PathBuf,
    remote: PathBuf,
    home: PathBuf,
    api: FakeDaytona,
    transport: Arc<dyn Remote>,
    daemon: Option<std::process::Child>,
    tmp: TempDir,
}

impl Hybrid {
    fn new(name: &str) -> Self {
        let tmp = TempDir::new(name);
        let root = tmp.path();
        let (host, remote, home) = (root.join("controller"), root.join("sandbox"), root.join("home"));
        for p in [&host, &remote, &home] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(host.join("AGENTS.md"), "HOST-INSTRUCTIONS").unwrap();
        std::fs::write(remote.join("AGENTS.md"), "REMOTE-INSTRUCTIONS").unwrap();
        std::fs::write(host.join("source.py"), "HOST-CANARY").unwrap();
        std::fs::write(remote.join("source.py"), "def add(a,b):\n    return a - b\n").unwrap();
        let bin = Path::new(RUSTY).parent().unwrap();
        let machine = Machine::new(root.join("remote-home"), &format!("{}:/usr/bin:/bin", bin.display()));
        machine.env.lock().unwrap().insert("RUSTY_NO_DOTENV".into(), "1".into());
        let api = FakeDaytona::new(machine);
        api.add_sandbox("offline-sandbox", "started", json!({}));
        let d = Daytona::new(Config::new(Some(api.api()), Some(DAYTONA_KEY.into()), None).unwrap()).unwrap();
        let transport: Arc<dyn Remote> = Arc::new(d.remote(&d.sandbox("offline-sandbox").unwrap()).unwrap());
        Self { host, remote, home, api, transport, daemon: None, tmp }
    }

    fn machine(&self) -> &Machine {
        &self.api.machine
    }

    fn rusty(&self) -> Command {
        let mut c = Command::new(RUSTY);
        c.current_dir(&self.host)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("RUSTY_HOME", &self.home)
            .env("RUSTY_NO_DOTENV", "1")
            .env("NVIDIA_API_KEY", "offline-model-key")
            .env("NO_COLOR", "1");
        c
    }

    fn rpc(&self, request: &Value) -> Value {
        remote_rpc(self.transport.as_ref(), self.remote.to_str().unwrap(), request).unwrap()
    }

    fn bridge(&self) -> Bridge {
        Bridge::start(self.transport.clone(), self.remote.to_str().unwrap()).unwrap()
    }
}

impl Drop for Hybrid {
    fn drop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            unsafe { libc::kill(d.id() as i32, libc::SIGTERM) };
            let start = Instant::now();
            while d.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = d.kill();
            let _ = d.wait();
        }
    }
}

fn run(mut c: Command, limit: Duration) -> Output {
    wait(c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap(), limit)
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
}

#[test]
fn rpc_needs_no_model_key_and_checks_remote_scripts_and_rechecks_approval() {
    let h = Hybrid::new("rpc");
    let policy = json!({"mode": "auto", "allow": [], "deny": []});
    // Same script name, different bytes in the two workspaces.
    std::fs::write(h.host.join("script.sh"), "echo harmless\n").unwrap();
    std::fs::write(h.remote.join("script.sh"), "git push origin main\n").unwrap();
    let mut req = json!({"op": "decide", "name": "bash", "args": {"command": "bash script.sh"}, "policy": policy});
    let first = h.rpc(&req);
    assert_eq!(first["ok"], true, "{first}");
    assert!(first["data"].to_string().contains("Ask"), "{first}");
    req["op"] = json!("execute");
    req["approval"] = json!("Allow");
    assert_eq!(h.rpc(&req)["ok"], false, "an old or forged approval bypassed remote inspection");
    req["name"] = json!("write_file");
    req["args"] = json!({"path": "forbidden.txt", "content": "no"});
    req["policy"] = json!({"mode": "read-only"});
    assert_eq!(h.rpc(&req)["ok"], false);
    assert!(!h.remote.join("forbidden.txt").exists());
    // The tool process itself, not just its parent, has no model credentials.
    req["name"] = json!("bash");
    req["args"] = json!({"command": "env"});
    req["policy"] = json!({"mode": "yolo"});
    let env = h.rpc(&req)["data"].as_str().unwrap().to_string();
    assert!(env.contains("HOME="), "{env}");
    assert!(!env.contains("NVIDIA_API_KEY") && !env.contains("RUSTY_TOOL_BRIDGE_TOKEN"));
    let described = h.rpc(&json!({"op": "describe"}));
    assert_eq!(described["data"]["sandbox"], "offline-sandbox");
    assert_eq!(described["data"]["protocol"], 1);
    h.api.assert_ok();
}

fn post(url: &str, headers: &str, body: &[u8]) -> u16 {
    let mut s = std::net::TcpStream::connect(url.trim_start_matches("http://")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "POST /rpc HTTP/1.1\r\nHost: x\r\n{headers}\r\n").unwrap();
    s.write_all(body).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.split_whitespace().nth(1).unwrap_or("0").parse().unwrap()
}

#[test]
fn bridge_rejects_missing_auth_and_oversized_requests() {
    let h = Hybrid::new("bridge-auth");
    let bridge = h.bridge();
    assert_eq!(post(&bridge.url, "Content-Length: 2\r\n", b"{}"), 403);
    let wrong = format!("Authorization: Bearer {}x\r\nContent-Length: 2\r\n", bridge.token);
    assert_eq!(post(&bridge.url, &wrong, b"{}"), 403);
    let big = format!("Authorization: Bearer {}\r\nContent-Length: {}\r\n", bridge.token, 1024 * 1024 + 1);
    assert_eq!(post(&bridge.url, &big, b"{}"), 413);
    let empty = format!("Authorization: Bearer {}\r\nContent-Length: 0\r\n", bridge.token);
    assert_eq!(post(&bridge.url, &empty, b""), 413);
    assert!(bridge.url.starts_with("http://127.0.0.1:"));
    assert_eq!(bridge.token.len(), 64);
    drop(bridge);
    assert!(h.machine().calls.lock().unwrap().is_empty());
    assert!(!h.api.requests().iter().any(|r| r.target.contains("/process/")));
}

#[test]
fn transport_failure_never_falls_back_or_logs_secrets() {
    let h = Hybrid::new("transport-failure");
    h.machine().fail.store(true, Ordering::SeqCst);
    let bridge = h.bridge();
    let mut c = h.rusty();
    c.args(["--tools", "daytona", "--memory", "off", "probe"]).envs(bridge.env());
    let out = run(c, Duration::from_secs(20));
    assert!(!out.status.success());
    let all = text(&out);
    assert!(all.contains("no local fallback"), "{all}");
    assert!(!all.contains("credential-canary"), "{all}");
    assert_eq!(std::fs::read_to_string(h.host.join("source.py")).unwrap(), "HOST-CANARY");
}

#[test]
fn missing_bridge_and_invalid_location_fail_before_model_calls() {
    let h = Hybrid::new("bad-location");
    for args in [["--tools", "daytona", "probe"], ["--tools", "typo", "probe"]] {
        let mut c = h.rusty();
        c.args(args);
        let out = run(c, Duration::from_secs(10));
        assert!(!out.status.success());
        if args[1] == "daytona" {
            assert!(text(&out).contains("rusty-cloud hybrid"), "{}", text(&out));
        }
    }
    for url in ["https://example.com", "http://127.0.0.1/path", "http://user:secret@127.0.0.1"] {
        let mut c = h.rusty();
        c.args(["--tools", "daytona", "probe"]).env("RUSTY_TOOL_BRIDGE_URL", url);
        let out = run(c, Duration::from_secs(10));
        assert!(text(&out).contains("localhost HTTP origin"), "{}", text(&out));
        assert!(!text(&out).contains("user:secret"));
    }
}

#[test]
fn tool_endpoint_flag_after_double_dash_remains_a_user_prompt() {
    let h = Hybrid::new("double-dash");
    let mut c = h.rusty();
    c.env_remove("NVIDIA_API_KEY").args(["--", "--tool-rpc"]).stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = c.stderr(Stdio::piped()).spawn().unwrap();
    // rusty may exit (no model key) before reading; that is fine.
    let _ = child.stdin.take().unwrap().write_all(br#"{"op":"describe"}"#);
    let out = wait(child, Duration::from_secs(10));
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("\"protocol\""));
}

#[test]
fn saved_tool_default_only_changes_future_sessions() {
    let h = Hybrid::new("saved-default");
    let repl = |args: &[&str], input: &str| {
        let mut c = h.rusty();
        let mut child =
            c.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        // rusty may exit before reading (the second run is meant to fail); that is fine.
        let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
        wait(child, Duration::from_secs(10))
    };
    let out = repl(&["--tools", "local"], "/tools daytona\n/settings\n/exit\n");
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("takes effect next session") && stdout.contains("current tools local"), "{stdout}");
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(h.home.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["tools"], "daytona");
    assert!(!repl(&[], "/exit\n").status.success());
    assert!(repl(&["--tools", "local"], "/exit\n").status.success());
}

fn hybrid_args(h: &Hybrid) -> HybridArgs {
    HybridArgs {
        sandbox: "offline-sandbox".into(),
        workspace: h.remote.display().to_string(),
        local_binary: RUSTY.into(),
        goal: None,
        prompt: None,
        mode: "standard".into(),
        agents: "swarm".into(),
        swarm_max: 3,
        model: None,
        memory_mode: "off".into(),
        permissions: "auto".into(),
        stats: false,
        trajectory: None,
        resume: false,
        project_id: None,
    }
}

fn sigint_handler() -> usize {
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current);
        current.sa_sigaction
    }
}

#[test]
fn launcher_attaches_only_and_never_creates_wakes_or_deletes() {
    let h = Hybrid::new("attach");
    let args = hybrid_args(&h);
    let before = sigint_handler();
    let mut calls = Vec::new();
    let mut spawn = |command: &[String], env: &[(String, String)]| -> anyhow::Result<i32> {
        // Ctrl-C reaches the launcher while rusty runs; the bridge survives.
        // raise() delivers to this thread before it returns.
        unsafe { libc::raise(libc::SIGINT) };
        calls.push((command.to_vec(), env.to_vec()));
        Ok(0)
    };
    assert_eq!(hybrid::attach("started", h.transport.clone(), &args, &mut spawn).unwrap(), 0);
    let err = hybrid::attach("stopped", h.transport.clone(), &args, &mut spawn).unwrap_err();
    assert!(err.to_string().contains("never creates or wakes"), "{err}");
    assert_eq!(calls.len(), 1);
    let (command, env) = &calls[0];
    let joined = command.join(" ");
    assert!(joined.contains("--tools daytona") && joined.contains("--swarm-max 3"), "{joined}");
    assert!(env.iter().any(|(k, v)| k == "RUSTY_TOOL_BRIDGE_TOKEN" && v.len() == 64));
    assert_eq!(sigint_handler(), before, "the SIGINT handler was not restored");
    assert!(h.machine().calls.lock().unwrap().is_empty());
    assert!(!h.api.requests().iter().any(|r| r.method != "GET"), "attach changed the sandbox");
}

// ------------------------------------------------------- scripted model

#[derive(Default)]
struct ModelState {
    requests: usize,
    entered: usize,
    checker_requests: usize,
    problems: Vec<String>,
}

type Shared = Arc<(Mutex<ModelState>, Condvar)>;

/// Scripted lead, workers and checker. Panics are recorded, not swallowed.
fn model() -> (Server, Shared) {
    let shared: Shared = Arc::default();
    let s = shared.clone();
    let server = serve(Arc::new(move |req: Request| {
        if req.method == "GET" {
            return Response::json(401, json!({}));
        }
        let body = req.json();
        let (lock, cv) = &*s;
        lock.lock().unwrap().requests += 1;
        match step(&body, lock, cv) {
            Ok((steps, action)) => reply(
                steps,
                action.as_ref().map(|(n, a)| (n.as_str(), a.clone())),
                "Inspected and verified the remote workspace.",
            ),
            Err(problem) => {
                lock.lock().unwrap().problems.push(problem.clone());
                Response::json(500, json!({"error": problem}))
            }
        }
    }));
    (server, shared)
}

type Action = Option<(String, Value)>;

fn step(body: &Value, lock: &Mutex<ModelState>, cv: &Condvar) -> Result<(usize, Action), String> {
    let (prompt, steps, messages) = conversation(body);
    let last = messages.last().map(content).unwrap_or_default();
    let system = messages.first().map(content).unwrap_or_default();
    let ensure = |ok: bool, what: &str| if ok { Ok(()) } else { Err(format!("{what}: {last}")) };
    let act = |name: &str, args: Value| Some((name.to_string(), args));
    if system.contains("research worker") {
        ensure(!body["tools"].to_string().contains("\"write_file\""), "a worker was offered write_file")?;
        if prompt.starts_with("WORKER-") {
            let action = match steps {
                0 => {
                    let mut m = lock.lock().unwrap();
                    m.entered += 1;
                    cv.notify_all();
                    let (_m, t) = cv.wait_timeout_while(m, Duration::from_secs(5), |m| m.entered < 3).unwrap();
                    ensure(!t.timed_out(), "workers did not overlap")?;
                    act("write_file", json!({"path": "worker-forbidden.txt", "content": "forged"}))
                }
                1 => {
                    ensure(last.contains("denied"), "a worker write was not denied")?;
                    act("read_file", json!({"path": "source.py"}))
                }
                _ => {
                    ensure(last.contains("return a - b"), "the worker did not read the remote file")?;
                    None
                }
            };
            return Ok((steps, action));
        }
        // Careful's single checker reads the changed remote file.
        if steps == 0 {
            lock.lock().unwrap().checker_requests += 1;
            return Ok((steps, act("read_file", json!({"path": "source.py"}))));
        }
        ensure(last.contains("return a + b"), "the checker did not see the fix")?;
        return Ok((steps, None));
    }
    ensure(system.contains("REMOTE-INSTRUCTIONS"), "remote instructions are missing")?;
    ensure(!system.contains("HOST-INSTRUCTIONS"), "host instructions leaked")?;
    let infra = messages.iter().any(|m| m["role"] == "user" && content(m).starts_with("INFRA"));
    let plan: Vec<(&str, Value)> = if infra {
        if (steps == 1 || steps == 5) && messages.last().is_some_and(|m| m["role"] == "tool") {
            ensure(last.contains("blocked by careful mode"), "careful did not block")?;
        }
        if steps == 3 {
            ensure(last.contains("[verified:") && last.contains("pre-change snapshot"), "no verification")?;
        }
        vec![
            ("bash", json!({"command": "kubectl apply -f deployment.yaml"})),
            ("bash", json!({"command": "kubectl diff -f deployment.yaml"})),
            ("bash", json!({"command": "kubectl apply -f deployment.yaml"})),
            ("edit_file", json!({"path": "deployment.yaml", "old_string": "replicas: 1", "new_string": "replicas: 2"})),
            ("bash", json!({"command": "kubectl apply -f deployment.yaml"})),
        ]
    } else {
        vec![
            (
                "swarm",
                json!({"tasks": (0..3).map(|i| json!({"description": format!("inspect {i}"), "prompt": format!("WORKER-{i}")})).collect::<Vec<_>>()}),
            ),
            ("read_file", json!({"path": "source.py"})),
            ("outline", json!({"path": "source.py"})),
            ("search", json!({"pattern": "return", "path": "source.py"})),
            ("glob", json!({"pattern": "*.py"})),
            ("list_files", json!({"path": "."})),
            ("edit_file", json!({"path": "source.py", "old_string": "return a - b", "new_string": "return a + b"})),
            ("write_file", json!({"path": "note.txt", "content": "done"})),
            ("bash", json!({"command": "test \"$(cat note.txt)\" = done && printf checked > result.txt"})),
        ]
    };
    Ok((steps, plan.get(steps).map(|(n, a)| (n.to_string(), a.clone()))))
}

fn assert_model_ok(shared: &Shared) {
    let m = shared.0.lock().unwrap();
    assert!(m.problems.is_empty(), "scripted model: {:?}", m.problems);
}

#[test]
fn infra_gate_snapshot_target_and_verification_use_remote_environment() {
    let h = Hybrid::new("infra");
    let bindir = h.tmp.path().join("remote-bin");
    std::fs::create_dir_all(&bindir).unwrap();
    write_exec(
        &bindir.join("kubectl"),
        r#"#!/bin/bash
echo "$*" >> "$HOME/kube-calls"
case "$1" in
  config) printf 'dev-cloud|test' ;;
  diff) exit 1 ;;
  apply) printf applied ;;
  get) case "$*" in
    *'-o yaml'*) printf 'kind: Deployment\nmetadata:\n  name: api\n' ;;
    *) printf 'deployment.apps/api\n' ;;
  esac ;;
  rollout) printf 'deployment api successfully rolled out' ;;
esac
"#,
    );
    {
        let mut path = h.machine().path.lock().unwrap();
        *path = format!("{}:{path}", bindir.display());
    }
    // The sandbox has a cluster configured, so its kubectl is asked.
    let kube = h.tmp.path().join("remote-home/.kube");
    std::fs::create_dir_all(&kube).unwrap();
    std::fs::write(kube.join("config"), "").unwrap();
    std::fs::write(h.remote.join("deployment.yaml"), "replicas: 1\n").unwrap();
    std::fs::write(h.remote.join("source.py"), "def add(a,b):\n    return a + b\n").unwrap();
    let (server, shared) = model();
    let trace = h.tmp.path().join("infra.json");
    let bridge = h.bridge();
    let mut c = h.rusty();
    c.args(["--tools", "daytona", "--mode", "careful", "--memory", "off", "--permissions", "yolo", "--trajectory"])
        .arg(&trace)
        .arg("INFRA verify the gate")
        .envs(bridge.env())
        .env("RUSTY_BASE_URL", format!("{}/v1", server.url));
    let out = run(c, Duration::from_secs(60));
    drop(bridge);
    assert_model_ok(&shared);
    assert!(out.status.success(), "{}", text(&out));
    let calls = std::fs::read_to_string(h.tmp.path().join("remote-home/kube-calls")).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    assert_eq!(calls.iter().filter(|l| **l == "apply -f deployment.yaml").count(), 1, "{calls:?}");
    assert!(calls.iter().any(|l| l.starts_with("rollout status")));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dev-cloud/test") && stdout.contains("dry run is stale"), "{stdout}");
    let snapshots: Vec<PathBuf> = std::fs::read_dir(h.tmp.path().join("remote-home/.config/rusty/snapshots"))
        .unwrap()
        .map(|e| e.unwrap().path().join("rollback.yaml"))
        .filter(|p| p.is_file())
        .collect();
    assert_eq!(snapshots.len(), 1);
    assert!(std::fs::read_to_string(&snapshots[0]).unwrap().contains("kind: Deployment"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&snapshots[0]).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!h.remote.join(".rusty/snapshots").exists());
    assert!(!h.host.join("deployment.yaml").exists());
}

#[test]
fn three_modes_run_parallel_swarms_with_all_remote_tools_and_local_memory() {
    let mut h = Hybrid::new("modes");
    for (mode, memory) in [("standard", "off"), ("vibe", "off"), ("careful", "learn")] {
        std::fs::write(h.remote.join("source.py"), "def add(a,b):\n    return a - b\n").unwrap();
        let _ = std::fs::remove_file(h.remote.join("note.txt"));
        let _ = std::fs::remove_file(h.remote.join("result.txt"));
        h.machine().calls.lock().unwrap().clear();
        h.machine().peak.store(0, Ordering::SeqCst);
        let (server, shared) = model();
        let trajectory = h.tmp.path().join(format!("{mode}.json"));
        if memory == "learn" {
            let mut c = Command::new(MEMORYD);
            c.arg("--home").arg(&h.home).env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &h.home);
            h.daemon = Some(c.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            while !h.home.join("memory/advisor.sock").exists() {
                assert!(Instant::now() < deadline, "memory daemon did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let bridge = h.bridge();
        let mut c = h.rusty();
        c.args(["--tools", "daytona", "--mode", mode, "--agents", "swarm", "--swarm-max", "3", "--memory", memory])
            .args(["--stats", "--trajectory"])
            .arg(&trajectory)
            .arg("Inspect with three workers then fix the function and verify.")
            .envs(bridge.env())
            .env("RUSTY_BASE_URL", format!("{}/v1", server.url));
        let out = run(c, Duration::from_secs(90));
        drop(bridge);
        assert_model_ok(&shared);
        assert!(out.status.success(), "{mode}: {}", text(&out));
        let trace: Value = serde_json::from_str(&std::fs::read_to_string(&trajectory).unwrap()).unwrap();
        assert!(h.machine().peak.load(Ordering::SeqCst) >= 3, "{mode}: workers did not overlap remotely");
        assert_eq!(std::fs::read_to_string(h.remote.join("note.txt")).unwrap(), "done");
        assert_eq!(std::fs::read_to_string(h.remote.join("result.txt")).unwrap(), "checked");
        assert!(std::fs::read_to_string(h.remote.join("source.py")).unwrap().contains("return a + b"));
        assert_eq!(std::fs::read_to_string(h.host.join("source.py")).unwrap(), "HOST-CANARY");
        assert!(!h.host.join("note.txt").exists());
        assert!(!h.remote.join("worker-forbidden.txt").exists());
        let calls = h.machine().calls.lock().unwrap().clone();
        let tools: std::collections::BTreeSet<&str> =
            calls.iter().filter(|r| r["op"] == "execute").filter_map(|r| r["name"].as_str()).collect();
        for t in ["read_file", "outline", "search", "glob", "list_files", "edit_file", "write_file", "bash"] {
            assert!(tools.contains(t), "{mode}: {t} did not run remotely ({tools:?})");
        }
        assert!(!serde_json::to_string(&calls).unwrap().contains("offline-model-key"));
        assert_eq!(shared.0.lock().unwrap().checker_requests, usize::from(mode == "careful"), "{mode}");
        assert_eq!(trace["memory_mode"], memory);
        if memory == "learn" {
            assert!(trace["memory"]["hooks"].as_u64().unwrap() > 0);
            assert_eq!(trace["memory"]["timeouts"], 0);
        }
        assert!(!h.tmp.path().join("remote-home/.config/rusty/memory").exists());
    }
    h.api.assert_ok();
}

/// `rusty-cloud hybrid` end to end: the binary attaches, runs the local
/// rusty with the bridge, and the edit lands in the sandbox only.
#[test]
fn the_hybrid_command_runs_local_rusty_against_remote_tools() {
    let h = Hybrid::new("cli");
    let (server, shared) = model();
    let trace = h.tmp.path().join("cli.json");
    let out = h
        .api
        .cloud(&h.host)
        .env("HOME", &h.home)
        .env("RUSTY_HOME", &h.home)
        .env("RUSTY_NO_DOTENV", "1")
        .env("NVIDIA_API_KEY", "offline-model-key")
        .env("NO_COLOR", "1")
        .env("RUSTY_BASE_URL", format!("{}/v1", server.url))
        .args(["hybrid", "--sandbox", "offline-sandbox", "--workspace", h.remote.to_str().unwrap()])
        .args(["--local-binary", RUSTY, "--agents", "swarm", "--memory-mode", "off", "--stats", "--trajectory"])
        .arg(&trace)
        .args(["--prompt", "Inspect with three workers then fix the function and verify."])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_model_ok(&shared);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("never creates, stops or deletes it"));
    assert_eq!(std::fs::read_to_string(h.remote.join("result.txt")).unwrap(), "checked");
    assert!(!h.host.join("note.txt").exists());
    let trace: Value = serde_json::from_str(&std::fs::read_to_string(&trace).unwrap()).unwrap();
    assert!(
        trace["tools_location"].as_str().unwrap().starts_with("daytona · offline-sandbox"),
        "{}",
        trace["tools_location"]
    );
    assert!(!h.api.requests().iter().any(|r| r.method == "DELETE" || r.target.contains("/start")));
    h.api.assert_ok();
}
